//! Library surface of the pendant sync relay, split from the binary so
//! integration tests can mount the real router on an ephemeral port and the
//! desktop client can embed a relay in-process.

pub mod docs;
pub mod relay;

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use pendant_core::Store;

use crate::docs::ServerDocs;
use crate::relay::{AppState, PeerRegistry};

/// How often open docs are checkpointed to disk and idle ones unloaded.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(30);

pub fn app_state(store: Store, tokens: Vec<String>) -> AppState {
    AppState {
        docs: Arc::new(Mutex::new(ServerDocs::new(store))),
        peers: Arc::new(Mutex::new(PeerRegistry::default())),
        tokens: Arc::new(RwLock::new(tokens)),
    }
}

pub fn router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/ws", axum::routing::get(relay::ws_handler))
        .with_state(state)
}

/// Periodic checkpoint + idle unload; runs until the task is dropped.
pub async fn maintenance(docs: Arc<Mutex<ServerDocs>>) {
    let mut tick = tokio::time::interval(MAINTAIN_EVERY);
    loop {
        tick.tick().await;
        let result = docs.lock().expect("doc registry poisoned").maintain(true);
        if let Err(err) = result {
            tracing::error!(%err, "maintenance failed");
        }
    }
}

/// Durable checkpoint of every open doc; call once before exit.
pub fn checkpoint(state: &AppState) -> pendant_core::Result<()> {
    state
        .docs
        .lock()
        .expect("doc registry poisoned")
        .maintain(false)
}
