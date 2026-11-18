//! Bridge MCP tools to the `AgentTool` trait so they can be used in the agent loop.

use std::sync::Arc;

use async_trait::async_trait;

use {
    crate::{
        client_slot::ClientSlot,
        error::{Error, Result},
        manager::McpManager,
        session_pool::UNSCOPED_SESSION,
        traits::McpClientTrait,
        types::{McpToolDef, ToolContent},
    },
    moltis_config::schema::McpServerId,
    tokio::sync::RwLock,
};

/// The tool-context key the agent runner merges into every tool call.
pub(crate) const SESSION_KEY_ARG: &str = "_session_key";

/// Recursively strip null values from nested objects and arrays.
///
/// Top-level null stripping is handled by the caller; this recurses into
/// values that are themselves objects or arrays.
fn strip_nulls_recursive(map: &mut serde_json::Map<String, serde_json::Value>) {
    for value in map.values_mut() {
        match value {
            serde_json::Value::Object(inner) => {
                inner.retain(|_, v| !v.is_null());
                strip_nulls_recursive(inner);
            },
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    if let serde_json::Value::Object(inner) = item {
                        inner.retain(|_, v| !v.is_null());
                        strip_nulls_recursive(inner);
                    }
                }
            },
            _ => {},
        }
    }
}

/// Drop optional top-level arguments a model filled with a placeholder instead
/// of leaving them out: empty / "null" / "None" strings, or a value outside the
/// property's `enum`. Some models (Qwen, notably) send every schema property on
/// every call — `"engines": ""`, `"time_range": "\nnull"` — and strict servers
/// reject the whole call, which then repeats until the loop detector fires.
/// Required properties are never touched, so real validation errors still
/// reach the model.
fn drop_placeholder_args(
    map: &mut serde_json::Map<String, serde_json::Value>,
    schema: &serde_json::Value,
) {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    map.retain(|key, value| {
        if required.contains(&key.as_str()) {
            return true;
        }
        if let Some(s) = value.as_str()
            && matches!(
                s.trim(),
                "" | "null" | "None" | "none" | "undefined" | "nil"
            )
        {
            return false;
        }
        match props
            .get(key)
            .and_then(|p| p.get("enum"))
            .and_then(|e| e.as_array())
        {
            Some(allowed) => allowed.contains(value),
            None => true,
        }
    });
}

/// An `AgentTool` implementation that delegates to an MCP server via `McpClient`.
///
/// Cheap to clone: every field is a handle or a small owned value, so a test or
/// a caller can hold one bridge per concurrent turn.
#[derive(Clone)]
pub struct McpToolBridge {
    /// Prefixed tool name: `mcp__<server>__<tool>`.
    prefixed_name: String,
    /// Original tool name on the MCP server.
    original_name: String,
    /// Name of the MCP server this tool belongs to.
    server_name: McpServerId,
    description: String,
    input_schema: serde_json::Value,
    client_slot: Arc<ClientSlot>,
    /// The manager and the server this bridge belongs to, when the bridge was
    /// built for a live server. With them, `execute` resolves the calling
    /// session's own connection — and picks up a restart's new connections,
    /// and a stop's removal — instead of pinning whatever existed when the
    /// `ToolRegistry` was last synced.
    live: Option<LiveServer>,
}

/// Where a bridge looks up the connection for a session, freshly, per call.
#[derive(Clone)]
pub(crate) struct LiveServer {
    pub(crate) manager: McpManager,
    pub(crate) server: String,
}

impl McpToolBridge {
    /// Create a bridge for a single MCP tool.
    pub fn new(
        server_name: &str,
        tool_def: &McpToolDef,
        client: Arc<RwLock<dyn McpClientTrait>>,
    ) -> Self {
        Self::from_slot_tool(
            server_name,
            tool_def,
            Arc::new(ClientSlot::with_client(client)),
        )
    }

