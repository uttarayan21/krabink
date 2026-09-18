//! Sync plugin: one tokio task per relay link owns its WebSocket (with
//! reconnect + backoff) and shuttles raw frames over channels; the Bevy
//! side owns the docs and one sans-io [`ClientSession`] per link, driven
//! once per frame.
//!
//! A desktop always has at least one link — its own embedded relay — and
//! optionally a dedicated relay. Updates arriving over one link are
//! re-sent over every other ready link, so the desktop bridges the two
//! paths and peers on either side converge without talking to each other.

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
/// A black-holed address (wrong network) must not stall the link for the
/// OS's TCP timeout.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Which relay a link talks to; the embedded one is always first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Embedded,
    Remote,
}

/// After a fatal session error the link is torn down and reopened this
/// much later: a fresh handshake re-subscribes and backfills, which is
/// the recovery for a bad import; the pause keeps a repeating failure
/// from spinning.
const REOPEN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// One relay connection: socket task on the runtime, session on this side.
struct Link {
    kind: LinkKind,
    server: String,
    token: String,
    inbound: mpsc::UnboundedReceiver<TransportEvent>,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    session: Option<ClientSession>,
    /// Set by a fatal session error; the link is reopened once due.
    reopen_at: Option<std::time::Instant>,
}

impl Link {
    fn open(
        runtime: &tokio::runtime::Handle,
        kind: LinkKind,
        server: String,
        token: String,
    ) -> Self {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        runtime.spawn(transport_task(server.clone(), token.clone(), in_tx, out_rx));
        Self {
            kind,
            server,
            token,
            inbound: in_rx,
            outbound: out_tx,
            session: None,
            reopen_at: None,
        }
    }

    /// Replace the socket task with a fresh one (the old task ends when its
    /// channels drop, closing the socket) and start over from the handshake.
    fn reopen(&mut self, runtime: &tokio::runtime::Handle) {
        *self = Link::open(runtime, self.kind, self.server.clone(), self.token.clone());
    }

    fn send(&self, frame: Vec<u8>) {
        let _ = self.outbound.send(frame);
    }

    fn status(&self) -> SyncStatus {
        if self.session.as_ref().is_some_and(|s| s.is_ready()) {
            SyncStatus::Connected
        } else {
            SyncStatus::Connecting
        }
    }
}

/// Snapshot of one link for the settings screen.
pub struct LinkStatus {
    pub kind: LinkKind,
    pub server: String,
    pub status: SyncStatus,
}

#[derive(Resource)]
pub struct SyncTransport {
    links: Vec<Link>,
    device: DeviceId,
}

impl SyncTransport {
    pub fn new(device: DeviceId) -> Self {
        Self {
            links: Vec::new(),
            device,
        }
    }

    /// Add a relay; the socket task starts connecting immediately.
    pub fn add_link(
        &mut self,
        runtime: &tokio::runtime::Handle,
        kind: LinkKind,
        server: String,
        token: String,
    ) {
        self.links.push(Link::open(runtime, kind, server, token));
    }

    /// Drop every remote link (their socket tasks end when the channels
    /// close) and connect to `servers` instead. The embedded link stays.
    pub fn replace_remotes(
        &mut self,
        runtime: &tokio::runtime::Handle,
        servers: impl IntoIterator<Item = String>,
        token: &str,
    ) {
        self.links.retain(|l| l.kind == LinkKind::Embedded);
        for server in servers {
            self.add_link(runtime, LinkKind::Remote, server, token.to_string());
        }
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    pub fn links(&self) -> Vec<LinkStatus> {
        self.links
            .iter()
            .map(|l| LinkStatus {
                kind: l.kind,
                server: l.server.clone(),
                status: l.status(),
            })
            .collect()
    }
}

/// Coarse per-link connection state for the settings screen. There is no
/// "offline": the embedded relay link always exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    /// Socket down or handshake pending.
    Connecting,
    Connected,
}

/// How this machine announces itself in the synced device registry.
pub fn local_device_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

