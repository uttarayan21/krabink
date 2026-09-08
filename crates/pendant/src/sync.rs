//! Sync plugin: a tokio task owns the WebSocket (with reconnect + backoff)
//! and shuttles raw frames over channels; the Bevy side owns the docs and the
//! sans-io [`ClientSession`], driven once per frame.

use bevy::prelude::*;
use futures::{SinkExt, StreamExt};
use pendant_core::{ClientEffect, ClientSession, DeviceId, DocKey, ServerMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::docs::Docs;
use crate::sketch::WetInkFrame;
use crate::ui::EditorState;

const RECONNECT_MIN: std::time::Duration = std::time::Duration::from_millis(500);
const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Raised by the UI when a doc needs server subscription (note opened).
#[derive(Message)]
pub struct SubscribeNeeded(pub DocKey);

/// Raised whenever a local CRDT commit produced a sync payload.
#[derive(Message)]
pub struct LocalCommit {
    pub doc: DocKey,
    pub payload: Vec<u8>,
}

enum TransportEvent {
    Up,
    Down,
    Frame(Vec<u8>),
}

#[derive(Resource)]
pub struct SyncTransport {
    /// Keeps the runtime (and the websocket task) alive.
    _runtime: Option<tokio::runtime::Runtime>,
    inbound: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    outbound: Option<mpsc::UnboundedSender<Vec<u8>>>,
    session: Option<ClientSession>,
    device: DeviceId,
    token: String,
}

impl SyncTransport {
    /// Offline mode: no server configured.
    pub fn disabled(device: DeviceId) -> Self {
        Self {
            _runtime: None,
            inbound: None,
            outbound: None,
            session: None,
            device,
            token: String::new(),
        }
    }

    pub fn connect(server: String, token: String, device: DeviceId) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        runtime.spawn(transport_task(server, token.clone(), in_tx, out_rx));
        Self {
            _runtime: Some(runtime),
            inbound: Some(in_rx),
            outbound: Some(out_tx),
            session: None,
            device,
            token,
        }
    }

    fn send(&self, frame: Vec<u8>) {
        if let Some(outbound) = &self.outbound {
            let _ = outbound.send(frame);
        }
    }
}

/// Owns the socket forever: connect, pump, reconnect with backoff.
async fn transport_task(
    server: String,
    token: String,
    inbound: mpsc::UnboundedSender<TransportEvent>,
    mut outbound: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
        let request = match server.as_str().into_client_request() {
            Ok(mut request) => {
                match format!("Bearer {token}").parse() {
                    Ok(value) => {
                        request.headers_mut().insert("Authorization", value);
                    }
                    Err(err) => {
                        tracing::error!(%err, "token not header-safe; sync disabled");
                        return;
                    }
                }
                request
            }
            Err(err) => {
                tracing::error!(%err, server, "invalid server url; sync disabled");
                return;
            }
        };

        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => {
                tracing::info!(server, "sync connected");
                backoff = RECONNECT_MIN;
                if inbound.send(TransportEvent::Up).is_err() {
                    return;
                }
                let (mut sink, mut stream) = socket.split();
                loop {
                    tokio::select! {
                        frame = outbound.recv() => {
                            let Some(frame) = frame else { return };
                            if sink.send(WsMessage::Binary(frame.into())).await.is_err() {
                                break;
                            }
                        }
                        msg = stream.next() => {
                            match msg {
                                Some(Ok(WsMessage::Binary(bytes))) => {
                                    if inbound.send(TransportEvent::Frame(bytes.to_vec())).is_err() {
                                        return;
                                    }
                                }
                                Some(Ok(_)) => {}
                                Some(Err(_)) | None => break,
                            }
                        }
                    }
                }
                if inbound.send(TransportEvent::Down).is_err() {
                    return;
                }
            }
            Err(err) => {
                tracing::warn!(%err, server, "sync connect failed; retrying");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

pub struct SyncPlugin;

impl Plugin for SyncPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<SubscribeNeeded>()
            .add_message::<LocalCommit>()
            .add_systems(Update, drive_sync);
    }
}