    fn from_slot_tool(
        server_name: &str,
        tool_def: &McpToolDef,
        client_slot: Arc<ClientSlot>,
    ) -> Self {
        Self::from_slot_tool_with_pool(server_name, tool_def, client_slot, None)
    }

    fn from_slot_tool_with_pool(
        server_name: &str,
        tool_def: &McpToolDef,
        client_slot: Arc<ClientSlot>,
        live: Option<LiveServer>,
    ) -> Self {
        Self {
            prefixed_name: format!("mcp__{}__{}", server_name, tool_def.name),
            original_name: tool_def.name.clone(),
            server_name: McpServerId::from(server_name),
            description: tool_def
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool: {}", tool_def.name)),
            input_schema: tool_def.input_schema.clone(),
            client_slot,
            live,
        }
    }

    /// Create bridges backed by the provided client.
    pub fn from_client(
        server_name: &str,
        tools: &[McpToolDef],
        client: Arc<RwLock<dyn McpClientTrait>>,
    ) -> Vec<Self> {
        Self::from_slot(
            server_name,
            tools,
            Arc::new(ClientSlot::with_client(client)),
        )
    }

    pub(crate) fn from_slot(
        server_name: &str,
        tools: &[McpToolDef],
        client_slot: Arc<ClientSlot>,
    ) -> Vec<Self> {
        Self::from_slot_with_pool(server_name, tools, client_slot, None)
    }

    /// Bridges that resolve a per-session client on every call.
    ///
    /// `client_slot` stays as the fallback for calls made outside any chat
    /// session (status polling, MCP self-tests, the CLI), which have no
    /// session identity and no concurrent sibling.
    pub(crate) fn from_slot_with_pool(
        server_name: &str,
        tools: &[McpToolDef],
        client_slot: Arc<ClientSlot>,
        live: Option<LiveServer>,
    ) -> Vec<Self> {
        tools
            .iter()
            .map(|tool| {
                Self::from_slot_tool_with_pool(
                    server_name,
                    tool,
                    Arc::clone(&client_slot),
                    live.clone(),
                )
            })
            .collect()
    }

    pub fn prefixed_name(&self) -> &str {
        &self.prefixed_name
    }

    /// The MCP server this tool belongs to.
    pub fn server_name(&self) -> &McpServerId {
        &self.server_name
    }
}

/// Trait for agent-callable tools, matching `AgentTool` in moltis-agents.
///
/// We define our own copy here to avoid a circular dependency on moltis-agents.
/// The gateway wires `McpToolBridge` into the `ToolRegistry` via a thin adapter.
#[async_trait]
pub trait McpAgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value>;
}

#[async_trait]
impl McpAgentTool for McpToolBridge {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        // Read the calling session before the internal metadata keys are
        // stripped below: the runner injects `_session_key` into every tool
        // call, and it is the only session identity that reaches this layer.
        let session_key = params
            .get(SESSION_KEY_ARG)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(UNSCOPED_SESSION)
            .to_string();

        // Clean up arguments before forwarding to the MCP server:
        // 1. Strip internal metadata keys (e.g. _session_key) injected by the
        //    agent runner — these break servers with strict validation.
        // 2. Strip null values — these arise from strict-mode schema patching
        //    that makes optional properties nullable.  MCP servers expect
        //    optional fields to be absent, not null.
        // 3. Drop optional arguments filled with placeholders (see
        //    `drop_placeholder_args`).
        let params = match params {
            serde_json::Value::Object(mut map) => {
                map.retain(|k, v| !k.starts_with('_') && !v.is_null());
                strip_nulls_recursive(&mut map);
                drop_placeholder_args(&mut map, &self.input_schema);
                serde_json::Value::Object(map)
            },
            other => other,
        };

