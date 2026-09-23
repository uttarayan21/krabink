//! Sans-io sync protocol: wire types plus the client and server session state
//! machines. No sockets here - callers feed decoded frames in and execute the
//! returned effects, so the whole protocol is unit-testable over in-memory
//! queues and reused verbatim by the desktop app, the server and the FFI
//! layer.
//!
//! Frame layout: one leading protocol-version byte, then the postcard-encoded
//! message. CRDT payloads (`Update`, catch-up) are opaque bytes produced and
//! consumed by [`crate::NoteDoc`]/[`crate::WorkspaceDoc`]; ephemeral payloads
//! (presence, live wet-ink) are opaque to the protocol and never persisted.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{DeviceId, DocKey, Error, Result};

/// Bump on any incompatible wire change.
pub const PROTO_VERSION: u8 = 1;

// ---- wire messages ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        device: DeviceId,
        token: String,
    },
    ListDocs,
    /// `have`: client's encoded version vector for the doc (empty = nothing).
    Subscribe {
        doc: DocKey,
        have: Vec<u8>,
    },
    Unsubscribe {
        doc: DocKey,
    },
    Update {
        doc: DocKey,
        payload: Vec<u8>,
    },
    Ephemeral {
        doc: DocKey,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    HelloAck,
    DocList {
        docs: Vec<DocKey>,
    },
    /// `catch_up` may be an updates blob or a full snapshot (when the client's
    /// version predates history GC) - loro's import auto-detects either.
    SubscribeAck {
        doc: DocKey,
        server_have: Vec<u8>,
        catch_up: Vec<u8>,
    },
    Update {
        doc: DocKey,
        payload: Vec<u8>,
        from: DeviceId,
    },
    Ephemeral {
        doc: DocKey,
        payload: Vec<u8>,
        from: DeviceId,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    BadVersion,
    BadToken,
    NotAuthenticated,
    NotSubscribed,
    DocFailure,
}

// ---- frame codec ----

fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let mut frame = vec![PROTO_VERSION];
    frame = postcard::to_extend(msg, frame)?;
    Ok(frame)
}

fn decode_frame<'a, T: Deserialize<'a>>(frame: &'a [u8]) -> Result<T> {
    match frame.split_first() {
        Some((&PROTO_VERSION, body)) => Ok(postcard::from_bytes(body)?),
        Some((version, _)) => Err(Error::Protocol(format!(
            "unsupported protocol version {version}"
        ))),
        None => Err(Error::Protocol("empty frame".into())),
    }
}

impl ClientMsg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_frame(self)
    }

    pub fn decode(frame: &[u8]) -> Result<Self> {
        decode_frame(frame)
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode_frame(self)
    }

    pub fn decode(frame: &[u8]) -> Result<Self> {
        decode_frame(frame)
    }
}

// ---- document access, injected by the io layer ----

/// Catch-up bundle for one doc: what the server knows plus what the peer lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUp {
    pub server_have: Vec<u8>,
    pub payload: Vec<u8>,
}

/// Server-side document access. Implemented over the doc actors + [`crate::Store`].
pub trait DocProvider {
    /// Updates the peer at `have` is missing, plus our own version vector.
    fn catch_up(&mut self, doc: DocKey, have: &[u8]) -> Result<CatchUp>;
    /// Import (and persist) an incoming update. Returns whether it changed
    /// the doc; duplicates (already seen via another peer) return `false`
    /// and are not re-broadcast, which keeps a mesh with cycles from
    /// echoing updates forever.
    fn import_update(&mut self, doc: DocKey, payload: &[u8]) -> Result<bool>;
    fn list_docs(&mut self) -> Vec<DocKey>;
}

/// Client-side document access. Implemented over the open local docs.
pub trait ClientDocs {
    /// Import a catch-up or live update. Returns whether it changed the doc
    /// (see [`DocProvider::import_update`]).
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> Result<bool>;
    /// Everything the local doc has that the peer at `have` lacks.
    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> Result<Vec<u8>>;
}

// ---- client session ----

#[derive(Debug, PartialEq, Eq)]
pub enum ClientEffect {
    /// Encode and send to the server.
    Send(ClientMsg),
    /// Handshake finished; subscriptions may now be issued.
    Connected,
    /// Sync state for `doc` is established (catch-up applied, backfill sent).
    DocSynced(DocKey),
    /// Deliver a remote ephemeral payload (presence / wet ink) to the UI.
    Ephemeral {
        doc: DocKey,
        payload: Vec<u8>,
        from: DeviceId,
    },
    /// The server rejected us; reconnecting without change is pointless.
    Fatal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Idle,
    HelloSent,
    Ready,
}

