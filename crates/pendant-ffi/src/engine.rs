//! The FFI client engine: shared state behind [`Core`]/[`NoteSession`], with
//! every mutator following the same shape — mutate the CRDT under the state
//! lock, persist the delta, hand it to the network task. Listener callbacks
//! are always invoked with the state lock released, so a listener may call
//! straight back into `Core`/`NoteSession` without deadlocking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use pendant_core as pcore;
use pendant_core::{DeviceId, DocKey, Flush, NoteId, NoteMeta, SketchId, Store, WorkspaceDoc};
use tokio::sync::mpsc;

use crate::net::{self, Cmd};
use crate::types::{
    DeviceInfo, NoteInfo, Stroke, StrokePoint, SyncState, Tool, WetPoint, rgba_from_u32,
};

/// Errors crossing the FFI boundary. Flattened to message-carrying variants;
/// Swift rarely needs more than "which kind" + a human-readable cause.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PendantError {
    #[error("malformed id: {id}")]
    MalformedId { id: String },
    #[error("unknown note: {id}")]
    UnknownNote { id: String },
    #[error("sync server not configured")]
    NoServer,
    #[error("{message}")]
    Internal { message: String },
}

impl From<pcore::Error> for PendantError {
    fn from(err: pcore::Error) -> Self {
        Self::Internal {
            message: err.to_string(),
        }
    }
}

pub type Result<T, E = PendantError> = core::result::Result<T, E>;

/// Library-level events: note registry changes and sync connection state.
#[uniffi::export(foreign)]
pub trait CoreListener: Send + Sync {
    fn notes_changed(&self, notes: Vec<NoteInfo>);
    fn sync_state(&self, state: SyncState);
}

/// Per-note events. Text and stroke changes are coarse: re-read via
/// [`NoteSession::text`] / [`NoteSession::strokes`]. Wet-ink events mirror the
/// ephemeral stream and never touch the CRDT; render them provisionally and
/// drop the overlay when `strokes_changed` delivers the committed stroke.
#[uniffi::export(foreign)]
pub trait NoteListener: Send + Sync {
    /// Catch-up with the server finished; local edits now propagate live.
    /// Fires once per (re)connection.
    fn synced(&self);
    fn text_changed(&self, text: String);
    fn strokes_changed(&self, sketch: String);
    fn wet_begin(&self, sketch: String, stroke: String, tool: Tool, color: u32, base_width: f32);
    /// `sent_ms` is the sender's unix-millis clock when the batch left the
    /// pen — latency telemetry only, meaningless across skewed clocks.
    fn wet_points(&self, stroke: String, sent_ms: u64, points: Vec<WetPoint>);
    fn wet_end(&self, stroke: String);
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
    pub server: Option<SyncTarget>,
}

/// Where background sync connects: direct path first, dedicated relay as
/// fallback; both take the same token.
#[derive(Clone)]
pub(crate) struct SyncTarget {
    pub url: String,
    pub token: String,
    pub fallback: Option<String>,
}

/// Everything shared between the FFI objects and the network task.
pub(crate) struct Shared {
    pub device: DeviceId,
    pub state: Mutex<State>,
    pub cmd: mpsc::UnboundedSender<Cmd>,
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn load_or_create_device_id(dir: &Path) -> Result<DeviceId> {
    let path = dir.join("device_id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return text
            .trim()
            .parse()
            .map_err(|_| PendantError::MalformedId { id: text });
    }
    let id = DeviceId::new();
    std::fs::write(&path, id.to_string()).map_err(|err| PendantError::Internal {
        message: format!("writing {}: {err}", path.display()),
    })?;
    Ok(id)
}

/// The pendant client: local store + note registry + background sync.
#[derive(uniffi::Object)]
pub struct Core {
    shared: Arc<Shared>,
    // Owns the network task; dropped (and shut down) with the last Core ref.
    _runtime: tokio::runtime::Runtime,
}

#[uniffi::export]
impl Core {
    /// Open (or initialise) the app data directory: `pendant.redb` store plus
    /// a stable per-install `device_id`.
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Arc<Self>> {
        let dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&dir).map_err(|err| PendantError::Internal {
            message: format!("creating {}: {err}", dir.display()),
        })?;
        let store = Store::open(&dir.join("pendant.redb"))?;
        let device = load_or_create_device_id(&dir)?;

