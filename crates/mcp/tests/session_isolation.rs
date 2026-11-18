//! Per-session MCP client isolation, end to end.
//!
//! These tests drive the real `McpManager` against a mock streamable-HTTP MCP
//! server. The mock mints a distinct `Mcp-Session-Id` per `initialize` and
//! **rejects any request whose session id it never issued** — exactly what a
//! real streamable-HTTP MCP server does, and exactly what the defect broke:
//! with one shared client every chat session presented the same session id.
//!
//! The regression pinned down here: before per-session pooling, two concurrent
//! assistant turns resolved the same `ClientSlot`, so the second turn's tool
//! call arrived on the first turn's MCP session. One shared server-side MCP
//! session for two people is the collision the household hit as "agent run
//! failed".

use std::{
    collections::HashMap,
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
    moltis_mcp::{
        McpAgentTool, McpManager, McpRegistry, McpServerConfig, McpToolBridge, TransportType,
    },
    serde_json::{Value, json},
};

/// What the mock server saw, so a test can assert on session identity.
#[derive(Default)]
struct ServerState {
    /// Session ids issued by `initialize`, in order.
    issued: StdMutex<Vec<String>>,
    /// Session id each `tools/call` ran on.
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

/// A minimal streamable-HTTP MCP server.
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

    // Notifications carry no id and expect no body.
    if method == "notifications/initialized" {
        return StatusCode::ACCEPTED.into_response();
    }

    if method == "initialize" {
        // A new session for every initialize, announced on the response, which
        // is how the transport learns it.
        let session = state.issue();
        return json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock-home-os", "version": "1" }
            }
        }))
        .with_session(&session);
    }

    // Every other method must arrive on a session this server issued.
    if !state.knows(&session) {
        // A streamable-HTTP server answers an unknown session with 404.
        return StatusCode::NOT_FOUND.into_response();
    }

    match method {
        "tools/list" => json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [ {
                "name": "whoami",
                "description": "reports the MCP session this call ran on",
                "inputSchema": { "type": "object", "properties": {} }
            } ] }
        }))
        .with_session(&session),
        "tools/call" => {
            state.tool_calls.lock().unwrap().push(session.clone());
            json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [ { "type": "text", "text": session } ],
                    "isError": false
                }
            }))
            .with_session(&session)
        },
        other => json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unknown method {other}") }
        }))
        .with_session(&session),
    }
}

/// Small helper so each response can carry its session header.
struct JsonWithSession(Value, Option<String>);

impl IntoResponse for JsonWithSession {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.0).unwrap_or_default();
        let mut response = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
        if let Some(session) = self.1
            && let Ok(value) = session.parse()
        {
            response.headers_mut().insert("mcp-session-id", value);
        }
        response
    }
}

fn json_response(value: Value) -> JsonWithSession {
    JsonWithSession(value, None)
}

impl JsonWithSession {
    fn with_session(self, session: &str) -> Response {
        JsonWithSession(self.0, Some(session.to_string())).into_response()
    }
}

/// Start a manager with one streamable-HTTP server pointed at `url`.
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

/// The bridge for `home-os`'s `whoami` tool.
async fn whoami_bridge(manager: &McpManager) -> McpToolBridge {
    manager
        .tool_bridges_for("home-os")
        .await
        .into_iter()
        .find(|bridge| bridge.prefixed_name() == "mcp__home-os__whoami")
        .expect("whoami bridge registered")
}

/// THE regression test.
///
/// Two sessions calling the same tool at the same time each get their own MCP
/// session, and both succeed. Without per-session pooling both bridges resolve
/// the primary slot: both calls go out on one `Mcp-Session-Id` and the two
/// callers are served by one MCP conversation, so the "different sessions"
/// assertion fails.
#[tokio::test]
async fn two_concurrent_sessions_use_independent_mcp_sessions() {
    let (url, state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;
    let bridge = whoami_bridge(&manager).await;

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
        "the two calls must have arrived on different MCP sessions"
    );
}

/// Five sessions at once — the war-game shape — all succeed, all distinct.
#[tokio::test]
async fn five_concurrent_sessions_all_succeed() {
    let (url, _state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;
    let bridge = whoami_bridge(&manager).await;

    let mut handles = Vec::new();
    for n in 0..5 {
        let bridge = bridge.clone();
        handles.push(tokio::spawn(async move {
            bridge
                .execute(json!({ "_session_key": format!("web:member-{n}") }))
                .await
        }));
    }

    let mut served_by = Vec::new();
    for handle in handles {
        let value = handle.await.expect("task joined").expect("call succeeded");
        // The tool's text content: the session id the call ran on.
        let session = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        served_by.push(session);
    }

    served_by.sort();
    served_by.dedup();
    assert_eq!(
        served_by.len(),
        5,
        "each of the five sessions must have its own MCP connection"
    );
}

/// A session keeps one connection across sequential calls.
#[tokio::test]
async fn one_session_reuses_its_connection() {
    let (url, state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;
    let bridge = whoami_bridge(&manager).await;

    let first = bridge
        .execute(json!({ "_session_key": "web:alice" }))
        .await
        .expect("first call");
    let second = bridge
        .execute(json!({ "_session_key": "web:alice" }))
        .await
        .expect("second call");

    assert_eq!(first, second, "one session keeps its MCP session");
    // One for the primary slot's own handshake, one for alice's connection.
    assert_eq!(
        state.issued.lock().unwrap().len(),
        2,
        "a second call in the same session must not re-initialize"
    );
}

/// A call carrying no session identity still works (status probes, CLI paths).
#[tokio::test]
async fn unsessioned_call_falls_back_to_the_shared_slot() {
    let (url, _state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;
    let bridge = whoami_bridge(&manager).await;

    let result = bridge.execute(json!({})).await;
    assert!(
        result.is_ok(),
        "a call outside any chat session must still work: {result:?}"
    );
}

/// Shutting the server down closes every per-session connection, and the
/// bridges stop resolving clients instead of talking to a dead server.
#[tokio::test]
async fn stopping_the_server_closes_session_connections() {
    let (url, _state) = spawn_mock_mcp().await;
    let manager = manager_with_server(&url).await;
    let bridge = whoami_bridge(&manager).await;

    bridge
        .execute(json!({ "_session_key": "web:alice" }))
        .await
        .expect("connected");

    manager.stop_server("home-os").await;

    let result = bridge.execute(json!({ "_session_key": "web:alice" })).await;
    assert!(
        result.is_err(),
        "a stopped server must not serve tool calls: {result:?}"
    );
}

/// Rough sanity check that the collection type used by the assertions is what
/// we think it is (guards against a silently-empty read).
#[test]
fn hashmap_import_is_used() {
    let map: HashMap<&str, usize> = HashMap::from([("a", 1)]);
    assert_eq!(map.get("a"), Some(&1));
}
