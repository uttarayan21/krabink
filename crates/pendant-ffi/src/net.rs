//! Background sync task: owns the WebSocket, drives the sans-io
//! [`ClientSession`], persists imports, and fans events out to listeners.
//! One task per [`crate::Core`], driven entirely by [`Cmd`]s — the FFI
//! mutators never block on the network.

use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pendant_core::{
    ClientDocs, ClientEffect, ClientMsg, ClientSession, DocKey, Flush, ServerMsg, WetInk,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::engine::{CoreListener, NoteListener, Shared, SyncTarget};
use crate::types::{NoteInfo, SyncState, rgba_to_u32};

pub(crate) enum Cmd {
    Connect(SyncTarget),
    Suspend,
    Subscribe(DocKey),
    Update { doc: DocKey, payload: Vec<u8> },
    Ephemeral { doc: DocKey, payload: Vec<u8> },
}

const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Sink = futures::stream::SplitSink<Socket, WsMessage>;

/// How a connection attempt ended, deciding what the outer loop does next.
enum ConnEnd {
    /// User asked to go offline; wait for the next `Connect`.
    Suspend,
    /// New connection parameters arrived mid-flight.
    Reconnect(SyncTarget),
    /// Transport dropped or failed; retry with backoff.
    Lost,
    /// Server rejected us; give up until told otherwise.
    Fatal(String),
    /// The `Core` was dropped.
    Shutdown,
}

pub(crate) async fn run(shared: Weak<Shared>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    // Offline until the first Connect; commands other than Connect are safely
    // droppable here (subscriptions are re-derived from open notes, updates
    // are re-derived from version vectors on catch-up).
    loop {
        let mut target = loop {
            match rx.recv().await {
                None => return,
                Some(Cmd::Connect(target)) => break target,
                Some(_) => {}
            }
        };

        notify_sync_state(&shared, SyncState::Connecting);
        let mut backoff = BACKOFF_START;
        // Alternate direct path / fallback relay: attempt 0 direct, 1
        // fallback, 2 direct… so a lost direct path lands on the dedicated
        // relay after one backoff and keeps probing for the direct path.
        let mut attempt = 0u32;
        loop {
            let Some(strong) = shared.upgrade() else {
                return;
            };
            let url = match (&target.fallback, attempt % 2) {
                (Some(fallback), 1) => fallback.as_str(),
                _ => target.url.as_str(),
            };
            match connection(&strong, &mut rx, url, &target.token).await {
                ConnEnd::Shutdown => return,
                ConnEnd::Suspend => {
                    notify_sync_state(&shared, SyncState::Disconnected);
                    break;
                }
                ConnEnd::Fatal(message) => {
                    notify_sync_state(&shared, SyncState::Fatal { message });
                    break;
                }
                ConnEnd::Reconnect(next) => {
                    target = next;
                    attempt = 0;
                    notify_sync_state(&shared, SyncState::Connecting);
                }
                ConnEnd::Lost => {
                    drop(strong);
                    attempt += 1;
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        cmd = rx.recv() => match cmd {
                            None => return,
                            Some(Cmd::Suspend) => {
                                notify_sync_state(&shared, SyncState::Disconnected);
                                break;
                            }
                            Some(Cmd::Connect(next)) => {
                                target = next;
                                attempt = 0;
                            }
                            Some(_) => {}
                        },
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
}

async fn connection(
    shared: &Arc<Shared>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    url: &str,
    token: &str,
) -> ConnEnd {
    let mut request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => return ConnEnd::Fatal(format!("bad server url {url:?}: {err}")),
    };
    let auth = match format!("Bearer {token}").parse() {
        Ok(auth) => auth,
        Err(_) => return ConnEnd::Fatal("token not header-safe".into()),
    };
    request.headers_mut().insert("Authorization", auth);

    let socket = match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => socket,
        Err(err) => {
            tracing::warn!(url, "sync connect failed: {err}");
            return ConnEnd::Lost;
        }
    };
    let (mut sink, mut stream) = socket.split();
    let mut session = ClientSession::new(shared.device, token.to_string());

    let effects = session.connect();
    if let Err(end) = apply_effects(&mut session, &mut sink, shared, effects).await {
        return end;
    }

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let effects = match cmd {
                    None => return ConnEnd::Shutdown,
                    Some(Cmd::Suspend) => return ConnEnd::Suspend,
                    Some(Cmd::Connect(target)) => return ConnEnd::Reconnect(target),
                    // Not ready yet: drop it — the post-handshake enumeration
                    // of open notes covers every doc present in the map, and
                    // unsubscribed updates are re-derived on catch-up.
                    Some(Cmd::Subscribe(doc)) if session.is_ready() => {
                        session.subscribe(doc, shared.doc_version(doc))
                    }
                    Some(Cmd::Update { doc, payload }) => session.local_update(doc, payload),
                    Some(Cmd::Ephemeral { doc, payload }) => session.ephemeral(doc, payload),
                    Some(Cmd::Subscribe(_)) => Vec::new(),
                };
                if let Err(end) = apply_effects(&mut session, &mut sink, shared, effects).await {
                    return end;
                }
            }
            frame = stream.next() => {
                let bytes = match frame {
                    Some(Ok(WsMessage::Binary(bytes))) => bytes,
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => return ConnEnd::Lost,
                    Some(Ok(_)) => continue,
                };
                let msg = match ServerMsg::decode(&bytes) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!("dropping malformed server frame: {err}");
                        continue;
                    }
                };
                let mut docs = Docs {
                    shared,
                    pending: Vec::new(),
                };
                let effects = session.handle(msg, &mut docs);
                for notify in docs.pending {
                    notify.dispatch();
                }
                if let Err(end) = apply_effects(&mut session, &mut sink, shared, effects).await {
                    return end;
                }
            }
        }
    }
}

