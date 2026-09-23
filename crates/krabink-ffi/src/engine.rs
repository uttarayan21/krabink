//! The FFI client engine: shared state behind [`Core`]/[`NoteSession`], with
//! every mutator following the same shape — mutate the CRDT under the state
//! lock, persist the delta, hand it to the session task. Listener callbacks
//! are always invoked with the state lock released, so a listener may call
//! straight back into `Core`/`NoteSession` without deadlocking.
//!
//! Networking is the device's [`Node`] (`krabink-local`): it accepts peers,
//! dials the paired desktop and the workspace replica, and fans updates
//! out. The app talks to it over the in-process link like any other peer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use krabink_core as pcore;
use krabink_core::{DeviceId, DocKey, Flush, NoteId, NoteMeta, SketchId, Store, WorkspaceDoc};
use krabink_local::{Node, NodeConfig, PeerKind, PeerTarget, RelayTarget, Role, direct_addrs};
use tokio::sync::mpsc;

use crate::brush::{AssetInfo, AssetKind};
use crate::net::{self, Cmd};
use crate::types::{
    BrushInfo, DeviceInfo, Element, NoteInfo, PairInfo, PeerInfo, ShapeElement, Stroke,
    StrokePoint, SyncState, Tilt, Tool, rgba_from_u32,
};

/// Errors crossing the FFI boundary. Flattened to message-carrying variants;
/// Swift rarely needs more than "which kind" + a human-readable cause.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum KrabinkError {
    #[error("malformed id: {id}")]
    MalformedId { id: String },
    #[error("unknown note: {id}")]
    UnknownNote { id: String },
    #[error("bad pairing: {message}")]
    BadPairing { message: String },
    #[error("{message}")]
    Internal { message: String },
}

impl From<pcore::Error> for KrabinkError {
    fn from(err: pcore::Error) -> Self {
        Self::Internal {
            message: err.to_string(),
        }
    }
}

pub type Result<T, E = KrabinkError> = core::result::Result<T, E>;

/// Library-level events: note registry changes and sync connection state.
#[uniffi::export(foreign)]
pub trait CoreListener: Send + Sync {
    fn notes_changed(&self, notes: Vec<NoteInfo>);
    /// The shared brush library changed (a peer added, edited or removed
    /// a brush); the full list, newest edit first.
    fn brushes_changed(&self, brushes: Vec<BrushInfo>);
    /// The workspace's image assets changed; the full list with bytes,
    /// newest first. Renderers upload what they lack and redraw strokes
    /// that were waiting for it.
    fn assets_changed(&self, assets: Vec<AssetInfo>);
    /// The synced device registry changed (a peer joined, was renamed or
    /// removed); the full list, most recently seen first.
    fn devices_changed(&self, devices: Vec<DeviceInfo>);
    fn sync_state(&self, state: SyncState);
}

/// Per-note events. Text and element changes are coarse: re-read via
/// [`NoteSession::text`] / [`NoteSession::elements`]. Wet-ink events mirror
/// the ephemeral stream and never touch the CRDT; render them provisionally
/// and drop the overlay when `strokes_changed` delivers the committed
/// element (a stroke or a snapped shape) under the same id.
#[uniffi::export(foreign)]
pub trait NoteListener: Send + Sync {
    /// Catch-up with the server finished; local edits now propagate live.
    /// Fires once per (re)connection.
    fn synced(&self);
    fn text_changed(&self, text: String);
    fn strokes_changed(&self, sketch: String);
    /// `spec` is a custom brush's encoded spec (pass it as
    /// `BrushRef.custom` with any id to mesh the wet points); `None` for
    /// the tool's preset.
    fn wet_begin(
        &self,
        sketch: String,
        stroke: String,
        tool: Tool,
        color: u32,
        base_width: f32,
        spec: Option<Vec<u8>>,
    );
    /// Stored points the sender emitted since its last batch; fold them
    /// through `points_mesh` with `StrokeEnd.live`.
    /// `sent_ms` is the sender's unix-millis clock when the batch left the
    /// pen — latency telemetry only, meaningless across skewed clocks.
    fn wet_points(&self, stroke: String, sent_ms: u64, points: Vec<StrokePoint>);
    fn wet_end(&self, stroke: String);
    /// No stroke will follow: drop the provisional ink immediately.
    fn wet_cancel(&self, stroke: String);
}

