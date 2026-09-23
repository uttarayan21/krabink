//! Background tasks behind [`crate::Core`]: the app's sans-io
//! [`ClientSession`] over the node's in-process link, and the sync-state
//! aggregator. The node itself (peers, relay, fan-out) lives in
//! `pendant-local`; this file only bridges it to the FFI listeners.
//!
//! One session task per [`crate::Core`], driven by [`Cmd`]s — the FFI
//! mutators never block on the network. The link never goes down on its
//! own; a fatal session error (bad import) reopens it after a pause so a
//! fresh handshake re-subscribes and backfills.

use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pendant_core::{
    ClientDocs, ClientEffect, ClientMsg, ClientSession, DocKey, Flush, ServerMsg, WetInk,
};
use pendant_local::{EndpointId, LocalLink, Node, PeerKind, PeerState, Route};
use tokio::sync::mpsc;

use crate::brush::AssetInfo;
use crate::engine::{CoreListener, NoteListener, Shared};
use crate::types::{BrushInfo, DeviceInfo, NoteInfo, SyncState, rgba_to_u32};

pub(crate) enum Cmd {
    Subscribe(DocKey),
    Update { doc: DocKey, payload: Vec<u8> },
    Ephemeral { doc: DocKey, payload: Vec<u8> },
}

/// After a fatal session error the link is reopened this much later.
const REOPEN_AFTER: Duration = Duration::from_secs(5);

/// How a link session ended.
enum End {
    /// The node side closed the link (only on shutdown).
    Lost,
    /// The session hit a fatal error (bad frame, failed import).
    Fatal(String),
    /// The `Core` was dropped.
    Shutdown,
}

/// The app's session over the local link, reopened forever.
pub(crate) async fn run(shared: Weak<Shared>, node: Node, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let link = node.local_link();
        let end = session(&strong, &mut rx, link).await;
        drop(strong);
        match end {
            End::Shutdown => return,
            End::Fatal(message) => {
                tracing::error!(%message, "local sync session failed; reopening shortly");
                tokio::time::sleep(REOPEN_AFTER).await;
            }
            End::Lost => {
                tracing::warn!("local sync link closed; reopening");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn session(
    shared: &Arc<Shared>,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    mut link: LocalLink,
) -> End {
    let mut session = ClientSession::new(shared.device, shared.token.clone());
    let effects = session.connect();
    if let Err(end) = apply_effects(&mut session, &link, shared, effects) {
        return end;
    }
    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let effects = match cmd {
                    None => return End::Shutdown,
                    // Not ready yet: drop it — the post-handshake enumeration
                    // of open notes covers every doc present in the map, and
                    // unsubscribed updates are re-derived on catch-up.
                    Some(Cmd::Subscribe(doc)) if session.is_ready() => {
                        session.subscribe(doc, shared.doc_version(doc))
                    }
                    Some(Cmd::Subscribe(_)) => Vec::new(),
                    Some(Cmd::Update { doc, payload }) => session.local_update(doc, payload),
                    Some(Cmd::Ephemeral { doc, payload }) => session.ephemeral(doc, payload),
                };
                if let Err(end) = apply_effects(&mut session, &link, shared, effects) {
                    return end;
                }
            }
            frame = link.from_node.recv() => {
                let Some(bytes) = frame else { return End::Lost };
                let msg = match ServerMsg::decode(&bytes) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!("dropping malformed frame from node: {err}");
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
                if let Err(end) = apply_effects(&mut session, &link, shared, effects) {
                    return end;
                }
            }
        }
    }
}

/// Drain a session's effect list, including the follow-on effects produced by
/// the post-handshake subscriptions.
fn apply_effects(
    session: &mut ClientSession,
    link: &LocalLink,
    shared: &Arc<Shared>,
    effects: Vec<ClientEffect>,
) -> Result<(), End> {
    let mut queue: VecDeque<ClientEffect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        match effect {
            ClientEffect::Send(msg) => send_msg(link, &msg)?,
            ClientEffect::Connected => {
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
            ClientEffect::Fatal(message) => return Err(End::Fatal(message)),
            // The app talks only to its own node over the local link, which
            // never sends an unpair; nothing to do here.
            ClientEffect::Unpaired(_) => {}
        }
    }
    Ok(())
}

