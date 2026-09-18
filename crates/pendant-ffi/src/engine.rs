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
    BrushInfo, DeviceInfo, Element, NoteInfo, ShapeElement, Stroke, StrokePoint, SyncState, Tool,
    rgba_from_u32,
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
    /// The shared brush library changed (a peer added, edited or removed
    /// a brush); the full list, newest edit first.
    fn brushes_changed(&self, brushes: Vec<BrushInfo>);
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
    pub server: Option<SyncTarget>,
}

/// Where background sync connects: every direct path is raced, the
/// dedicated relay is only used when none of them answers; all take the
/// same token.
#[derive(Clone)]
pub(crate) struct SyncTarget {
    pub direct: Vec<String>,
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

    /// Every `direct` path is dialled in parallel on each (re)connect and
    /// the first handshake wins; `fallback` is only used when none of them
    /// answers within a few seconds. While on the fallback the direct paths
    /// are re-probed periodically and the session moves over as soon as
    /// one answers.
    pub fn set_sync_server(&self, direct: Vec<String>, token: String, fallback: Option<String>) {
        self.shared.lock_state().server = Some(SyncTarget {
            direct,
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

    /// Every device that ever joined this workspace, most recent first.
    /// Forget a device in the synced registry (every peer's list loses the
    /// row). Not revocation: it re-registers if it reconnects with a valid
    /// token.
    pub fn remove_device(&self, id: String) -> Result<()> {
        let device: pcore::DeviceId = id.parse().map_err(|_| PendantError::MalformedId { id })?;
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
            .map_err(|_| PendantError::MalformedId { id: stroke })?;
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
            stroke.id.parse().map_err(|_| PendantError::MalformedId {
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
        let id: pcore::ElementId = shape.id.parse().map_err(|_| PendantError::MalformedId {
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
            .map_err(|_| PendantError::MalformedId { id: stroke })?;
        self.send_wet(pcore::WetInk::Cancel { stroke: stroke_id })
    }

    /// Remove one element (stroke or shape) by id.
    pub fn remove_element(&self, sketch: String, element: String) -> Result<()> {
        let sketch = self.parse_sketch(&sketch)?;
        let id: pcore::ElementId = element
            .parse()
            .map_err(|_| PendantError::MalformedId { id: element })?;
        self.commit(Flush::Immediate, |doc| doc.remove_element(sketch, id))
    }

    /// Alias of [`Self::remove_element`].
    pub fn remove_stroke(&self, sketch: String, stroke: String) -> Result<()> {
        self.remove_element(sketch, stroke)
    }
}