pub(crate) struct OpenNote {
    pub doc: pcore::NoteDoc,
    pub listener: Option<Arc<dyn NoteListener>>,
}

pub(crate) struct State {
    pub store: Store,
    pub workspace: WorkspaceDoc,
    pub notes: HashMap<NoteId, OpenNote>,
    pub core_listener: Option<Arc<dyn CoreListener>>,
    /// The workspace adopted from a pairing URI, if any.
    pub pairing: Option<pcore::PairInfo>,
}

/// Everything shared between the FFI objects and the background tasks.
pub(crate) struct Shared {
    pub device: DeviceId,
    /// Per-install secret the node always accepts; what the app's own link
    /// and, until a workspace is adopted, its QR use.
    pub token: String,
    pub state: Mutex<State>,
    pub cmd: mpsc::UnboundedSender<Cmd>,
    /// False between `suspend` and `connect`.
    pub online: AtomicBool,
    /// Wakes the sync-state watcher after `suspend`/`connect`/`set_pairing`.
    pub poke: tokio::sync::Notify,
    /// Bumped by every `set_pairing`/`unpair`; the delayed teardown after
    /// an unpair checks it so a quick re-pair is not torn down.
    pub pairing_gen: AtomicU64,
}

impl Shared {
    pub fn lock_state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means a panic mid-mutation; the CRDT itself is
        // never left half-applied (loro commits are atomic), so continuing
        // beats poisoning every future call.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current encoded version of a doc, for subscribe catch-up.
    pub fn doc_version(&self, doc: DocKey) -> Vec<u8> {
        let state = self.lock_state();
        if doc == DocKey::WORKSPACE {
            state.workspace.version()
        } else {
            state
                .notes
                .iter()
                .find(|(id, _)| DocKey::from(**id) == doc)
                .map(|(_, note)| note.doc.version())
                .unwrap_or_default()
        }
    }
}

/// How long an unpair keeps the old peers up so the registry removal
/// reaches them before their connections are dropped.
const UNPAIR_LINGER: std::time::Duration = std::time::Duration::from_millis(750);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Stable per-install random secret (a ULID carries 80 random bits).
fn load_or_create_token(dir: &Path) -> Result<String> {
    let path = dir.join("workspace_token");
    if let Ok(text) = std::fs::read_to_string(&path)
        && !text.trim().is_empty()
    {
        return Ok(text.trim().to_string());
    }
    let token = ulid::Ulid::new().to_string();
    std::fs::write(&path, &token).map_err(|err| KrabinkError::Internal {
        message: format!("writing {}: {err}", path.display()),
    })?;
    Ok(token)
}

fn load_or_create_device_id(dir: &Path) -> Result<DeviceId> {
    let path = dir.join("device_id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return text
            .trim()
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: text });
    }
    let id = DeviceId::new();
    std::fs::write(&path, id.to_string()).map_err(|err| KrabinkError::Internal {
        message: format!("writing {}: {err}", path.display()),
    })?;
    Ok(id)
}

/// The krabink client: local store + note registry + the device's sync node.
#[derive(uniffi::Object)]
pub struct Core {
    shared: Arc<Shared>,
    node: Node,
    // Drives the node and the session task; dropped (and shut down) with
    // the last Core ref.
    runtime: tokio::runtime::Runtime,
}

impl Core {
    /// Run a node call to completion from an FFI thread.
    fn block_on<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(fut)
    }

    fn internal(err: impl std::fmt::Display) -> KrabinkError {
        KrabinkError::Internal {
            message: err.to_string(),
        }
    }

    /// Adopted workspace token when paired, else our own.
    fn pair_token(&self) -> String {
        let state = self.shared.lock_state();
        state
            .pairing
            .as_ref()
            .map(|p| p.token.clone())
            .unwrap_or_else(|| self.shared.token.clone())
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Final checkpoint and endpoint close. Never on a runtime thread
        // (block_on would panic); a Swift release always comes from one of
        // its own threads.
        if tokio::runtime::Handle::try_current().is_err()
            && let Err(err) = self.runtime.block_on(self.node.shutdown())
        {
            tracing::warn!(%err, "node shutdown failed");
        }
    }
}