fn send_msg(link: &LocalLink, msg: &ClientMsg) -> Result<(), End> {
    let frame = msg
        .encode()
        .map_err(|err| End::Fatal(format!("encoding frame: {err}")))?;
    link.send(frame);
    Ok(())
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

/// Report the aggregate [`SyncState`] whenever the node's peers, its relay
/// health, or the app's online flag change.
pub(crate) async fn watch_status(shared: Weak<Shared>, node: Node) {
    let mut peers = node.watch_peers();
    let mut relay = node.watch_relay();
    let mut last: Option<SyncState> = None;
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let state = sync_state(&strong, &node);
        if last.as_ref() != Some(&state) {
            let listener = strong.lock_state().core_listener.clone();
            if let Some(listener) = listener {
                listener.sync_state(state.clone());
            }
            last = Some(state);
        }
        drop(strong);
        tokio::select! {
            changed = peers.changed() => if changed.is_err() { return },
            changed = relay.changed() => if changed.is_err() { return },
            _ = shared_poke(&shared) => {}
        }
    }
}

/// When a peer tells us it unpaired, and it is the device our pairing
/// points at, we have been evicted: forget the pairing so we stop dialling
/// and the UI shows "not paired". Notes already synced stay put.
pub(crate) async fn watch_unpaired(shared: Weak<Shared>, node: Node) {
    let mut events = node.watch_unpaired();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            // Lagged: we only care about the latest state, keep going.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let Some(strong) = shared.upgrade() else {
            return;
        };
        // Is the unpairing peer the node our pairing points at?
        let paired_to = strong
            .lock_state()
            .pairing
            .as_ref()
            .and_then(|p| p.node.parse::<EndpointId>().ok());
        let evicted = matches!((event.endpoint, paired_to), (Some(a), Some(b)) if a == b);
        if !evicted {
            continue;
        }
        // Drop the pairing and the remover's row; stop dialling.
        let previous = strong.lock_state().pairing.take();
        strong
            .pairing_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let payload = {
            let mut state = strong.lock_state();
            crate::engine::commit_workspace(&mut state, |ws| ws.remove_device(event.device)).ok()
        };
        if let Some(Some(payload)) = payload {
            let _ = strong.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        if let Some(previous) = &previous {
            node.remove_token(&previous.token);
        }
        node.set_peers(Vec::new()).await;
        if let Err(err) = node.set_relay(None).await {
            tracing::warn!(%err, "dropping relay after eviction failed");
        }
        // Push the trimmed device list and refreshed sync state.
        let (listener, devices) = {
            let state = strong.lock_state();
            (
                state.core_listener.clone(),
                state
                    .workspace
                    .devices()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            )
        };
        if let Some(listener) = listener {
            listener.devices_changed(devices);
        }
        strong.poke.notify_one();
    }
}

async fn shared_poke(shared: &Weak<Shared>) {
    match shared.upgrade() {
        Some(strong) => strong.poke.notified().await,
        None => std::future::pending().await,
    }
}

