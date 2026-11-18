//! Per-session MCP connections.
//!
//! One [`ClientSlot`] per server is shared by every chat session in the
//! gateway process (see `McpManagerInner::clients`). For every transport that
//! is wrong in the same way: the connection holds mutable per-conversation
//! state that two concurrent turns must not share.
//!
//! - Streamable HTTP / SSE: `SseTransport::session_id` is a single mutable
//!   string. Two sessions therefore present the *same* `Mcp-Session-Id` to the
//!   server (so they are one session server-side), the HTTP-404 recovery path
//!   re-`initialize`s and swaps that id under a concurrent caller, and
//!   `kill()` DELETEs the session every other caller is still using.
//! - Stdio: `StdioTransport` multiplexes one child's `stdin`, one pending map
//!   and one id counter; `PendingRequestGuard::drop`/`cancel()` emit
//!   `notifications/cancelled` for another session's in-flight request.
//!
//! [`SessionPool`] gives each chat session its own connection to a server, so a
//! session's abort, timeout or teardown cannot disturb a concurrent sibling.
//! Sessions are pooled and bounded; the least recently used connection is shut
//! down when the pool is full.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use {
    futures::future::BoxFuture,
    tokio::sync::Mutex,
    tracing::{debug, warn},
};

use crate::{
    client_slot::{ClientSlot, SharedMcpClient},
    error::Result,
};

/// Opens one fresh connection to a specific MCP server.
///
/// Built once per running server by [`crate::manager::McpManager`] and reused
/// for every session that needs its own connection.
pub(crate) type SessionConnector =
    Arc<dyn Fn() -> BoxFuture<'static, Result<SharedMcpClient>> + Send + Sync>;

/// How many per-session connections one server may keep open.
///
/// A household gateway serves a handful of concurrent conversations; the cap
/// is a safety valve against an unbounded session-key space, not a tuning
/// knob.
pub(crate) const DEFAULT_MAX_SESSION_CONNECTIONS: usize = 32;

/// The session key a bridge uses when it was handed no session at all.
///
/// Startup probing, status checks and the CLI paths call tools outside any
/// chat session; they share this one connection, which is exactly the old
/// single-slot behaviour for callers that have no session identity.
pub(crate) const UNSCOPED_SESSION: &str = "";

/// A single session's private connection to one server.
struct PoolEntry {
    slot: Arc<ClientSlot>,
    /// Serialises connection setup for this session only. Never held while a
    /// tool call runs, so concurrent calls in one session are still parallel.
    connect: Mutex<()>,
    /// Milliseconds since pool creation, for LRU eviction.
    last_used_ms: AtomicU64,
}

impl PoolEntry {
    fn new() -> Self {
        Self {
            slot: Arc::new(ClientSlot::empty()),
            connect: Mutex::new(()),
            last_used_ms: AtomicU64::new(0),
        }
    }

    fn touch(&self, now_ms: u64) {
        self.last_used_ms.store(now_ms, Ordering::Relaxed);
    }

    fn last_used(&self) -> u64 {
        self.last_used_ms.load(Ordering::Relaxed)
    }
}

/// Per-session connections for a single MCP server.
///
/// A pool is rebuilt on every server start, so connection state can never
/// outlive the transport it was opened on; tool bridges captured in the
/// `ToolRegistry` follow the new pool because they resolve it through the
/// manager on every call.
pub(crate) struct SessionPool {
    server_name: String,
    connector: SessionConnector,
    entries: Mutex<HashMap<String, Arc<PoolEntry>>>,
    max_connections: usize,
    started: Instant,
}