#[uniffi::export]
impl Core {
    /// Open (or initialise) the app data directory: `krabink.redb` store,
    /// the node's `node.redb` mirror and `node_key`, plus a stable
    /// per-install `device_id` and `workspace_token`. The node binds
    /// immediately so this device can be dialled (its QR is valid) before
    /// it is paired with anything.
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Arc<Self>> {
        let dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&dir).map_err(|err| KrabinkError::Internal {
            message: format!("creating {}: {err}", dir.display()),
        })?;
        let store = Store::open(&dir.join("krabink.redb"))?;
        let device = load_or_create_device_id(&dir)?;
        let token = load_or_create_token(&dir)?;

        let stored = store.load(DocKey::WORKSPACE)?;
        let workspace = WorkspaceDoc::from_bytes(
            stored.snapshot.as_deref(),
            stored.updates.iter().map(Vec::as_slice),
        )?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| KrabinkError::Internal {
                message: format!("starting runtime: {err}"),
            })?;
        let node = runtime
            .block_on(Node::start(NodeConfig {
                key_path: dir.join("node_key"),
                store_path: dir.join("node.redb"),
                device,
                tokens: vec![token.clone()],
                relay: None,
                bind_port: None,
                role: Role::Device,
            }))
            .map_err(Self::internal)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            device,
            token,
            state: Mutex::new(State {
                store,
                workspace,
                notes: HashMap::new(),
                core_listener: None,
                pairing: None,
            }),
            cmd: tx,
            online: AtomicBool::new(true),
            poke: tokio::sync::Notify::new(),
            pairing_gen: AtomicU64::new(0),
        });
        runtime.spawn(net::run(Arc::downgrade(&shared), node.clone(), rx));
        runtime.spawn(net::watch_status(Arc::downgrade(&shared), node.clone()));
        runtime.spawn(net::watch_unpaired(Arc::downgrade(&shared), node.clone()));
        Ok(Arc::new(Self {
            shared,
            node,
            runtime,
        }))
    }

    pub fn set_listener(&self, listener: Arc<dyn CoreListener>) {
        self.shared.lock_state().core_listener = Some(listener);
    }

    /// All notes, newest edit first.
    pub fn list_notes(&self) -> Vec<NoteInfo> {
        let state = self.shared.lock_state();
        state
            .workspace
            .notes()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    pub fn create_note(self: Arc<Self>, title: String) -> Result<Arc<NoteSession>> {
        let id = NoteId::new();
        let key = DocKey::from(id);
        let doc = pcore::NoteDoc::new(id);
        doc.set_title(&title)?;
        {
            let mut state = self.shared.lock_state();
            let payload = doc.export_updates_since(&[])?;
            state.store.append_update(key, &payload, Flush::Immediate)?;
            state.notes.insert(
                id,
                OpenNote {
                    doc,
                    listener: None,
                },
            );
            commit_workspace(&mut state, |ws| {
                ws.upsert(&NoteMeta {
                    id,
                    title,
                    archived: false,
                    updated_ms: now_ms(),
                })
            })?
            .map(|payload| {
                self.shared.cmd.send(Cmd::Update {
                    doc: DocKey::WORKSPACE,
                    payload,
                })
            });
            let _ = self.shared.cmd.send(Cmd::Subscribe(key));
        }
        Ok(Arc::new(NoteSession {
            shared: self.shared.clone(),
            id,
            key,
        }))
    }

    /// Open a note from the local store (empty if unknown locally — the
    /// server backfills it on subscribe).
    pub fn open_note(self: Arc<Self>, id: String) -> Result<Arc<NoteSession>> {
        let note_id: NoteId = id.parse().map_err(|_| KrabinkError::MalformedId { id })?;
        let key = DocKey::from(note_id);
        {
            let mut state = self.shared.lock_state();
            if !state.notes.contains_key(&note_id) {
                let stored = state.store.load(key)?;
                let doc = pcore::NoteDoc::from_bytes(
                    note_id,
                    stored.snapshot.as_deref(),
                    stored.updates.iter().map(Vec::as_slice),
                )?;
                state.notes.insert(
                    note_id,
                    OpenNote {
                        doc,
                        listener: None,
                    },
                );
            }
        }
        let _ = self.shared.cmd.send(Cmd::Subscribe(key));
        Ok(Arc::new(NoteSession {
            shared: self.shared.clone(),
            id: note_id,
            key,
        }))
    }

    /// Adopt a workspace from a scanned/opened pairing URI: accept its
    /// token, use its relay, dial the node that showed the QR and (when
    /// named) the workspace replica. The node keeps dialling with backoff
    /// until [`Core::suspend`]; the relay is used only when no direct
    /// path can be punched.
    pub fn set_pairing(&self, info: PairInfo) -> Result<()> {
        let info = pcore::PairInfo::from(info);
        let bad = |message: String| KrabinkError::BadPairing { message };
        let mut targets = vec![PeerTarget::from_pair(&info, PeerKind::Desktop).map_err(bad)?];
        targets.extend(PeerTarget::replica_from_pair(&info).map_err(bad)?);
        let relay = info
            .relay
            .as_deref()
            .map(|url| {
                url.parse()
                    .map(|url| RelayTarget {
                        url,
                        token: info.token.clone(),
                    })
                    .map_err(|err| bad(format!("relay {url:?}: {err}")))
            })
            .transpose()?;
        self.node.add_token(info.token.clone());
        let previous = self.shared.lock_state().pairing.replace(info);
        self.shared.pairing_gen.fetch_add(1, Ordering::AcqRel);
        if let Some(previous) = previous
            && previous.token != self.shared.token
            && previous.token != self.pair_token()
        {
            self.node.remove_token(&previous.token);
        }
        self.block_on(async {
            self.node.set_relay(relay).await.map_err(Self::internal)?;
            self.node.set_peers(targets).await;
            Ok::<(), KrabinkError>(())
        })?;
        self.shared.poke.notify_one();
        Ok(())
    }

    /// Leave the adopted workspace: drop this device's row from the synced
    /// registry, stop accepting the workspace token, forget its relay and
    /// stop dialling its peers. Notes already synced stay in the local
    /// store; the QR falls back to this device's own token. Peers are
    /// dropped shortly after the removal has been handed to them, so the
    /// row disappears on the other devices too (best effort: a peer that
    /// is offline learns it from the replica or the next device it meets).
    pub fn unpair(&self) -> Result<()> {
        let Some(previous) = self.shared.lock_state().pairing.take() else {
            return Ok(());
        };
        let generation = self.shared.pairing_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.remove_device(self.shared.device))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        if previous.token != self.shared.token {
            self.node.remove_token(&previous.token);
        }
        let node = self.node.clone();
        let shared = Arc::downgrade(&self.shared);
        self.runtime.spawn(async move {
            tokio::time::sleep(UNPAIR_LINGER).await;
            // A re-pair in the meantime owns the peer list now.
            let Some(shared) = shared.upgrade() else {
                return;
            };
            if shared.pairing_gen.load(Ordering::Acquire) != generation {
                return;
            }
            node.set_peers(Vec::new()).await;
            if let Err(err) = node.set_relay(None).await {
                tracing::warn!(%err, "dropping relay after unpair failed");
            }
            shared.poke.notify_one();
        });
        self.shared.poke.notify_one();
        Ok(())
    }

    /// A direct `ip:port` for a peer found on the local network (Bonjour);
    /// tried on the next dial and, when connected through the relay, as a
    /// path upgrade.
    pub fn add_peer_addr(&self, node: String, addr: String) -> Result<()> {
        let id = node
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: node })?;
        let addr = addr
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: addr })?;
        self.node.add_addr_hint(id, addr);
        Ok(())
    }

    /// Bring the node online (after [`Core::suspend`], or a no-op when it
    /// already is): rebinds and redials every peer.
    pub fn connect(&self) -> Result<()> {
        self.block_on(self.node.resume()).map_err(Self::internal)?;
        self.shared.online.store(true, Ordering::Release);
        self.shared.poke.notify_one();
        Ok(())
    }

    /// Close every connection and the endpoint (app background). Local
    /// edits keep flowing into the node's store; [`Core::connect`] resumes.
    pub fn suspend(&self) {
        self.shared.online.store(false, Ordering::Release);
        self.block_on(self.node.suspend());
        self.shared.poke.notify_one();
    }

    /// The network changed under us (Wi-Fi hop, VPN, cellular): re-probe
    /// paths now instead of waiting for timeouts.
    pub fn network_changed(&self) {
        self.block_on(self.node.network_changed());
    }

    pub fn device_id(&self) -> String {
        self.shared.device.to_string()
    }

    /// This node's endpoint id (its public key), what peers dial.
    pub fn node_id(&self) -> String {
        self.node.id().to_string()
    }

    /// UDP port the node is bound to; `None` while suspended.
    pub fn bound_port(&self) -> Option<u16> {
        self.block_on(self.node.bound_port())
    }

    /// Every peer the node knows: dialled ones (with their dial state) and
    /// inbound ones.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.node
            .peers()
            .into_iter()
            .filter_map(PeerInfo::from_status)
            .collect()
    }

    /// Current aggregate state (also pushed to the listener on change).
    pub fn sync_state(&self) -> SyncState {
        net::sync_state(&self.shared, &self.node)
    }

    /// What this device shows as a QR: its node id and direct addresses,
    /// plus the adopted workspace's token, relay and replica (or our own
    /// token while unpaired).
    pub fn pair_info(&self) -> PairInfo {
        let (relay, replica) = {
            let state = self.shared.lock_state();
            match &state.pairing {
                Some(p) => (p.relay.clone(), p.replica.clone()),
                None => (None, None),
            }
        };
        let addrs = direct_addrs(&self.block_on(self.node.addr()))
            .into_iter()
            .map(|a| a.to_string())
            .collect();
        PairInfo {
            node: self.node.id().to_string(),
            token: self.pair_token(),
            relay,
            addrs,
            replica,
        }
    }

    /// Drop a note from the shared registry: every peer's list loses the row.
    /// The note doc's history stays in local stores (GC is backlog).
    pub fn delete_note(&self, id: String) -> Result<()> {
        let note_id: NoteId = id.parse().map_err(|_| KrabinkError::MalformedId { id })?;
        let payload = {
            let mut state = self.shared.lock_state();
            state.notes.remove(&note_id);
            commit_workspace(&mut state, |ws| ws.remove(note_id))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    /// Upsert this device into the synced registry. The app shell calls it
    /// with a user-facing name whenever it (re)connects to a workspace and
    /// again when the user renames the device; every peer's list follows.
    pub fn register_device(&self, name: String, platform: String) -> Result<()> {
        let meta = pcore::DeviceMeta {
            id: self.shared.device,
            name,
            platform,
            last_seen_ms: now_ms(),
        };
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.upsert_device(&meta))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    /// Forget a device in the synced registry (every peer's list loses the
    /// row). Not revocation: it re-registers if it reconnects with a valid
    /// token.
    pub fn remove_device(&self, id: String) -> Result<()> {
        let device: pcore::DeviceId = id.parse().map_err(|_| KrabinkError::MalformedId { id })?;
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.remove_device(device))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        // Sever the live link to that device both ways: it hears the unpair
        // over the wire, drops us, and forgets the pairing on its side.
        self.block_on(self.node.unpair(device));
        Ok(())
    }

    /// Every device that ever joined this workspace, most recent first.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        let state = self.shared.lock_state();
        state
            .workspace
            .devices()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// The workspace's brush library, newest edit first. Bundled brushes
    /// (`builtin_brushes`) are not in it.
    pub fn list_brushes(&self) -> Vec<BrushInfo> {
        let state = self.shared.lock_state();
        state
            .workspace
            .brushes()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Add or edit a library brush; `spec` must decode in this build.
    /// Existing strokes keep the spec they snapshotted.
    pub fn upsert_brush(&self, id: String, name: String, spec: Vec<u8>) -> Result<()> {
        pcore::BrushSpec::decode(&spec)?;
        let meta = pcore::BrushMeta {
            id: pcore::BrushId(id),
            name,
            spec,
            updated_ms: now_ms(),
        };
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.upsert_brush(&meta))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    /// The workspace's image assets, newest first (bundled ones are in
    /// `builtin_assets`).
    pub fn list_assets(&self) -> Vec<AssetInfo> {
        let state = self.shared.lock_state();
        state
            .workspace
            .assets()
            .into_iter()
            .map(|a| a.asset.into())
            .collect()
    }

    /// Add a greyscale PNG (≤ 64 KiB) to the workspace; returns its id.
    /// The same bytes added twice are one asset.
    pub fn put_asset(&self, name: String, kind: AssetKind, png: Vec<u8>) -> Result<String> {
        let asset = pcore::Asset::from_png(name, kind.into(), png)?;
        let id = asset.id.0.clone();
        let meta = pcore::AssetMeta {
            asset,
            added_ms: now_ms(),
        };
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.put_asset(&meta))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(id)
    }

    pub fn remove_asset(&self, id: String) -> Result<()> {
        let id = pcore::AssetId(id);
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.remove_asset(&id))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    pub fn remove_brush(&self, id: String) -> Result<()> {
        let id = pcore::BrushId(id);
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.remove_brush(&id))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }
}