/// Drain a session's effect list, including the follow-on effects produced by
/// the post-handshake subscriptions.
async fn apply_effects(
    session: &mut ClientSession,
    sink: &mut Sink,
    shared: &Arc<Shared>,
    effects: Vec<ClientEffect>,
) -> Result<(), ConnEnd> {
    let mut queue: VecDeque<ClientEffect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        match effect {
            ClientEffect::Send(msg) => send_msg(sink, &msg).await?,
            ClientEffect::Connected => {
                notify_sync_state_arc(shared, SyncState::Connected);
                for (doc, have) in open_docs(shared) {
                    queue.extend(session.subscribe(doc, have));
                }
            }
            ClientEffect::DocSynced(doc) => {
                if let Some(listener) = note_listener(shared, doc) {
                    listener.synced();
                }
            }
            ClientEffect::Ephemeral { doc, payload, .. } => dispatch_wet(shared, doc, &payload),
            ClientEffect::Fatal(message) => return Err(ConnEnd::Fatal(message)),
        }
    }
    Ok(())
}

async fn send_msg(sink: &mut Sink, msg: &ClientMsg) -> Result<(), ConnEnd> {
    let frame = msg
        .encode()
        .map_err(|err| ConnEnd::Fatal(format!("encoding frame: {err}")))?;
    sink.send(WsMessage::Binary(frame.into()))
        .await
        .map_err(|_| ConnEnd::Lost)
}

/// Workspace + every open note, with their current versions.
fn open_docs(shared: &Shared) -> Vec<(DocKey, Vec<u8>)> {
    let state = shared.lock_state();
    let mut docs = vec![(DocKey::WORKSPACE, state.workspace.version())];
    docs.extend(
        state
            .notes
            .iter()
            .map(|(id, note)| (DocKey::from(*id), note.doc.version())),
    );
    docs
}

fn notify_sync_state(shared: &Weak<Shared>, sync: SyncState) {
    if let Some(shared) = shared.upgrade() {
        notify_sync_state_arc(&shared, sync);
    }
}

fn notify_sync_state_arc(shared: &Shared, sync: SyncState) {
    let listener = shared.lock_state().core_listener.clone();
    if let Some(listener) = listener {
        listener.sync_state(sync);
    }
}

fn note_listener(shared: &Shared, doc: DocKey) -> Option<Arc<dyn NoteListener>> {
    let state = shared.lock_state();
    state
        .notes
        .iter()
        .find(|(id, _)| DocKey::from(**id) == doc)
        .and_then(|(_, note)| note.listener.clone())
}