impl SessionPool {
    pub(crate) fn new(
        server_name: String,
        connector: SessionConnector,
        max_connections: usize,
    ) -> Self {
        Self {
            server_name,
            connector,
            entries: Mutex::new(HashMap::new()),
            max_connections: max_connections.max(1),
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// The session's client, connecting on first use.
    ///
    /// Returns `None` only when the connector fails; the error is logged with
    /// the server name so a broken per-session connect is diagnosable without
    /// failing every other session.
    pub(crate) async fn client_for_session(&self, session_key: &str) -> Option<Arc<ClientSlot>> {
        let (entry, evicted) = {
            let mut entries = self.entries.lock().await;
            let now = self.now_ms();
            let entry = Arc::clone(
                entries
                    .entry(session_key.to_string())
                    .or_insert_with(|| Arc::new(PoolEntry::new())),
            );
            entry.touch(now);
            let evicted = self.evict_locked(&mut entries, session_key);
            (entry, evicted)
        };
        if let Some(evicted) = evicted {
            // Shut the evicted connection down off the hot path: its owner may
            // still be finishing a call.
            tokio::spawn(async move { shut_down(evicted).await });
        }

        // Fast path: this session already has a live connection.
        if entry.slot.current().await.is_some() {
            entry.touch(self.now_ms());
            return Some(Arc::clone(&entry.slot));
        }

        // Connect while holding only this session's own lock, so one slow
        // connect cannot serialise unrelated sessions.
        let _connecting = entry.connect.lock().await;
        if entry.slot.current().await.is_some() {
            entry.touch(self.now_ms());
            return Some(Arc::clone(&entry.slot));
        }

        match (self.connector)().await {
            Ok(client) => {
                entry.slot.replace(client).await;
                entry.touch(self.now_ms());
                debug!(server = %self.server_name, "opened per-session MCP connection");
                Some(Arc::clone(&entry.slot))
            },
            Err(error) => {
                warn!(
                    server = %self.server_name,
                    %error,
                    "failed to open per-session MCP connection"
                );
                // Drop the half-built entry so the next call retries cleanly.
                let mut entries = self.entries.lock().await;
                if entries
                    .get(session_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    entries.remove(session_key);
                }
                None
            },
        }
    }

    /// Drop the least recently used session when the pool is over capacity.
    ///
    /// The requesting session is never evicted. Returns the slot to shut down.
    fn evict_locked(
        &self,
        entries: &mut HashMap<String, Arc<PoolEntry>>,
        keep: &str,
    ) -> Option<Arc<ClientSlot>> {
        if entries.len() <= self.max_connections {
            return None;
        }
        let victim = entries
            .iter()
            .filter(|(key, _)| key.as_str() != keep)
            .min_by_key(|(_, entry)| entry.last_used())
            .map(|(key, _)| key.clone())?;
        let removed = entries.remove(&victim)?;
        debug!(server = %self.server_name, "evicting idle per-session MCP connection");
        Some(Arc::clone(&removed.slot))
    }

    /// Sessions currently holding a pooled connection.
    #[cfg(test)]
    pub(crate) async fn session_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Shut down every session connection. Used when the server stops.
    pub(crate) async fn shutdown_all(&self) {
        let drained: Vec<Arc<PoolEntry>> = {
            let mut entries = self.entries.lock().await;
            entries.drain().map(|(_, entry)| entry).collect()
        };
        for entry in drained {
            shut_down(Arc::clone(&entry.slot)).await;
        }
    }
}

/// Take the slot's client, if any, and shut it down.
async fn shut_down(slot: Arc<ClientSlot>) {
    if let Some(client) = slot.take().await {
        let mut client = client.write().await;
        client.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use {async_trait::async_trait, serde_json::Value, tokio::sync::RwLock};

    use {
        super::*,
        crate::{
            client::McpClientState,
            traits::McpClientTrait,
            types::{McpToolDef, ToolContent, ToolsCallResult},
        },
    };

    /// Records which pool entry it belongs to, so a test can prove two
    /// sessions got two different connections.
    struct LabeledClient {
        label: usize,
        tools: Vec<McpToolDef>,
        shutdowns: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl McpClientTrait for LabeledClient {
        fn server_name(&self) -> &str {
            "test"
        }

        fn state(&self) -> McpClientState {
            McpClientState::Ready
        }

        fn tools(&self) -> &[McpToolDef] {
            &self.tools
        }

        async fn list_tools(&mut self) -> Result<&[McpToolDef]> {
            Ok(&self.tools)
        }

        async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<ToolsCallResult> {
            Ok(ToolsCallResult {
                content: vec![ToolContent::Text {
                    text: self.label.to_string(),
                }],
                is_error: false,
            })
        }

        async fn is_alive(&self) -> bool {
            true
        }

        async fn shutdown(&mut self) {
            self.shutdowns.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    fn counting_pool(
        max: usize,
        created: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
    ) -> SessionPool {
        let connector: SessionConnector = Arc::new(move || {
            let created = Arc::clone(&created);
            let shutdowns = Arc::clone(&shutdowns);
            Box::pin(async move {
                let label = created.fetch_add(1, AtomicOrdering::SeqCst);
                // A real connector does network or process work here.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let client: SharedMcpClient = Arc::new(RwLock::new(LabeledClient {
                    label,
                    tools: Vec::new(),
                    shutdowns,
                }));
                Ok(client)
            })
        });
        SessionPool::new("test".to_string(), connector, max)
    }

    /// The defect this module exists to fix: two sessions must not share one
    /// connection. Before per-session pooling both sessions resolved the same
    /// `ClientSlot`, so the second turn saw the first turn's client identity.
    #[tokio::test]
    async fn two_sessions_get_independent_connections() {
        let created = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(8, Arc::clone(&created), Arc::clone(&shutdowns));

        // Both sessions connect at the same time, as two parents chatting at
        // once would.
        let (alice, bob) = tokio::join!(
            pool.client_for_session("web:alice"),
            pool.client_for_session("web:bob"),
        );
        let alice = alice.expect("alice connected");
        let bob = bob.expect("bob connected");

        assert!(!Arc::ptr_eq(&alice, &bob), "sessions must not share a slot");

        let alice_client = alice.current().await.expect("alice client");
        let bob_client = bob.current().await.expect("bob client");
        assert!(
            !Arc::ptr_eq(&alice_client, &bob_client),
            "sessions must not share an MCP client"
        );
        assert_eq!(created.load(AtomicOrdering::SeqCst), 2);
    }

    /// One session keeps one connection across calls.
    #[tokio::test]
    async fn a_session_reuses_its_own_connection() {
        let created = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(8, Arc::clone(&created), Arc::clone(&shutdowns));

        let first = pool.client_for_session("web:alice").await.unwrap();
        let second = pool.client_for_session("web:alice").await.unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(created.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(pool.session_count().await, 1);
    }

    /// Concurrent first calls in the same session must not open two children.
    #[tokio::test]
    async fn concurrent_connect_in_one_session_opens_one_connection() {
        let created = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(8, Arc::clone(&created), Arc::clone(&shutdowns));

        let (a, b) = tokio::join!(
            pool.client_for_session("web:alice"),
            pool.client_for_session("web:alice"),
        );

        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(created.load(AtomicOrdering::SeqCst), 1);
    }

    /// The pool is bounded: the least recently used session loses its
    /// connection (and its client is shut down) when a new one arrives.
    #[tokio::test]
    async fn least_recently_used_session_is_evicted_and_shut_down() {
        let created = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(2, Arc::clone(&created), Arc::clone(&shutdowns));

        // `a` is touched first, then `b`; `a` is therefore the LRU victim.
        pool.client_for_session("web:a").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pool.client_for_session("web:b").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pool.client_for_session("web:c").await.unwrap();

        assert_eq!(pool.session_count().await, 2);
        // Eviction shuts the old connection down on a background task.
        for _ in 0..50 {
            if shutdowns.load(AtomicOrdering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            shutdowns.load(AtomicOrdering::SeqCst),
            1,
            "evicted session's client must be shut down"
        );
    }

    /// `shutdown_all` drains the pool and closes every connection.
    #[tokio::test]
    async fn shutdown_all_closes_every_session() {
        let created = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let pool = counting_pool(8, Arc::clone(&created), Arc::clone(&shutdowns));

        pool.client_for_session("web:a").await.unwrap();
        pool.client_for_session("web:b").await.unwrap();
        pool.shutdown_all().await;

        assert_eq!(pool.session_count().await, 0);
        assert_eq!(shutdowns.load(AtomicOrdering::SeqCst), 2);
    }

    /// A failed connect is not cached: the next call retries.
    #[tokio::test]
    async fn failed_connect_is_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_connector = Arc::clone(&attempts);
        let connector: SessionConnector = Arc::new(move || {
            let attempts = Arc::clone(&attempts_for_connector);
            Box::pin(async move {
                let n = attempts.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    return Err(crate::error::Error::message("connect refused"));
                }
                let client: SharedMcpClient = Arc::new(RwLock::new(LabeledClient {
                    label: n,
                    tools: Vec::new(),
                    shutdowns: Arc::new(AtomicUsize::new(0)),
                }));
                Ok(client)
            })
        });
        let pool = SessionPool::new("test".to_string(), connector, 8);

        assert!(pool.client_for_session("web:a").await.is_none());
        assert_eq!(pool.session_count().await, 0, "failure must not be cached");
        assert!(pool.client_for_session("web:a").await.is_some());
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }
}
