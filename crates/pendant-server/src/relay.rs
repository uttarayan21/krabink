//! WebSocket relay: one [`pendant_core::ServerSession`] per connection, a
//! shared peer registry for broadcasts, and the doc registry behind a mutex.
//!
//! Locks are only ever taken one at a time and never held across an await.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use pendant_core::{ClientMsg, DocKey, ServerEffect, ServerMsg, ServerSession};
use tokio::sync::mpsc;

use crate::docs::ServerDocs;

struct Peer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    subscribed: HashSet<DocKey>,
}

#[derive(Default)]
pub struct PeerRegistry {
    next_id: u64,
    peers: HashMap<u64, Peer>,
}

#[derive(Clone)]
pub struct AppState {
    pub docs: Arc<Mutex<ServerDocs>>,
    pub peers: Arc<Mutex<PeerRegistry>>,
    /// Accepted bearer tokens; grows at runtime when an embedding client
    /// adopts a new workspace token.
    pub tokens: Arc<RwLock<Vec<String>>>,
}

impl AppState {
    pub fn tokens(&self) -> Vec<String> {
        self.tokens.read().expect("token list poisoned").clone()
    }

    /// Accept `token` from now on (no-op if already accepted).
    pub fn add_token(&self, token: String) {
        let mut tokens = self.tokens.write().expect("token list poisoned");
        if !token.is_empty() && !tokens.contains(&token) {
            tokens.push(token);
        }
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let authorized = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| state.tokens().iter().any(|t| t == token));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let peer_id = {
        let mut peers = state.peers.lock().expect("peer registry poisoned");
        let id = peers.next_id;
        peers.next_id += 1;
        peers.peers.insert(
            id,
            Peer {
                tx,
                subscribed: HashSet::new(),
            },
        );
        id
    };

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
    });

    let mut session = ServerSession::new(state.tokens());

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Binary(frame) = msg else {
            continue; // pings/pongs handled by axum; text frames ignored
        };
        let msg = match ClientMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(peer_id, %err, "undecodable frame; closing");
                break;
            }
        };

        let effects = {
            let mut docs = state.docs.lock().expect("doc registry poisoned");
            session.handle(msg, &mut *docs)
        };

        // Mirror this session's subscription set into the shared registry so
        // broadcasts can be routed without reaching into other tasks.
        {
            let mut peers = state.peers.lock().expect("peer registry poisoned");
            if let Some(peer) = peers.peers.get_mut(&peer_id) {
                peer.subscribed = session.subscriptions();
            }
        }

        let mut disconnect = false;
        for effect in effects {
            match effect {
                ServerEffect::Send(msg) => {
                    send_to(&state, peer_id, &msg);
                }
                ServerEffect::Broadcast { doc, msg } => {
                    let frame = match msg.encode() {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::error!(%err, "broadcast encode failed");
                            continue;
                        }
                    };
                    let peers = state.peers.lock().expect("peer registry poisoned");
                    for (&id, peer) in &peers.peers {
                        if id != peer_id && peer.subscribed.contains(&doc) {
                            let _ = peer.tx.send(frame.clone());
                        }
                    }
                }
                ServerEffect::Disconnect { code, message } => {
                    tracing::info!(peer_id, ?code, message, "disconnecting peer");
                    send_to(&state, peer_id, &ServerMsg::Error { code, message });
                    disconnect = true;
                }
            }
        }
        if disconnect {
            break;
        }
    }

    state
        .peers
        .lock()
        .expect("peer registry poisoned")
        .peers
        .remove(&peer_id);
    writer.abort();
}

fn send_to(state: &AppState, peer_id: u64, msg: &ServerMsg) {
    let frame = match msg.encode() {
        Ok(frame) => frame,
        Err(err) => {
            tracing::error!(%err, "encode failed");
            return;
        }
    };
    let peers = state.peers.lock().expect("peer registry poisoned");
    if let Some(peer) = peers.peers.get(&peer_id) {
        let _ = peer.tx.send(frame);
    }
}