        // Resolve the client for every call. With a session pool the calling
        // session gets its own connection; without one (and for calls made
        // outside any session) the process-wide slot is used. Restarts replace
        // whichever slot's client instance, while agent turns may retain this
        // bridge for the lifetime of the turn.
        let slot = match self.live.as_ref() {
            Some(live) => {
                // Resolve from the manager on every call: a restart installs new
                // connections and a stop removes them, and this bridge outlives
                // both (the `ToolRegistry` keeps it until MCP tools are resynced).
                live.manager
                    .client_slot_for_session(&live.server, &session_key)
                    .await
                    .ok_or_else(|| {
                        Error::message(format!("MCP server '{}' is not running", self.server_name))
                    })?
            },
            None => Arc::clone(&self.client_slot),
        };
        let client = slot.read_client().await.ok_or_else(|| {
            Error::message(format!("MCP server '{}' is not running", self.server_name))
        })?;
        let result = client.call_tool(&self.original_name, params).await?;

        if result.is_error {
            let text = result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::message(format!("MCP tool error: {text}")));
        }

        // Flatten text content into a single JSON value.
        let texts: Vec<&str> = result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Image content (e.g. a browser screenshot) travels as data URIs so the
        // agent runner can show it to vision models; text-only results keep
        // their previous shape.
        let images: Vec<String> = result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Image { data, mime_type } if mime_type.starts_with("image/") => {
                    Some(format!("data:{mime_type};base64,{data}"))
                },
                _ => None,
            })
            .collect();

        let value = if texts.len() == 1 {
            // Try to parse as JSON first.
            serde_json::from_str(texts[0])
                .unwrap_or_else(|_| serde_json::Value::String(texts[0].to_string()))
        } else {
            serde_json::json!({ "content": texts })
        };
        if images.is_empty() {
            Ok(value)
        } else {
            Ok(serde_json::json!({ "result": value, "images": images }))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn drop_placeholder_args_keeps_real_and_required_values() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "engines": { "type": "string" },
                "time_range": { "type": "string", "enum": ["day", "month", "year"] },
                "safesearch": { "type": "string", "enum": ["0", "1", "2"] },
                "pageno": { "type": "number" },
                "note": { "type": "string" }
            },
            "required": ["query", "note"]
        });
        let mut map = serde_json::json!({
            "query": "1969 c10",
            "engines": "",
            "time_range": "\nnull",
            "safesearch": "0",
            "pageno": 1,
            "note": "",
            "extra": "None"
        })
        .as_object()
        .cloned()
        .unwrap();
        drop_placeholder_args(&mut map, &schema);
        assert_eq!(
            serde_json::Value::Object(map),
            serde_json::json!({ "query": "1969 c10", "safesearch": "0", "pageno": 1, "note": "" })
        );

        let mut map = serde_json::json!({ "time_range": "week" })
            .as_object()
            .cloned()
            .unwrap();
        drop_placeholder_args(&mut map, &schema);
        assert!(map.is_empty(), "values outside the enum are dropped");
    }

    use {
        super::*,
        crate::{
            client::McpClientState,
            traits::McpClientTrait,
            types::{McpToolDef, ToolContent, ToolsCallResult},
        },
        std::sync::Arc,
        tokio::sync::RwLock,
    };

    /// Mock MCP client that records the arguments passed to `call_tool`.
    struct MockMcpClient {
        received_args: Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    }

    #[async_trait]
    impl McpClientTrait for MockMcpClient {
        fn server_name(&self) -> &str {
            "mock"
        }

        fn state(&self) -> McpClientState {
            McpClientState::Ready
        }

        fn tools(&self) -> &[McpToolDef] {
            &[]
        }

        async fn list_tools(&mut self) -> Result<&[McpToolDef]> {
            Ok(&[])
        }

        async fn call_tool(
            &self,
            _name: &str,
            arguments: serde_json::Value,
        ) -> Result<ToolsCallResult> {
            *self.received_args.lock().await = Some(arguments);
            Ok(ToolsCallResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_string(),
                }],
                is_error: false,
            })
        }

        async fn is_alive(&self) -> bool {
            true
        }

        async fn shutdown(&mut self) {}
    }

    #[test]
    fn test_prefixed_name_format() {
        let name = format!("mcp__{}__{}", "filesystem", "read_file");
        assert_eq!(name, "mcp__filesystem__read_file");
    }

    #[test]
    fn test_double_underscore_separator() {
        // Verify tool names use double underscore for unambiguous splitting.
        let name = "mcp__my-server__read_file";
        let parts: Vec<&str> = name.splitn(3, "__").collect();
        assert_eq!(parts, vec!["mcp", "my-server", "read_file"]);
    }

    #[test]
    fn test_from_client_shares_slot_between_bridges() {
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(MockMcpClient {
            received_args: Arc::new(tokio::sync::Mutex::new(None)),
        }));
        let tools = ["first", "second"].map(|name| McpToolDef {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        });

        let bridges = McpToolBridge::from_client("test", &tools, client);

        assert_eq!(bridges.len(), 2);
        assert!(Arc::ptr_eq(
            &bridges[0].client_slot,
            &bridges[1].client_slot
        ));
    }

    #[tokio::test]
    async fn test_execute_strips_internal_metadata() {
        let received = Arc::new(tokio::sync::Mutex::new(None));
        let client = MockMcpClient {
            received_args: Arc::clone(&received),
        };
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(client));

        let tool_def = McpToolDef {
            name: "read_file".to_string(),
            description: Some("Read a file".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let bridge = McpToolBridge::new("fs", &tool_def, client);

        let params = serde_json::json!({
            "path": "/tmp/test.txt",
            "_session_key": "abc123",
            "_accept_language": "en",
            "_conn_id": "conn-42",
            "_working_dir": "/workspace/project",
            "encoding": "utf-8"
        });

        let result = bridge.execute(params).await;
        assert!(result.is_ok());

        let forwarded = received.lock().await.take().expect("call_tool was called");
        let map = forwarded.as_object().expect("args should be an object");

        // Real parameters are forwarded.
        assert_eq!(
            map.get("path").and_then(|v| v.as_str()),
            Some("/tmp/test.txt")
        );
        assert_eq!(map.get("encoding").and_then(|v| v.as_str()), Some("utf-8"));

        // Internal metadata keys are stripped.
        assert!(!map.contains_key("_session_key"));
        assert!(!map.contains_key("_accept_language"));
        assert!(!map.contains_key("_conn_id"));
        assert!(!map.contains_key("_working_dir"));
    }

    #[tokio::test]
    async fn test_execute_passes_non_object_params_unchanged() {
        let received = Arc::new(tokio::sync::Mutex::new(None));
        let client = MockMcpClient {
            received_args: Arc::clone(&received),
        };
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(client));

        let tool_def = McpToolDef {
            name: "echo".to_string(),
            description: Some("Echo".to_string()),
            input_schema: serde_json::json!({"type": "string"}),
        };
        let bridge = McpToolBridge::new("test", &tool_def, client);

        let params = serde_json::json!("hello");
        let result = bridge.execute(params).await;
        assert!(result.is_ok());

        let forwarded = received.lock().await.take().expect("call_tool was called");
        assert_eq!(forwarded, serde_json::json!("hello"));
    }

    #[tokio::test]
    async fn test_execute_strips_null_values() {
        let received = Arc::new(tokio::sync::Mutex::new(None));
        let client = MockMcpClient {
            received_args: Arc::clone(&received),
        };
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(client));

        let tool_def = McpToolDef {
            name: "add_task".to_string(),
            description: Some("Add a task".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let bridge = McpToolBridge::new("todoist", &tool_def, client);

        // Simulate what the LLM sends when optional fields are nullable:
        // real values for used fields, null for unused optional fields.
        let params = serde_json::json!({
            "content": "Buy milk",
            "due_string": "today",
            "description": null,
            "deadline_date": null,
            "parent_id": null
        });

        let result = bridge.execute(params).await;
        assert!(result.is_ok());

        let forwarded = received.lock().await.take().expect("call_tool was called");
        let map = forwarded.as_object().expect("args should be an object");

        // Real values are forwarded.
        assert_eq!(
            map.get("content").and_then(|v| v.as_str()),
            Some("Buy milk")
        );
        assert_eq!(
            map.get("due_string").and_then(|v| v.as_str()),
            Some("today")
        );

        // Null values are stripped.
        assert!(!map.contains_key("description"));
        assert!(!map.contains_key("deadline_date"));
        assert!(!map.contains_key("parent_id"));
    }

    #[tokio::test]
    async fn test_execute_strips_nulls_in_nested_objects() {
        let received = Arc::new(tokio::sync::Mutex::new(None));
        let client = MockMcpClient {
            received_args: Arc::clone(&received),
        };
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(client));

        let tool_def = McpToolDef {
            name: "create".to_string(),
            description: Some("Create".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let bridge = McpToolBridge::new("svc", &tool_def, client);

        let params = serde_json::json!({
            "tasks": [
                {
                    "content": "Test",
                    "due": "today",
                    "parent_id": null,
                    "section_id": null
                }
            ],
            "project": {
                "name": "Inbox",
                "color": null
            }
        });

        let result = bridge.execute(params).await;
        assert!(result.is_ok());

        let forwarded = received.lock().await.take().expect("call_tool was called");

        // Array item nulls stripped
        let task = &forwarded["tasks"][0];
        assert_eq!(task["content"], "Test");
        assert_eq!(task["due"], "today");
        assert!(task.get("parent_id").is_none());
        assert!(task.get("section_id").is_none());

        // Nested object nulls stripped
        let project = &forwarded["project"];
        assert_eq!(project["name"], "Inbox");
        assert!(project.get("color").is_none());
    }

    #[tokio::test]
    async fn test_execute_uses_replacement_client_after_restart() {
        let old_received = Arc::new(tokio::sync::Mutex::new(None));
        let old_client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(MockMcpClient {
            received_args: Arc::clone(&old_received),
        }));
        let slot = Arc::new(ClientSlot::with_client(old_client));

        let tool_def = McpToolDef {
            name: "query".to_string(),
            description: Some("Search".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let bridge = McpToolBridge::from_slot_tool("search", &tool_def, Arc::clone(&slot));

        let replacement_received = Arc::new(tokio::sync::Mutex::new(None));
        let replacement: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(MockMcpClient {
            received_args: Arc::clone(&replacement_received),
        }));
        let _ = slot.replace(replacement).await;

        let params = serde_json::json!({"query": "moltis"});
        bridge.execute(params.clone()).await.unwrap();

        assert!(old_received.lock().await.is_none());
        assert_eq!(replacement_received.lock().await.as_ref(), Some(&params));
    }

    #[tokio::test]
    async fn test_invalidated_slot_does_not_follow_recreated_server() {
        let old_received = Arc::new(tokio::sync::Mutex::new(None));
        let old_client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(MockMcpClient {
            received_args: Arc::clone(&old_received),
        }));
        let old_slot = Arc::new(ClientSlot::with_client(old_client));

        let tool_def = McpToolDef {
            name: "query".to_string(),
            description: Some("Search".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let bridge = McpToolBridge::from_slot_tool("search", &tool_def, Arc::clone(&old_slot));

        let _ = old_slot.take().await;
        let replacement_received = Arc::new(tokio::sync::Mutex::new(None));
        let replacement: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(MockMcpClient {
            received_args: Arc::clone(&replacement_received),
        }));
        let _new_slot = ClientSlot::with_client(replacement);

        let error = bridge
            .execute(serde_json::json!({"query": "moltis"}))
            .await
            .expect_err("invalidated bridge should not call a recreated server");

        assert!(error.to_string().contains("is not running"));
        assert!(old_received.lock().await.is_none());
        assert!(replacement_received.lock().await.is_none());
    }
}