pub const LOCAL_PLATFORM: &str = std::env::consts::OS;

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

        let attempt =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| {
                    tokio_tungstenite::tungstenite::Error::Io(std::io::ErrorKind::TimedOut.into())
                });
        match attempt.and_then(|r| r) {
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
        // A dropped link (channel closed) must not linger through the backoff.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = inbound.closed() => return,
        }
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
    runtime: Res<crate::Runtime>,
    mut transport: ResMut<SyncTransport>,
    mut docs: ResMut<Docs>,
    mut editor: ResMut<EditorState>,
    mut subscribes: MessageReader<SubscribeNeeded>,
    mut commits: MessageReader<LocalCommit>,
    mut wet: MessageWriter<WetInkFrame>,
) {
    let device = transport.device;

    // 0. Links whose session died fatally come back after a pause.
    let now = std::time::Instant::now();
    for link in &mut transport.links {
        if link.reopen_at.is_some_and(|at| at <= now) {
            tracing::info!(
                server = link.server,
                "reopening sync link after fatal error"
            );
            link.reopen(runtime.0.handle());
        }
    }

    // 1. Local commits out, on every link.
    for commit in commits.read() {
        for link in &mut transport.links {
            if let Some(session) = link.session.as_mut() {
                let effects = session.local_update(commit.doc, commit.payload.clone());
                apply_effects(effects, link, device, &mut docs, &mut editor, &mut wet);
            }
        }
    }

    // 2. New subscriptions, on every ready link.
    for SubscribeNeeded(doc) in subscribes.read() {
        for link in &mut transport.links {
            let ready = link.session.as_ref().is_some_and(|s| s.is_ready());
            if ready {
                let have = docs.version_of(*doc);
                let session = link.session.as_mut().expect("checked above");
                let effects = session.subscribe(*doc, have);
                apply_effects(effects, link, device, &mut docs, &mut editor, &mut wet);
            }
        }
    }

    // 3. Transport events in; remote updates are bridged to the other links.
    let mut bridged: Vec<(usize, DocKey, Vec<u8>)> = Vec::new();
    for (index, link) in transport.links.iter_mut().enumerate() {
        while let Ok(event) = link.inbound.try_recv() {
            match event {
                TransportEvent::Up => {
                    let mut session = ClientSession::new(device, link.token.clone());
                    let effects = session.connect();
                    link.session = Some(session);
                    apply_effects(effects, link, device, &mut docs, &mut editor, &mut wet);
                }
                TransportEvent::Down => {
                    link.session = None;
                }
                TransportEvent::Frame(frame) => {
                    let Some(mut session) = link.session.take() else {
                        continue;
                    };
                    let msg = match ServerMsg::decode(&frame) {
                        Ok(msg) => msg,
                        Err(err) => {
                            tracing::warn!(%err, server = link.server, "bad frame from server");
                            link.session = Some(session);
                            continue;
                        }
                    };
                    match &msg {
                        ServerMsg::Update { doc, payload, .. } => {
                            editor.remote_dirty = true; // handle() imports into the docs
                            bridged.push((index, *doc, payload.clone()));
                        }
                        ServerMsg::SubscribeAck { doc, catch_up, .. } => {
                            editor.remote_dirty = true;
                            if !catch_up.is_empty() {
                                bridged.push((index, *doc, catch_up.clone()));
                            }
                        }
                        _ => {}
                    }
                    let effects = session.handle(msg, &mut *docs);
                    link.session = Some(session);
                    apply_effects(effects, link, device, &mut docs, &mut editor, &mut wet);
                }
            }
        }
    }

    // 4. Bridge: what one relay told us, every other relay hears too.
    // Loro imports are idempotent, so a relay that already has the update
    // (because it was the origin) simply ignores it.
    for (from, doc, payload) in bridged {
        for (index, link) in transport.links.iter_mut().enumerate() {
            if index == from {
                continue;
            }
            if let Some(session) = link.session.as_mut() {
                let effects = session.local_update(doc, payload.clone());
                apply_effects(effects, link, device, &mut docs, &mut editor, &mut wet);
            }
        }
    }
}

fn apply_effects(
    effects: Vec<ClientEffect>,
    link: &mut Link,
    device: DeviceId,
    docs: &mut Docs,
    editor: &mut EditorState,
    wet: &mut MessageWriter<WetInkFrame>,
) {
    for effect in effects {
        match effect {
            ClientEffect::Send(msg) => match msg.encode() {
                Ok(frame) => link.send(frame),
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
                    let session = link.session.as_mut().expect("just connected");
                    let effects = session.subscribe(doc, have);
                    apply_effects(effects, link, device, docs, editor, wet);
                }
                // Announce this device in the synced registry so peers
                // (e.g. the iPad's settings screen) can list it.
                match docs.register_device(device, &local_device_name(), LOCAL_PLATFORM) {
                    Ok(payload) if !payload.is_empty() => {
                        let session = link.session.as_mut().expect("just connected");
                        let effects = session.local_update(DocKey::WORKSPACE, payload);
                        apply_effects(effects, link, device, docs, editor, wet);
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(%err, "device registration failed"),
                }
            }
            ClientEffect::DocSynced(_) => {
                editor.remote_dirty = true;
            }
            ClientEffect::Ephemeral { doc, payload, .. } => {
                wet.write(WetInkFrame { doc, payload });
            }
            ClientEffect::Fatal(err) => {
                tracing::error!(%err, server = link.server, "sync session failed; reopening link shortly");
                link.session = None;
                link.reopen_at = Some(std::time::Instant::now() + REOPEN_AFTER);
            }
        }
    }
}
