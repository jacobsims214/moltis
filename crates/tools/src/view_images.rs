//! `view_images`: fetch a few image URLs, downscale them and hand them back so
//! a vision-capable model can actually look at them — e.g. to confirm that
//! image-search results really show "a green 1969 C10" before answering.
//!
//! The images are returned as `data:image/jpeg;base64,…` URIs; the agent
//! runner lifts them out of the tool result into a follow-up multimodal
//! message for vision models. Every fetch (and each redirect hop) passes the
//! SSRF check, bodies are capped, and images are shrunk to fit
//! `MAX_EDGE` so a set of six costs a few thousand tokens, not tens.

use std::{io::Cursor, time::Duration};

use {async_trait::async_trait, base64::Engine as _, url::Url};

use {
    crate::{
        error::{Error, Result},
        ssrf::ssrf_check,
    },
    moltis_agents::tool_registry::AgentTool,
};

/// Most images one call will fetch.
const MAX_IMAGES: usize = 6;
/// Largest image body accepted, before decoding.
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Longest edge after downscaling.
const MAX_EDGE: u32 = 768;
const JPEG_QUALITY: u8 = 80;
const MAX_REDIRECTS: u8 = 3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct ViewImagesTool {
    ssrf_allowlist: Vec<ipnet::IpNet>,
}

impl ViewImagesTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Same SSRF allowlist as `[tools.web.fetch]`.
    #[must_use]
    pub fn from_fetch_config(config: &moltis_config::schema::WebFetchConfig) -> Self {
        Self::new().with_ssrf_allowlist(
            config
                .ssrf_allowlist
                .iter()
                .filter_map(|s| s.parse::<ipnet::IpNet>().ok())
                .collect(),
        )
    }

    /// Private ranges that may still be fetched (mirrors web_fetch's policy).
    #[must_use]
    pub fn with_ssrf_allowlist(mut self, allowlist: Vec<ipnet::IpNet>) -> Self {
        self.ssrf_allowlist = allowlist;
        self
    }

    async fn fetch(&self, client: &reqwest::Client, raw: &str) -> Result<Vec<u8>> {
        let mut url = Url::parse(raw).map_err(|e| Error::message(format!("invalid URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::message("only http(s) image URLs are supported"));
        }
        for _ in 0..=MAX_REDIRECTS {
            ssrf_check(&url, &self.ssrf_allowlist).await?;
            let mut resp = client
                .get(url.as_str())
                .header(reqwest::header::ACCEPT, "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8")
                .send()
                .await?;
            if resp.status().is_redirection() {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Error::message("redirect without a Location header"))?;
                url = url
                    .join(location)
                    .map_err(|e| Error::message(format!("bad redirect target: {e}")))?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(Error::message(format!("HTTP {}", resp.status())));
            }
            if let Some(len) = resp.content_length()
                && len as usize > MAX_BYTES
            {
                return Err(Error::message(format!("image too large ({len} bytes)")));
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_BYTES {
                    return Err(Error::message("image too large"));
                }
            }
            return Ok(body);
        }
        Err(Error::message("too many redirects"))
    }

    async fn view_one(&self, client: &reqwest::Client, raw: &str) -> Result<(String, u32, u32)> {
        let bytes = self.fetch(client, raw).await?;
        let img = image::load_from_memory(&bytes)
            .map_err(|e| Error::message(format!("not a decodable image: {e}")))?;
        let (width, height) = (img.width(), img.height());
        let small = if width > MAX_EDGE || height > MAX_EDGE {
            img.thumbnail(MAX_EDGE, MAX_EDGE)
        } else {
            img
        };
        let mut out = Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        small
            .to_rgb8()
            .write_with_encoder(encoder)
            .map_err(|e| Error::message(format!("could not re-encode image: {e}")))?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(out.into_inner());
        Ok((format!("data:image/jpeg;base64,{b64}"), width, height))
    }
}