fn drive_sync(
    mut transport: ResMut<SyncTransport>,
    mut docs: ResMut<Docs>,
    mut editor: ResMut<EditorState>,
    mut subscribes: MessageReader<SubscribeNeeded>,
    mut commits: MessageReader<LocalCommit>,
    mut wet: MessageWriter<WetInkFrame>,
) {
    // 1. Local commits out.
    for commit in commits.read() {
        if let Some(session) = transport.session.as_mut() {
            let effects = session.local_update(commit.doc, commit.payload.clone());
            apply_effects(effects, &mut transport, &mut docs, &mut editor, &mut wet);
        }
    }

    // 2. New subscriptions.
    for SubscribeNeeded(doc) in subscribes.read() {
        let ready = transport.session.as_ref().is_some_and(|s| s.is_ready());
        if ready {
            let have = docs.version_of(*doc);
            let session = transport.session.as_mut().expect("checked above");
            let effects = session.subscribe(*doc, have);
            apply_effects(effects, &mut transport, &mut docs, &mut editor, &mut wet);
        }
    }

    // 3. Transport events in.
    loop {
        let Some(inbound) = transport.inbound.as_mut() else {
            return;
        };
        let event = match inbound.try_recv() {
            Ok(event) => event,
            Err(mpsc::error::TryRecvError::Empty) => return,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                transport.inbound = None;
                return;
            }
        };
        match event {
            TransportEvent::Up => {
                let mut session = ClientSession::new(transport.device, transport.token.clone());
                let effects = session.connect();
                transport.session = Some(session);
                apply_effects(effects, &mut transport, &mut docs, &mut editor, &mut wet);
            }
            TransportEvent::Down => {
                transport.session = None;
            }
            TransportEvent::Frame(frame) => {
                let Some(mut session) = transport.session.take() else {
                    continue;
                };
                let msg = match ServerMsg::decode(&frame) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!(%err, "bad frame from server");
                        transport.session = Some(session);
                        continue;
                    }
                };
                if matches!(
                    msg,
                    ServerMsg::Update { .. } | ServerMsg::SubscribeAck { .. }
                ) {
                    editor.remote_dirty = true; // handle() imports into the docs
                }
                let effects = session.handle(msg, &mut *docs);
                transport.session = Some(session);
                apply_effects(effects, &mut transport, &mut docs, &mut editor, &mut wet);
            }
        }
    }
}

fn apply_effects(
    effects: Vec<ClientEffect>,
    transport: &mut SyncTransport,
    docs: &mut Docs,
    editor: &mut EditorState,
    wet: &mut MessageWriter<WetInkFrame>,
) {
    for effect in effects {
        match effect {
            ClientEffect::Send(msg) => match msg.encode() {
                Ok(frame) => transport.send(frame),
                Err(err) => tracing::error!(%err, "encode failed"),
            },
            ClientEffect::Connected => {
                // (Re)establish every subscription we care about.
                let mut wanted = vec![DocKey::WORKSPACE];
                if let Some(open) = editor.open {
                    wanted.push(DocKey::from(open));
                }
                for doc in wanted {
                    let have = docs.version_of(doc);
                    let session = transport.session.as_mut().expect("just connected");
                    let effects = session.subscribe(doc, have);
                    apply_effects(effects, transport, docs, editor, wet);
                }
            }
            ClientEffect::DocSynced(_) => {
                editor.remote_dirty = true;
            }
            ClientEffect::Ephemeral { doc, payload, .. } => {
                wet.write(WetInkFrame { doc, payload });
            }
            ClientEffect::Fatal(err) => {
                tracing::error!(%err, "sync session failed");
                transport.session = None;
            }
        }
    }
}