fn dispatch_wet(shared: &Shared, doc: DocKey, payload: &[u8]) {
    let ink = match WetInk::decode(payload) {
        Ok(ink) => ink,
        Err(err) => {
            tracing::warn!("dropping malformed wet-ink payload: {err}");
            return;
        }
    };
    let Some(listener) = note_listener(shared, doc) else {
        return;
    };
    match ink {
        WetInk::Begin {
            sketch,
            stroke,
            tool,
            color,
            base_width,
        } => listener.wet_begin(
            sketch.to_string(),
            stroke.to_string(),
            tool.into(),
            rgba_to_u32(color),
            base_width,
        ),
        WetInk::Points {
            stroke,
            points,
            sent_ms,
            ..
        } => listener.wet_points(
            stroke.to_string(),
            sent_ms,
            points
                .iter()
                .map(|p| crate::types::WetPoint {
                    x: p.x,
                    y: p.y,
                    force: p.force,
                    width: p.width,
                })
                .collect(),
        ),
        WetInk::End { stroke, .. } => listener.wet_end(stroke.to_string()),
    }
}

/// A queued listener call, captured under the state lock, dispatched after it
/// is released.
enum Notify {
    Notes(Arc<dyn CoreListener>, Vec<NoteInfo>),
    Text(Arc<dyn NoteListener>, String),
    Strokes(Arc<dyn NoteListener>, String),
}

impl Notify {
    fn dispatch(self) {
        match self {
            Self::Notes(listener, notes) => listener.notes_changed(notes),
            Self::Text(listener, text) => listener.text_changed(text),
            Self::Strokes(listener, sketch) => listener.strokes_changed(sketch),
        }
    }
}

/// [`ClientDocs`] over the engine state: imports persist to the store and
/// queue listener notifications.
struct Docs<'a> {
    shared: &'a Shared,
    pending: Vec<Notify>,
}

impl ClientDocs for Docs<'_> {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> pendant_core::Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        let state = self.shared.lock_state();
        if doc == DocKey::WORKSPACE {
            state.workspace.import_update(payload)?;
            state.store.append_update(doc, payload, Flush::Eventual)?;
            if let Some(listener) = state.core_listener.clone() {
                let notes = state
                    .workspace
                    .notes()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                self.pending.push(Notify::Notes(listener, notes));
            }
            return Ok(());
        }
        let Some((_, note)) = state.notes.iter().find(|(id, _)| DocKey::from(**id) == doc) else {
            return Ok(()); // note was closed since we subscribed
        };
        let text_before = note.doc.text();
        let counts_before = sketch_counts(&note.doc);
        note.doc.import_update(payload)?;
        state.store.append_update(doc, payload, Flush::Eventual)?;
        if let Some(listener) = note.listener.clone() {
            let text_after = note.doc.text();
            if text_after != text_before {
                self.pending
                    .push(Notify::Text(listener.clone(), text_after));
            }
            // Strokes are only ever added/removed whole, so a per-sketch count
            // diff catches every change.
            let counts_after = sketch_counts(&note.doc);
            for (sketch, count) in &counts_after {
                if counts_before.get(sketch) != Some(count) {
                    self.pending
                        .push(Notify::Strokes(listener.clone(), sketch.to_string()));
                }
            }
            for sketch in counts_before.keys() {
                if !counts_after.contains_key(sketch) {
                    self.pending
                        .push(Notify::Strokes(listener.clone(), sketch.to_string()));
                }
            }
        }
        Ok(())
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> pendant_core::Result<Vec<u8>> {
        let state = self.shared.lock_state();
        if doc == DocKey::WORKSPACE {
            state.workspace.export_updates_since(have)
        } else {
            state
                .notes
                .iter()
                .find(|(id, _)| DocKey::from(**id) == doc)
                .map(|(_, note)| note.doc.export_updates_since(have))
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }
}

fn sketch_counts(
    doc: &pendant_core::NoteDoc,
) -> std::collections::HashMap<pendant_core::SketchId, usize> {
    doc.sketch_ids()
        .into_iter()
        .map(|sketch| {
            let count = doc.strokes(sketch).map(|s| s.len()).unwrap_or(0);
            (sketch, count)
        })
        .collect()
}