        let stored = store.load(DocKey::WORKSPACE)?;
        let workspace = WorkspaceDoc::from_bytes(
            stored.snapshot.as_deref(),
            stored.updates.iter().map(Vec::as_slice),
        )?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| PendantError::Internal {
                message: format!("starting runtime: {err}"),
            })?;
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            device,
            state: Mutex::new(State {
                store,
                workspace,
                notes: HashMap::new(),
                core_listener: None,
                server: None,
            }),
            cmd: tx,
        });
        runtime.spawn(net::run(Arc::downgrade(&shared), rx));
        Ok(Arc::new(Self {
            shared,
            _runtime: runtime,
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
        let note_id: NoteId = id.parse().map_err(|_| PendantError::MalformedId { id })?;
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

    /// `url` is tried first on every (re)connect; `fallback` on the attempt
    /// after a failed one, so a device that cannot reach the desktop
    /// directly ends up on the dedicated relay within one backoff step.
    pub fn set_sync_server(&self, url: String, token: String, fallback: Option<String>) {
        self.shared.lock_state().server = Some(SyncTarget {
            url,
            token,
            fallback,
        });
    }

    /// Start (or restart) background sync with the configured server.
    pub fn connect(&self) -> Result<()> {
        let Some(target) = self.shared.lock_state().server.clone() else {
            return Err(PendantError::NoServer);
        };
        let _ = self.shared.cmd.send(Cmd::Connect(target));
        Ok(())
    }

    /// Drop the connection (app background). Reconnect with [`Core::connect`].
    pub fn suspend(&self) {
        let _ = self.shared.cmd.send(Cmd::Suspend);
    }

    pub fn device_id(&self) -> String {
        self.shared.device.to_string()
    }

    /// Drop a note from the shared registry: every peer's list loses the row.
    /// The note doc's history stays in local stores (GC is backlog).
    pub fn delete_note(&self, id: String) -> Result<()> {
        let note_id: NoteId = id.parse().map_err(|_| PendantError::MalformedId { id })?;
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
    /// with a user-facing name whenever it (re)connects to a workspace.
    pub fn register_device(&self, name: String, platform: String) -> Result<()> {
        let meta = pcore::DeviceMeta {
            id: self.shared.device.to_string(),
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

    /// Every device that ever joined this workspace, most recent first.
    /// Forget a device in the synced registry (every peer's list loses the
    /// row). Not revocation: it re-registers if it reconnects with a valid
    /// token.
    pub fn remove_device(&self, id: String) -> Result<()> {
        let payload = {
            let mut state = self.shared.lock_state();
            commit_workspace(&mut state, |ws| ws.remove_device(&id))?
        };
        if let Some(payload) = payload {
            let _ = self.shared.cmd.send(Cmd::Update {
                doc: DocKey::WORKSPACE,
                payload,
            });
        }
        Ok(())
    }

    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        let state = self.shared.lock_state();
        state
            .workspace
            .devices()
            .into_iter()
            .map(Into::into)
            .collect()
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
            let note = state.notes.get(&self.id).ok_or(PendantError::UnknownNote {
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
        let note = state.notes.get(&self.id).ok_or(PendantError::UnknownNote {
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
        sketch.parse().map_err(|_| PendantError::MalformedId {
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

    /// All strokes of a sketch in z-order.
    pub fn strokes(&self, sketch: String) -> Result<Vec<Stroke>> {
        let sketch = self.parse_sketch(&sketch)?;
        self.read(|doc| Ok(doc.strokes(sketch)?.into_iter().map(Into::into).collect()))
    }

    /// Pen-down: announce a wet stroke on the ephemeral channel. Returns the
    /// stroke id to use for `append_points` and the committed stroke.
    pub fn begin_stroke(
        &self,
        sketch: String,
        tool: Tool,
        color: u32,
        base_width: f32,
    ) -> Result<String> {
        let sketch = self.parse_sketch(&sketch)?;
        let stroke = pcore::StrokeId::new();
        self.send_wet(pcore::WetInk::Begin {
            sketch,
            stroke,
            tool: tool.into(),
            color: rgba_from_u32(color),
            base_width,
        })?;
        Ok(stroke.to_string())
    }

    /// Live pen samples. `seq` is monotonic per stroke, starting at 1.
    pub fn append_points(&self, stroke: String, seq: u32, points: Vec<WetPoint>) -> Result<()> {
        let stroke: pcore::StrokeId = stroke
            .parse()
            .map_err(|_| PendantError::MalformedId { id: stroke })?;
        self.send_wet(pcore::WetInk::Points {
            stroke,
            seq,
            sent_ms: now_ms(),
            points: points
                .into_iter()
                .map(|p| pcore::WetPoint {
                    x: p.x,
                    y: p.y,
                    force: p.force,
                    width: p.width,
                })
                .collect(),
        })
    }

    /// Pen-up: commit the authoritative stroke to the CRDT and end the wet
    /// stream. `stroke.id` must be the id returned by `begin_stroke` (or a
    /// fresh ULID when there was no wet phase).
    pub fn finish_stroke(&self, sketch: String, stroke: Stroke) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let stroke_id: pcore::StrokeId =
            stroke.id.parse().map_err(|_| PendantError::MalformedId {
                id: stroke.id.clone(),
            })?;
        let committed = pcore::Stroke {
            id: stroke_id,
            tool: stroke.tool.into(),
            color: rgba_from_u32(stroke.color),
            base_width: stroke.base_width,
            kind: stroke.kind.into(),
            points: stroke.points.into_iter().map(StrokePoint::into).collect(),
            created_ms: stroke.created_ms,
        };
        self.commit(Flush::Immediate, |doc| doc.add_stroke(sketch, &committed))?;
        self.send_wet(pcore::WetInk::End {
            stroke: stroke_id,
            sent_ms: now_ms(),
        })
    }

    pub fn remove_stroke(&self, sketch: String, stroke: String) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let stroke: pcore::StrokeId = stroke
            .parse()
            .map_err(|_| PendantError::MalformedId { id: stroke })?;
        self.commit(Flush::Immediate, |doc| doc.remove_stroke(sketch, stroke))
    }
}