/// Accept `urls: [..]` or a single `url`, de-duplicated, at most `MAX_IMAGES`.
fn requested_urls(params: &serde_json::Value) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    if let Some(list) = params.get("urls").and_then(|v| v.as_array()) {
        urls.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    if let Some(one) = params.get("url").and_then(|v| v.as_str()) {
        urls.push(one.to_string());
    }
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| !u.trim().is_empty() && seen.insert(u.trim().to_string()));
    urls.truncate(MAX_IMAGES);
    urls
}

#[async_trait]
impl AgentTool for ViewImagesTool {
    fn name(&self) -> &str {
        "view_images"
    }

    fn description(&self) -> &str {
        "Look at images. Fetches up to 6 image URLs (e.g. img_src values from an image search) and shows them to you, in order, so you can check what each one actually depicts — colour, model, year, whether it matches the request — before using it. Use after narrowing candidates by text; drop images that don't match."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": MAX_IMAGES,
                    "description": "Direct image URLs to look at (up to 6)"
                }
            },
            "required": ["urls"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let urls = requested_urls(&params);
        if urls.is_empty() {
            anyhow::bail!("Provide `urls`: a list of up to {MAX_IMAGES} image URLs");
        }
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) moltis-view-images")
            .build()?;

        let results = futures::future::join_all(urls.iter().map(|u| self.view_one(&client, u))).await;

        let mut images = Vec::new();
        let mut failed = Vec::new();
        for (url, res) in urls.iter().zip(results) {
            match res {
                Ok((data_uri, width, height)) => images.push(serde_json::json!({
                    "image_number": images.len() + 1,
                    "url": url,
                    "original_size": format!("{width}x{height}"),
                    "image": data_uri,
                })),
                Err(e) => failed.push(serde_json::json!({ "url": url, "error": e.to_string() })),
            }
        }
        Ok(serde_json::json!({
            "shown": images.len(),
            "images": images,
            "failed": failed,
            "note": "The images follow in order (Image 1 = image_number 1). Say what each actually shows and only use the ones that match.",
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use {super::*, image::ImageFormat};

    #[test]
    fn requested_urls_dedupes_and_caps() {
        let p = serde_json::json!({
            "urls": ["https://a/1.jpg", "https://a/1.jpg", "", "https://a/2.jpg",
                     "https://a/3.jpg", "https://a/4.jpg", "https://a/5.jpg", "https://a/6.jpg", "https://a/7.jpg"],
        });
        let urls = requested_urls(&p);
        assert_eq!(urls.len(), MAX_IMAGES);
        assert_eq!(urls[0], "https://a/1.jpg");
        assert_eq!(urls[1], "https://a/2.jpg");
    }

    #[tokio::test]
    async fn rejects_private_and_non_http_urls() {
        let tool = ViewImagesTool::new();
        let out = tool
            .execute(serde_json::json!({ "urls": ["http://127.0.0.1/x.png", "file:///etc/passwd"] }))
            .await
            .unwrap();
        assert_eq!(out["shown"], 0);
        assert_eq!(out["failed"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fetches_and_downscales_an_image() {
        // A 2000x1000 PNG served by a local mock, allowed via the SSRF allowlist.
        let mut png = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(2000, 1000, image::Rgb([20, 160, 60]))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/truck.png")
            .with_status(200)
            .with_header("content-type", "image/png")
            .with_body(png.into_inner())
            .create_async()
            .await;
        let tool = ViewImagesTool::new()
            .with_ssrf_allowlist(vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()]);
        let out = tool
            .execute(serde_json::json!({ "urls": [format!("{}/truck.png", server.url())] }))
            .await
            .unwrap();
        assert_eq!(out["shown"], 1, "{out}");
        assert_eq!(out["images"][0]["original_size"], "2000x1000");
        let data = out["images"][0]["image"].as_str().unwrap();
        let b64 = data.strip_prefix("data:image/jpeg;base64,").unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let small = image::load_from_memory(&bytes).unwrap();
        assert_eq!((small.width(), small.height()), (768, 384));
    }
}