/// Protocol state for one client connection.
///
/// Lifecycle: [`ClientSession::connect`] once the transport is up, then feed
/// every inbound frame to [`ClientSession::handle`] and every local CRDT
/// commit to [`ClientSession::local_update`]. On transport loss, drop the
/// session and build a new one (version vectors make reconnect cheap).
pub struct ClientSession {
    device: DeviceId,
    token: String,
    state: ClientState,
    subscribed: HashSet<DocKey>,
}

impl ClientSession {
    pub fn new(device: DeviceId, token: String) -> Self {
        Self {
            device,
            token,
            state: ClientState::Idle,
            subscribed: HashSet::new(),
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    pub fn is_ready(&self) -> bool {
        self.state == ClientState::Ready
    }

    /// True once a `SubscribeAck` for `doc` has been processed.
    pub fn is_subscribed(&self, doc: DocKey) -> bool {
        self.subscribed.contains(&doc)
    }

    /// Begin the handshake. Call once per transport connection.
    pub fn connect(&mut self) -> Vec<ClientEffect> {
        self.state = ClientState::HelloSent;
        vec![ClientEffect::Send(ClientMsg::Hello {
            device: self.device,
            token: self.token.clone(),
        })]
    }

    /// Ask the server for a doc. `have` is the local doc's current version.
    pub fn subscribe(&mut self, doc: DocKey, have: Vec<u8>) -> Vec<ClientEffect> {
        if self.state != ClientState::Ready {
            return vec![ClientEffect::Fatal("subscribe before handshake".into())];
        }
        vec![ClientEffect::Send(ClientMsg::Subscribe { doc, have })]
    }

    /// Forward a local CRDT commit. No-op for docs the server doesn't track yet.
    pub fn local_update(&mut self, doc: DocKey, payload: Vec<u8>) -> Vec<ClientEffect> {
        if self.state == ClientState::Ready && self.subscribed.contains(&doc) {
            vec![ClientEffect::Send(ClientMsg::Update { doc, payload })]
        } else {
            Vec::new()
        }
    }

    /// Send a transient payload (presence / live wet ink).
    pub fn ephemeral(&mut self, doc: DocKey, payload: Vec<u8>) -> Vec<ClientEffect> {
        if self.state == ClientState::Ready && self.subscribed.contains(&doc) {
            vec![ClientEffect::Send(ClientMsg::Ephemeral { doc, payload })]
        } else {
            Vec::new()
        }
    }

    pub fn handle(&mut self, msg: ServerMsg, docs: &mut impl ClientDocs) -> Vec<ClientEffect> {
        match msg {
            ServerMsg::HelloAck => {
                self.state = ClientState::Ready;
                vec![ClientEffect::Connected]
            }
            ServerMsg::SubscribeAck {
                doc,
                server_have,
                catch_up,
            } => {
                if let Err(err) = docs.import(doc, &catch_up) {
                    return vec![ClientEffect::Fatal(format!(
                        "catch-up import failed: {err}"
                    ))];
                }
                self.subscribed.insert(doc);
                let mut effects = Vec::new();
                match docs.updates_since(doc, &server_have) {
                    Ok(backfill) if !backfill.is_empty() => {
                        effects.push(ClientEffect::Send(ClientMsg::Update {
                            doc,
                            payload: backfill,
                        }));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        return vec![ClientEffect::Fatal(format!(
                            "backfill export failed: {err}"
                        ))];
                    }
                }
                effects.push(ClientEffect::DocSynced(doc));
                effects
            }
            ServerMsg::Update { doc, payload, .. } => match docs.import(doc, &payload) {
                Ok(_) => Vec::new(),
                Err(err) => vec![ClientEffect::Fatal(format!("remote import failed: {err}"))],
            },
            ServerMsg::Ephemeral { doc, payload, from } => {
                vec![ClientEffect::Ephemeral { doc, payload, from }]
            }
            ServerMsg::DocList { .. } => Vec::new(),
            ServerMsg::Error { code, message } => {
                vec![ClientEffect::Fatal(format!(
                    "server error {code:?}: {message}"
                ))]
            }
        }
    }
}

// ---- server session ----

#[derive(Debug, PartialEq, Eq)]
pub enum ServerEffect {
    /// Encode and send to this connection.
    Send(ServerMsg),
    /// Relay to every *other* connection subscribed to `doc`.
    Broadcast { doc: DocKey, msg: ServerMsg },
    /// Protocol violation; close the transport.
    Disconnect { code: ErrorCode, message: String },
}

/// Protocol state for one connection on the server. One instance per client.
pub struct ServerSession {
    tokens: Vec<String>,
    device: Option<DeviceId>,
    subscribed: HashSet<DocKey>,
}

impl ServerSession {
    pub fn new(tokens: Vec<String>) -> Self {
        Self {
            tokens,
            device: None,
            subscribed: HashSet::new(),
        }
    }

    /// The authenticated device, once `Hello` has been accepted.
    pub fn device(&self) -> Option<DeviceId> {
        self.device
    }

    pub fn is_subscribed(&self, doc: DocKey) -> bool {
        self.subscribed.contains(&doc)
    }

    /// Snapshot of this session's subscriptions, for broadcast routing.
    pub fn subscriptions(&self) -> HashSet<DocKey> {
        self.subscribed.clone()
    }

    pub fn handle(&mut self, msg: ClientMsg, docs: &mut impl DocProvider) -> Vec<ServerEffect> {
        let Some(device) = self.device else {
            // Only Hello is legal before authentication.
            return match msg {
                ClientMsg::Hello { device, token } => {
                    if self.tokens.contains(&token) {
                        self.device = Some(device);
                        vec![ServerEffect::Send(ServerMsg::HelloAck)]
                    } else {
                        vec![ServerEffect::Disconnect {
                            code: ErrorCode::BadToken,
                            message: "invalid token".into(),
                        }]
                    }
                }
                _ => vec![ServerEffect::Disconnect {
                    code: ErrorCode::NotAuthenticated,
                    message: "message before hello".into(),
                }],
            };
        };

        match msg {
            ClientMsg::Hello { .. } => vec![ServerEffect::Disconnect {
                code: ErrorCode::NotAuthenticated,
                message: "duplicate hello".into(),
            }],
            ClientMsg::ListDocs => vec![ServerEffect::Send(ServerMsg::DocList {
                docs: docs.list_docs(),
            })],
            ClientMsg::Subscribe { doc, have } => match docs.catch_up(doc, &have) {
                Ok(catch_up) => {
                    self.subscribed.insert(doc);
                    vec![ServerEffect::Send(ServerMsg::SubscribeAck {
                        doc,
                        server_have: catch_up.server_have,
                        catch_up: catch_up.payload,
                    })]
                }
                Err(err) => vec![ServerEffect::Send(ServerMsg::Error {
                    code: ErrorCode::DocFailure,
                    message: format!("subscribe {doc:?}: {err}"),
                })],
            },
            ClientMsg::Unsubscribe { doc } => {
                self.subscribed.remove(&doc);
                Vec::new()
            }
            ClientMsg::Update { doc, payload } => {
                if !self.subscribed.contains(&doc) {
                    return vec![ServerEffect::Send(ServerMsg::Error {
                        code: ErrorCode::NotSubscribed,
                        message: format!("update for unsubscribed doc {doc:?}"),
                    })];
                }
                match docs.import_update(doc, &payload) {
                    // Already had it (arrived via another peer): nothing to relay.
                    Ok(false) => Vec::new(),
                    Ok(true) => vec![ServerEffect::Broadcast {
                        doc,
                        // Relay the raw payload untouched - no re-encode.
                        msg: ServerMsg::Update {
                            doc,
                            payload,
                            from: device,
                        },
                    }],
                    Err(err) => vec![ServerEffect::Send(ServerMsg::Error {
                        code: ErrorCode::DocFailure,
                        message: format!("import {doc:?}: {err}"),
                    })],
                }
            }
            ClientMsg::Ephemeral { doc, payload } => {
                if self.subscribed.contains(&doc) {
                    vec![ServerEffect::Broadcast {
                        doc,
                        msg: ServerMsg::Ephemeral {
                            doc,
                            payload,
                            from: device,
                        },
                    }]
                } else {
                    Vec::new() // transient; drop silently
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let msg = ClientMsg::Subscribe {
            doc: DocKey::WORKSPACE,
            have: vec![1, 2, 3],
        };
        assert_eq!(ClientMsg::decode(&msg.encode().unwrap()).unwrap(), msg);
    }

    #[test]
    fn wrong_version_rejected() {
        let mut frame = ClientMsg::ListDocs.encode().unwrap();
        frame[0] = PROTO_VERSION + 1;
        assert!(ClientMsg::decode(&frame).is_err());
        assert!(ClientMsg::decode(&[]).is_err());
    }
}