/// Apply a workspace mutation, persist the delta and return it for the wire.
/// Returns `None` when the mutation produced no ops.
pub(crate) fn commit_workspace(
    state: &mut State,
    f: impl FnOnce(&WorkspaceDoc) -> pcore::Result<()>,
) -> Result<Option<Vec<u8>>> {
    let before = state.workspace.version();
    f(&state.workspace)?;
    let payload = state.workspace.export_updates_since(&before)?;
    if payload.is_empty() {
        return Ok(None);
    }
    state
        .store
        .append_update(DocKey::WORKSPACE, &payload, Flush::Immediate)?;
    Ok(Some(payload))
}

/// One open note. Cheap handle: all state lives in the shared engine.
#[derive(uniffi::Object)]
pub struct NoteSession {
    shared: Arc<Shared>,
    id: NoteId,
    key: DocKey,
}

impl NoteSession {
    /// Mutate this note's doc, persist the delta, forward it to the network.
    fn commit<T>(
        &self,
        flush: Flush,
        f: impl FnOnce(&pcore::NoteDoc) -> pcore::Result<T>,
    ) -> Result<T> {
        let payload;
        let out;
        {
            let mut state = self.shared.lock_state();
            let state = &mut *state;
            let note = state.notes.get(&self.id).ok_or(KrabinkError::UnknownNote {
                id: self.id.to_string(),
            })?;
            let before = note.doc.version();
            out = f(&note.doc)?;
            payload = note.doc.export_updates_since(&before)?;
            if !payload.is_empty() {
                state.store.append_update(self.key, &payload, flush)?;
            }
        }
        if !payload.is_empty() {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: self.key,
                payload,
            });
        }
        Ok(out)
    }

    fn read<T>(&self, f: impl FnOnce(&pcore::NoteDoc) -> Result<T>) -> Result<T> {
        let state = self.shared.lock_state();
        let note = state.notes.get(&self.id).ok_or(KrabinkError::UnknownNote {
            id: self.id.to_string(),
        })?;
        f(&note.doc)
    }

    fn send_wet(&self, ink: pcore::WetInk) -> Result<()> {
        let payload = ink.encode()?;
        let _ = self.shared.cmd.send(Cmd::Ephemeral {
            doc: self.key,
            payload,
        });
        Ok(())
    }

    fn parse_sketch(&self, sketch: &str) -> Result<SketchId> {
        sketch.parse().map_err(|_| KrabinkError::MalformedId {
            id: sketch.to_string(),
        })
    }
}

