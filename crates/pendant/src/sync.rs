//! Sync plugin: the app talks to its own node over the in-process link
//! with one sans-io [`ClientSession`], driven once per frame. The node
//! does everything else (peers, relay, fan-out), so the link never goes
//! down; a fatal session error just reopens it after a pause.

use bevy::prelude::*;
use pendant_core::{ClientEffect, ClientSession, DeviceId, DocKey, ServerMsg};
use pendant_local::{LocalLink, Node};

use crate::docs::Docs;
use crate::sketch::WetInkFrame;
use crate::ui::EditorState;

/// Raised by the UI when a doc needs subscription (note opened).
#[derive(Message)]
pub struct SubscribeNeeded(pub DocKey);

/// Raised whenever a local CRDT commit produced a sync payload.
#[derive(Message)]
pub struct LocalCommit {
    pub doc: DocKey,
    pub payload: Vec<u8>,
}

/// After a fatal session error the link is reopened this much later: a
/// fresh handshake re-subscribes and backfills, which is the recovery for
/// a bad import; the pause keeps a repeating failure from spinning.
const REOPEN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Resource)]
pub struct SyncTransport {
    node: Node,
    link: LocalLink,
    session: Option<ClientSession>,
    token: String,
    reopen_at: Option<std::time::Instant>,
}

impl SyncTransport {
    /// Open the link; needs the runtime entered (spawns the node side).
    pub fn new(node: Node, token: String) -> Self {
        let link = node.local_link();
        Self {
            node,
            link,
            session: None,
            token,
            reopen_at: None,
        }
    }

    pub fn device(&self) -> DeviceId {
        self.node.device()
    }

    pub fn connected(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_ready())
    }

    fn send(&self, frame: Vec<u8>) {
        self.link.send(frame);
    }
}

/// How this machine announces itself in the synced device registry.
pub fn local_device_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

pub const LOCAL_PLATFORM: &str = std::env::consts::OS;

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
    let device = transport.device();

    // 0. Handshake (first frame), or reopen after a fatal error.
    if transport.session.is_none() {
        let due = transport
            .reopen_at
            .is_none_or(|at| at <= std::time::Instant::now());
        if !due {
            return;
        }
        if transport.reopen_at.take().is_some() {
            tracing::info!("reopening local sync link after fatal error");
            let _guard = runtime.0.enter();
            transport.link = transport.node.local_link();
        }
        let mut session = ClientSession::new(device, transport.token.clone());
        let effects = session.connect();
        transport.session = Some(session);
        apply_effects(
            effects,
            &mut transport,
            device,
            &mut docs,
            &mut editor,
            &mut wet,
        );
    }

    // 1. Local commits out.
    for commit in commits.read() {
        if let Some(session) = transport.session.as_mut() {
            let effects = session.local_update(commit.doc, commit.payload.clone());
            apply_effects(
                effects,
                &mut transport,
                device,
                &mut docs,
                &mut editor,
                &mut wet,
            );
        }
    }

    // 2. New subscriptions.
    for SubscribeNeeded(doc) in subscribes.read() {
        if transport.connected() {
            let have = docs.version_of(*doc);
            let session = transport.session.as_mut().expect("checked above");
            let effects = session.subscribe(*doc, have);
            apply_effects(
                effects,
                &mut transport,
                device,
                &mut docs,
                &mut editor,
                &mut wet,
            );
        }
    }

    // 3. Frames in.
    while let Ok(frame) = transport.link.from_node.try_recv() {
        let Some(mut session) = transport.session.take() else {
            break;
        };
        let msg = match ServerMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(%err, "bad frame from node");
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
        apply_effects(
            effects,
            &mut transport,
            device,
            &mut docs,
            &mut editor,
            &mut wet,
        );
    }
}

fn apply_effects(
    effects: Vec<ClientEffect>,
    transport: &mut SyncTransport,
    device: DeviceId,
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
                    apply_effects(effects, transport, device, docs, editor, wet);
                }
                // Announce this device in the synced registry so peers
                // (e.g. the iPad's settings screen) can list it.
                match docs.register_device(device, &local_device_name(), LOCAL_PLATFORM) {
                    Ok(payload) if !payload.is_empty() => {
                        let session = transport.session.as_mut().expect("just connected");
                        let effects = session.local_update(DocKey::WORKSPACE, payload);
                        apply_effects(effects, transport, device, docs, editor, wet);
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
                tracing::error!(%err, "sync session failed; reopening link shortly");
                transport.session = None;
                transport.reopen_at = Some(std::time::Instant::now() + REOPEN_AFTER);
            }
        }
    }
}
