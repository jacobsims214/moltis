//! Regression test for HOME-OS #5315, written against **pre-fix** API only.
//!
//! It uses `McpManager::tool_bridges()` and `McpMcpToolBridge::execute`, which
//! exist both before and after the fix, so the same file compiles against the
//! original code and fails there. That is the point: this is the "fails without
//! the fix" proof.
//!
//! Two concurrent assistant turns on two different sessions must be served by
//! two different server-side MCP sessions. Before per-session pooling both
//! turns resolve the one shared `ClientSlot`, so both calls carry the same
//! `Mcp-Session-Id` and the mock server answers both from one MCP session.

use std::{
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use {
    axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse, Response},
        routing::post,
    },
    moltis_mcp::{McpAgentTool, McpManager, McpRegistry, McpServerConfig, TransportType},
    serde_json::{Value, json},
};

#[derive(Default)]
struct ServerState {
    issued: StdMutex<Vec<String>>,
    tool_calls: StdMutex<Vec<String>>,
}

impl ServerState {
    fn issue(&self) -> String {
        let mut issued = self.issued.lock().unwrap();
        let session = format!("sess-{}", issued.len());
        issued.push(session.clone());
        session
    }

    fn knows(&self, session: &str) -> bool {
        self.issued.lock().unwrap().iter().any(|s| s == session)
    }
}

async fn spawn_mock_mcp() -> (String, Arc<ServerState>) {
    let state = Arc::new(ServerState::default());
    let app = Router::new()
        .route("/mcp", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/mcp"), state)
}

fn json_response(value: Value, session: Option<&str>) -> Response {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response();
    if let Some(session) = session
        && let Ok(value) = session.parse()
    {
        response.headers_mut().insert("mcp-session-id", value);
    }
    response
}

async fn handle(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Ok(request): Result<Value, _> = serde_json::from_str(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    if method == "notifications/initialized" {
        return StatusCode::ACCEPTED.into_response();
    }

    if method == "initialize" {
        let session = state.issue();
        return json_response(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock-home-os", "version": "1" }
                }
            }),
            Some(&session),
        );
    }

    if !state.knows(&session) {
        return StatusCode::NOT_FOUND.into_response();
    }

    match method {
        "tools/list" => json_response(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": [ {
                    "name": "whoami",
                    "description": "reports the MCP session this call ran on",
                    "inputSchema": { "type": "object", "properties": {} }
                } ] }
            }),
            Some(&session),
        ),
        "tools/call" => {
            state.tool_calls.lock().unwrap().push(session.clone());
            json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [ { "type": "text", "text": session } ],
                        "isError": false
                    }
                }),
                Some(&session),
            )
        },
        other => json_response(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method {other}") }
            }),
            Some(&session),
        ),
    }
}

async fn manager_with_server(url: &str) -> Arc<McpManager> {
    let mut registry = McpRegistry::new();
    registry
        .servers
        .insert("home-os".to_string(), McpServerConfig {
            transport: TransportType::StreamableHttp,
            url: Some(secrecy::Secret::new(url.to_string())),
            ..Default::default()
        });
    let manager = Arc::new(McpManager::new_with_request_timeout(
        registry,
        Duration::from_secs(5),
    ));
    manager.start_enabled().await;
    manager
}

/// Two concurrent sessions, two independent MCP sessions, both succeed.
#[tokio::test]
async fn two_concurrent_sessions_use_independent_mcp_sessions() {
    let (url, state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;

    let bridges = manager.tool_bridges().await;
    let bridge = bridges
        .iter()
        .find(|bridge| bridge.prefixed_name() == "mcp__home-os__whoami")
        .expect("whoami bridge registered");

    let (alice, bob) = tokio::join!(
        bridge.execute(json!({ "_session_key": "web:alice" })),
        bridge.execute(json!({ "_session_key": "web:bob" })),
    );

    let alice = alice.expect("alice's tool call succeeded");
    let bob = bob.expect("bob's tool call succeeded");

    assert_ne!(
        alice, bob,
        "two chat sessions must not be served by one MCP session"
    );

    let calls = state.tool_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "both tool calls reached the server");
    assert_ne!(
        calls[0], calls[1],
        "the two calls must arrive on different MCP sessions"
    );
}