#[uniffi::export]
impl NoteSession {
    pub fn id(&self) -> String {
        self.id.to_string()
    }

    pub fn set_listener(&self, listener: Arc<dyn NoteListener>) {
        let mut state = self.shared.lock_state();
        if let Some(note) = state.notes.get_mut(&self.id) {
            note.listener = Some(listener);
        }
    }

    pub fn text(&self) -> Result<String> {
        self.read(|doc| Ok(doc.text()))
    }

    /// Replace `del` unicode chars at `at` with `insert`.
    pub fn apply_text_edit(&self, at: u64, del: u64, insert: String) -> Result<()> {
        self.commit(Flush::Eventual, |doc| {
            doc.splice_text(at as usize, del as usize, &insert)
        })
    }

    pub fn title(&self) -> Result<Option<String>> {
        self.read(|doc| Ok(doc.title()))
    }

    pub fn set_title(&self, title: String) -> Result<()> {
        self.commit(Flush::Immediate, |doc| doc.set_title(&title))?;
        let ws_payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| {
                ws.upsert(&NoteMeta {
                    id: self.id,
                    title,
                    archived: false,
                    updated_ms: now_ms(),
                })
            })?
        };
        if let Some(payload) = ws_payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    pub fn sketch_ids(&self) -> Result<Vec<String>> {
        self.read(|doc| Ok(doc.sketch_ids().iter().map(SketchId::to_string).collect()))
    }

    pub fn create_sketch(&self) -> Result<String> {
        self.commit(Flush::Immediate, |doc| doc.create_sketch(now_ms()))
            .map(|id| id.to_string())
    }

    /// All strokes of a sketch in z-order (shapes left out; prefer
    /// [`Self::elements`]).
    pub fn strokes(&self, sketch: String) -> Result<Vec<Stroke>> {
        let sketch = self.parse_sketch(&sketch)?;
        self.read(|doc| Ok(doc.strokes(sketch)?.into_iter().map(Into::into).collect()))
    }

    /// Every element of a sketch (strokes and shapes) in z-order.
    pub fn elements(&self, sketch: String) -> Result<Vec<Element>> {
        let sketch = self.parse_sketch(&sketch)?;
        self.read(|doc| Ok(doc.elements(sketch)?.into_iter().map(Into::into).collect()))
    }

    /// Pen-down: announce a wet stroke on the ephemeral channel. Returns the
    /// stroke id to use for `append_points` and the committed stroke.
    pub fn begin_stroke(
        &self,
        sketch: String,
        tool: Tool,
        color: u32,
        base_width: f32,
        spec: Option<Vec<u8>>,
    ) -> Result<String> {
        let sketch = self.parse_sketch(&sketch)?;
        let stroke = pcore::StrokeId::new();
        self.send_wet(pcore::WetInk::Begin {
            sketch,
            stroke,
            tool: tool.into(),
            color: rgba_from_u32(color),
            base_width,
            spec,
        })?;
        Ok(stroke.to_string())
    }

    /// The stored points emitted since the last batch (`BrushModeler.push`
    /// results). `seq` is monotonic per stroke, starting at 1.
    pub fn append_points(&self, stroke: String, seq: u32, points: Vec<StrokePoint>) -> Result<()> {
        let stroke: pcore::StrokeId = stroke
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: stroke })?;
        let points: Vec<pcore::StrokePoint> = points.into_iter().map(Into::into).collect();
        self.send_wet(pcore::WetInk::points(stroke, seq, now_ms(), &points)?)
    }

    /// Pen-up: commit the authoritative stroke to the CRDT and end the wet
    /// stream. `stroke.id` must be the id returned by `begin_stroke` (or a
    /// fresh ULID when there was no wet phase). `tail` is whatever
    /// `BrushModeler.finish` added beyond the last `append_points` batch,
    /// so receivers complete the wet stroke before the commit lands.
    pub fn finish_stroke(
        &self,
        sketch: String,
        stroke: Stroke,
        tail: Vec<StrokePoint>,
    ) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let stroke_id: pcore::StrokeId =
            stroke.id.parse().map_err(|_| KrabinkError::MalformedId {
                id: stroke.id.clone(),
            })?;
        let committed = pcore::Stroke::from(stroke);
        self.commit(Flush::Immediate, |doc| doc.add_stroke(sketch, &committed))?;
        let tail: Vec<pcore::StrokePoint> = tail.into_iter().map(Into::into).collect();
        self.send_wet(pcore::WetInk::end(stroke_id, now_ms(), &tail)?)
    }

    /// Pen-up on a stroke that snapped to a shape: commit the shape under
    /// the wet stroke's id and end the wet stream, so receivers swap the
    /// provisional ink for the shape in one step. `shape.id` must be the id
    /// returned by `begin_stroke`.
    pub fn finish_shape(&self, sketch: String, shape: ShapeElement) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let id: pcore::ElementId = shape.id.parse().map_err(|_| KrabinkError::MalformedId {
            id: shape.id.clone(),
        })?;
        let committed = pcore::ShapeElement::from(shape);
        self.commit(Flush::Immediate, |doc| doc.add_shape(sketch, &committed))?;
        self.send_wet(pcore::WetInk::end(id, now_ms(), &[])?)
    }

    /// Eraser sample: remove every element whose ink a circle of `radius`
    /// at (`x`, `y`) touches; returns their ids so the view can drop them.
    /// The hit test lives in the core so erasing matches on every platform.
    pub fn erase_at(&self, sketch: String, x: f32, y: f32, radius: f32) -> Result<Vec<String>> {
        let sketch_id = self.parse_sketch(&sketch)?;
        let hit: Vec<pcore::ElementId> = self.read(|doc| {
            Ok(doc
                .elements(sketch_id)?
                .iter()
                .filter(|el| el.ink().hits(&el.outline(), x, y, radius))
                .map(pcore::Element::id)
                .collect())
        })?;
        for id in &hit {
            self.commit(Flush::Immediate, |doc| doc.remove_element(sketch_id, *id))?;
        }
        Ok(hit.iter().map(ToString::to_string).collect())
    }

    /// The wet stream opened by [`Self::begin_stroke`] ends without a
    /// stroke (the pen moved a ruler, not ink): tell receivers to drop the
    /// provisional ink immediately.
    pub fn cancel_stroke(&self, stroke: String) -> Result<()> {
        let stroke_id: pcore::StrokeId = stroke
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: stroke })?;
        self.send_wet(pcore::WetInk::Cancel { stroke: stroke_id })
    }

    /// Where the pen is now, hovering (`down == false`) or drawing: peers
    /// show a pointer there. `tool == None` means the eraser is selected
    /// and `base_width` is its diameter. Call at a throttled rate; the
    /// view decides the cadence.
    #[allow(clippy::too_many_arguments)]
    pub fn send_pointer(
        &self,
        sketch: String,
        x: f32,
        y: f32,
        tilt: Option<Tilt>,
        tool: Option<Tool>,
        color: u32,
        base_width: f32,
        down: bool,
    ) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        self.send_wet(pcore::WetInk::Pointer {
            sketch,
            x,
            y,
            tilt: tilt.map(Into::into),
            tool: tool.map(Into::into),
            color: rgba_from_u32(color),
            base_width,
            down,
            sent_ms: now_ms(),
        })
    }

    /// The pen left the sketch: peers drop the pointer at once instead of
    /// waiting for it to go stale.
    pub fn send_pointer_gone(&self, sketch: String) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        self.send_wet(pcore::WetInk::PointerGone { sketch })
    }

    /// Remove one element (stroke or shape) by id.
    pub fn remove_element(&self, sketch: String, element: String) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let id: pcore::ElementId = element
            .parse()
            .map_err(|_| KrabinkError::MalformedId { id: element })?;
        self.commit(Flush::Immediate, |doc| doc.remove_element(sketch, id))
    }

    /// Alias of [`Self::remove_element`].
    pub fn remove_stroke(&self, sketch: String, stroke: String) -> Result<()> {
        self.remove_element(sketch, stroke)
    }
}
