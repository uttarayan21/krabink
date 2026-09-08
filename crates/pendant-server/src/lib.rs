//! Library surface of the pendant sync relay, split from the binary so
//! integration tests can mount the real router on an ephemeral port.

pub mod docs;
pub mod relay;

use std::sync::{Arc, Mutex};

use pendant_core::Store;

use crate::docs::ServerDocs;
use crate::relay::{AppState, PeerRegistry};

pub fn app_state(store: Store, tokens: Vec<String>) -> AppState {
    AppState {
        docs: Arc::new(Mutex::new(ServerDocs::new(store))),
        peers: Arc::new(Mutex::new(PeerRegistry::default())),
        tokens: Arc::new(tokens),
    }
}

pub fn router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/ws", axum::routing::get(relay::ws_handler))
        .with_state(state)
}
