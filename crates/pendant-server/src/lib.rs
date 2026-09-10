//! Library surface of the pendant sync relay, split from the binary so
//! integration tests can mount the real router on an ephemeral port and the
//! desktop client can embed a relay in-process.

pub mod docs;
pub mod relay;

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use pendant_core::Store;

use crate::docs::{IdleDocs, ServerDocs};
pub use crate::relay::AppState;
use crate::relay::PeerRegistry;

/// How often open docs are checkpointed to disk and idle ones unloaded.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(30);

impl AppState {
    /// A relay over `store` accepting `tokens` as bearer tokens.
    pub fn new(store: Store, tokens: Vec<String>) -> Self {
        Self {
            docs: Arc::new(Mutex::new(ServerDocs::new(store))),
            peers: Arc::new(Mutex::new(PeerRegistry::default())),
            tokens: Arc::new(RwLock::new(tokens)),
        }
    }

    /// The websocket router; serve it with `axum::serve`.
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .route("/ws", axum::routing::get(relay::ws_handler))
            .with_state(self.clone())
    }

    /// Periodic checkpoint + idle unload; runs until the task is dropped.
    /// The checkpoint hits redb, so it runs on the blocking pool rather
    /// than stalling a runtime worker every tick.
    pub async fn maintenance(self) {
        let mut tick = tokio::time::interval(MAINTAIN_EVERY);
        loop {
            tick.tick().await;
            let docs = Arc::clone(&self.docs);
            let result = tokio::task::spawn_blocking(move || {
                docs.lock()
                    .expect("doc registry poisoned")
                    .maintain(IdleDocs::Unload)
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!(%err, "maintenance failed"),
                Err(err) => tracing::error!(%err, "maintenance task panicked"),
            }
        }
    }

    /// Durable checkpoint of every open doc; call once before exit.
    pub fn checkpoint(&self) -> pendant_core::Result<()> {
        self.docs
            .lock()
            .expect("doc registry poisoned")
            .maintain(IdleDocs::Keep)
    }
}
