# moltis Helm chart

Runs one isolated Moltis agent server per person. A single release manages a
whole household or team: every entry under `instances` gets its own
Deployment, Service, PVC, seeded `moltis.toml`, password and (optionally)
IngressRoute and NetworkPolicy, on top of shared `defaults`.

```yaml
defaults:
  timezone: America/New_York
  llm:                       # any OpenAI-compatible endpoint (llama.cpp, vLLM, …)
    name: muse               # → provider "custom-muse"
    baseUrl: http://192.168.1.204:8000/v1
    models: [Muse-Glimmer-30B]
    contextWindows:
      Muse-Glimmer-30B: 131072
  persistence:
    storageClass: nfs-storage

instances:
  jacob:
    displayName: Jacob
    terminal: true           # web UI shell (inside this instance's pod)
    password:
      existingSecret: moltis-jacob-password
  sam:
    displayName: Sam
    password:
      existingSecret: moltis-sam-password
    resources:
      limits:
        memory: 2Gi
```

```bash
kubectl -n moltis create secret generic moltis-jacob-password \
  --from-literal=password="$(openssl rand -base64 18)"
helm upgrade --install moltis charts/moltis -n moltis -f my-values.yaml
kubectl -n moltis port-forward svc/moltis-jacob 13131:13131   # http://localhost:13131
```

## How it maps onto Moltis

| Concern | Chart behaviour |
|---|---|
| Auth | `MOLTIS_PASSWORD` from the instance Secret becomes the initial password (≥ 12 chars) the first time the instance starts; change it or add a passkey in Settings afterwards. |
| TLS | Moltis runs with `--no-tls` and `MOLTIS_BEHIND_PROXY=true`; terminate TLS at your ingress. |
| Config | `moltis.toml` is rendered per instance but only **seeds** an empty volume, because Moltis edits it from the UI. `configOverwrite: true` replaces it on every start. |
| LLM | `defaults.llm` renders a `custom-<name>` OpenAI-compatible provider; `exclusive: true` hides every other provider. |
| Sandbox | Kubernetes has no Docker socket to share, so `sandboxBackend: restricted-host` runs agent commands inside the instance's own pod (env cleared, rlimits). The pod is the isolation boundary: it runs as uid 1000 with no privilege escalation (the image's passwordless sudo is inert), all capabilities dropped and RuntimeDefault seccomp. Shell commands still need approval in the UI. |
| Storage | One PVC per instance with `config/`, `data/` and `npm/` subPaths. Moltis uses SQLite, so the Deployment uses `Recreate` and never runs two pods on a volume. PVCs are kept on uninstall (`persistence.retain`). |
| Removing someone | `enabled: false` scales the instance to zero and keeps its data. |

## Web search and browsing (`webTools`)

```yaml
defaults:
  webTools:
    searxng: { enabled: true }      # meta search + MCP (mcp__searxng__*)
    playwright: { enabled: true }   # headless Chromium MCP (mcp__playwright__*)
```

Both run as **native sidecars** in each instance's pod, bound to the pod's
loopback (`127.0.0.1:8080` SearXNG, `:3010` its MCP, `:8931` Playwright), so
nothing new is exposed. Kubernetes starts them before Moltis and waits for
their startup probes, so the MCP connections never race the servers.

- SearXNG uses `files/searxng-settings.yml`: JSON API on, limiter off, a broad
  engine mix (Google, Bing, DuckDuckGo, Brave, Startpage, Mojeek, Qwant,
  Wikipedia, GitHub, Stack Overflow, Reddit) plus image and news engines.
  Engines fail independently, so the mix is what keeps results flowing.
  Override with `webTools.searxng.settings`.
- Enabling either adds its MCP server to the instance (merged into an existing
  list, never removing UI-added servers), denies Moltis's built-in
  `web_search` / `browser` tools, and appends search → read → browse guidance
  to `TOOLS.md` once.
- Image results are given as links: Moltis's CSP doesn't load external images
  inline (which also blocks image-URL data exfiltration).
- Resources: SearXNG ~200 MiB, its MCP ~64 MiB, Playwright 0.4–2 GiB while
  browsing; per instance.

See `values.yaml` for every option.