/// Best peer wins: direct beats relay beats "connected, path unknown";
/// then a rejection, then any dial in progress.
pub(crate) fn sync_state(shared: &Shared, node: &Node) -> SyncState {
    if !shared.online.load(std::sync::atomic::Ordering::Acquire) {
        return SyncState::Disconnected;
    }
    let peers: Vec<_> = node
        .peers()
        .into_iter()
        .filter(|p| p.kind != PeerKind::Local)
        .collect();
    let rank = |route: &Option<Route>| match route {
        Some(Route::Direct(_)) => 0,
        Some(Route::Relay(_)) => 1,
        None => 2,
    };
    let best = peers
        .iter()
        .filter_map(|p| match &p.state {
            PeerState::Connected { route } => Some((rank(route), p, route.clone())),
            _ => None,
        })
        .min_by_key(|(rank, ..)| *rank);
    if let Some((_, peer, route)) = best {
        return SyncState::Connected {
            peer: peer.id.map(|id| id.to_string()).unwrap_or_default(),
            route: route.map(Into::into),
        };
    }
    if let Some(message) = peers.iter().find_map(|p| match &p.state {
        PeerState::Fatal { message } => Some(message.clone()),
        _ => None,
    }) {
        return SyncState::Fatal { message };
    }
    let relay = node.relay_health();
    if let Some(error) = relay.error {
        return SyncState::Fatal {
            message: format!("relay: {error}"),
        };
    }
    if peers.is_empty() {
        SyncState::Disconnected
    } else {
        SyncState::Connecting
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
            spec,
        } => listener.wet_begin(
            sketch.to_string(),
            stroke.to_string(),
            tool.into(),
            rgba_to_u32(color),
            base_width,
            spec,
        ),
        WetInk::Points {
            stroke, sent_ms, ..
        }
        | WetInk::End {
            stroke, sent_ms, ..
        } => {
            match ink.decode_points() {
                Ok(points) if !points.is_empty() => listener.wet_points(
                    stroke.to_string(),
                    sent_ms,
                    points.into_iter().map(Into::into).collect(),
                ),
                Ok(_) => {}
                Err(err) => tracing::warn!("dropping undecodable wet-ink points: {err}"),
            }
            if matches!(ink, WetInk::End { .. }) {
                listener.wet_end(stroke.to_string());
            }
        }
        WetInk::Cancel { stroke } => listener.wet_cancel(stroke.to_string()),
        // Remote pointers are rendered on the desktop only for now.
        WetInk::Pointer { .. } | WetInk::PointerGone { .. } => {}
    }
}

/// A queued listener call, captured under the state lock, dispatched after it
/// is released.
enum Notify {
    Notes(Arc<dyn CoreListener>, Vec<NoteInfo>),
    Brushes(Arc<dyn CoreListener>, Vec<BrushInfo>),
    Assets(Arc<dyn CoreListener>, Vec<AssetInfo>),
    Devices(Arc<dyn CoreListener>, Vec<DeviceInfo>),
    Text(Arc<dyn NoteListener>, String),
    Strokes(Arc<dyn NoteListener>, String),
}

impl Notify {
    fn dispatch(self) {
        match self {
            Self::Notes(listener, notes) => listener.notes_changed(notes),
            Self::Brushes(listener, brushes) => listener.brushes_changed(brushes),
            Self::Assets(listener, assets) => listener.assets_changed(assets),
            Self::Devices(listener, devices) => listener.devices_changed(devices),
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
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> pendant_core::Result<bool> {
        if payload.is_empty() {
            return Ok(false);
        }
        let state = self.shared.lock_state();
        if doc == DocKey::WORKSPACE {
            let changed = state.workspace.import_update(payload)?;
            state.store.append_update(doc, payload, Flush::Eventual)?;
            if let Some(listener) = state.core_listener.clone() {
                let notes = state
                    .workspace
                    .notes()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                self.pending.push(Notify::Notes(listener.clone(), notes));
                let brushes = state
                    .workspace
                    .brushes()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                self.pending
                    .push(Notify::Brushes(listener.clone(), brushes));
                let assets = state
                    .workspace
                    .assets()
                    .into_iter()
                    .map(|a| a.asset.into())
                    .collect();
                self.pending.push(Notify::Assets(listener.clone(), assets));
                let devices = state
                    .workspace
                    .devices()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                self.pending.push(Notify::Devices(listener, devices));
            }
            return Ok(changed);
        }
        let Some((_, note)) = state.notes.iter().find(|(id, _)| DocKey::from(**id) == doc) else {
            return Ok(false); // note was closed since we subscribed
        };
        let text_before = note.doc.text();
        let counts_before = sketch_counts(&note.doc);
        let changed = note.doc.import_update(payload)?;
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
        Ok(changed)
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
            let count = doc.elements(sketch).map(|s| s.len()).unwrap_or(0);
            (sketch, count)
        })
        .collect()
}
