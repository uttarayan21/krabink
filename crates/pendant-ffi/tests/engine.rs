//! FFI engine tests: local persistence plus a full two-client sync round trip
//! through the real relay router (in-process, ephemeral port).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pendant_ffi::{
    Core, CoreListener, NoteInfo, NoteListener, PointKind, Stroke, StrokePoint, SyncState, Tool,
    WetPoint,
};

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// Real relay on an ephemeral port, on its own thread + runtime.
fn start_server(dir: &std::path::Path, token: &str) -> String {
    let store = pendant_core::Store::open(&dir.join("server.redb")).unwrap();
    let state = pendant_server::app_state(store, vec![token.to_string()]);
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, pendant_server::router(state))
                    .await
                    .unwrap();
            });
    });
    let addr = addr_rx.recv().unwrap();
    format!("ws://{addr}/ws")
}

#[derive(Default)]
struct RecCore {
    notes: Mutex<Vec<NoteInfo>>,
    states: Mutex<Vec<SyncState>>,
}

impl CoreListener for RecCore {
    fn notes_changed(&self, notes: Vec<NoteInfo>) {
        *self.notes.lock().unwrap() = notes;
    }

    fn sync_state(&self, state: SyncState) {
        self.states.lock().unwrap().push(state);
    }
}

#[derive(Default)]
struct RecNote {
    synced: Mutex<u32>,
    text: Mutex<String>,
    stroke_events: Mutex<Vec<String>>,
    wet: Mutex<Vec<String>>,
}

impl NoteListener for RecNote {
    fn synced(&self) {
        *self.synced.lock().unwrap() += 1;
    }

    fn text_changed(&self, text: String) {
        *self.text.lock().unwrap() = text;
    }

    fn strokes_changed(&self, sketch: String) {
        self.stroke_events.lock().unwrap().push(sketch);
    }

    fn wet_begin(&self, _sketch: String, stroke: String, _tool: Tool, color: u32, width: f32) {
        self.wet
            .lock()
            .unwrap()
            .push(format!("begin:{stroke}:{color:08x}:{width}"));
    }

    fn wet_points(&self, stroke: String, _sent_ms: u64, points: Vec<WetPoint>) {
        self.wet
            .lock()
            .unwrap()
            .push(format!("points:{stroke}:{}", points.len()));
    }

    fn wet_end(&self, stroke: String) {
        self.wet.lock().unwrap().push(format!("end:{stroke}"));
    }
}

fn polyline(n: u32) -> Vec<StrokePoint> {
    (0..n)
        .map(|i| StrokePoint {
            x: i as f32 * 3.0,
            y: i as f32,
            force: 0.5,
            t_ms: i * 8,
            tilt: None,
            size: None,
        })
        .collect()
}

#[test]
fn local_state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_string();

    let core = Core::new(path.clone()).unwrap();
    let note = core.clone().create_note("groceries".into()).unwrap();
    note.apply_text_edit(0, 0, "# groceries\n\nmilk".into())
        .unwrap();
    let sketch = note.create_sketch().unwrap();
    let stroke_id = note
        .begin_stroke(sketch.clone(), Tool::Pen, 0x1e3cc8ff, 3.0)
        .unwrap();
    note.finish_stroke(
        sketch.clone(),
        Stroke {
            id: stroke_id.clone(),
            tool: Tool::Pen,
            color: 0x1e3cc8ff,
            base_width: 3.0,
            kind: PointKind::PolylineSample,
            points: polyline(16),
            created_ms: 1,
        },
    )
    .unwrap();
    let note_id = note.id();
    let device = core.device_id();
    drop(note);
    drop(core);

    let core = Core::new(path).unwrap();
    assert_eq!(core.device_id(), device, "device id must be stable");
    let notes = core.list_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "groceries");
    assert_eq!(notes[0].id, note_id);

    let note = core.clone().open_note(note_id).unwrap();
    assert_eq!(note.text().unwrap(), "# groceries\n\nmilk");
    assert_eq!(note.title().unwrap().as_deref(), Some("groceries"));
    let strokes = note.strokes(sketch).unwrap();
    assert_eq!(strokes.len(), 1);
    assert_eq!(strokes[0].id, stroke_id);
    assert_eq!(strokes[0].color, 0x1e3cc8ff);
    assert_eq!(strokes[0].points.len(), 16);
}

#[test]
fn bad_ids_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(dir.path().to_str().unwrap().into()).unwrap();
    assert!(core.clone().open_note("not-a-ulid".into()).is_err());
    let note = core.clone().create_note("x".into()).unwrap();
    assert!(note.strokes("nope".into()).is_err());
    assert!(core.connect().is_err(), "connect without server must fail");
}

#[test]
fn two_cores_converge_through_relay() {
    let dir = tempfile::tempdir().unwrap();
    let url = start_server(dir.path(), "secret");

    // A: create a note, then go online.
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let note_a = core_a.clone().create_note("shared".into()).unwrap();
    let rec_a = Arc::new(RecNote::default());
    note_a.set_listener(rec_a.clone());
    core_a.set_sync_server(url.clone(), "secret".into());
    core_a.connect().unwrap();
    wait_for("A note synced", || *rec_a.synced.lock().unwrap() > 0);

    // B: connect fresh, discover the note via the workspace, open it.
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let rec_core_b = Arc::new(RecCore::default());
    core_b.set_listener(rec_core_b.clone());
    core_b.set_sync_server(url, "secret".into());
    core_b.connect().unwrap();
    wait_for("B discovers the note", || {
        core_b.list_notes().iter().any(|n| n.title == "shared")
    });
    assert!(
        rec_core_b
            .states
            .lock()
            .unwrap()
            .contains(&SyncState::Connected)
    );

    let note_b = core_b.clone().open_note(note_a.id()).unwrap();
    let rec_b = Arc::new(RecNote::default());
    note_b.set_listener(rec_b.clone());
    wait_for("B note synced", || *rec_b.synced.lock().unwrap() > 0);

    // Text: A types, B observes via listener and direct read.
    note_a
        .apply_text_edit(0, 0, "# hello from A".into())
        .unwrap();
    wait_for("text reaches B", || {
        note_b.text().unwrap() == "# hello from A"
    });
    assert_eq!(*rec_b.text.lock().unwrap(), "# hello from A");

    // Sketch + live ink: wet events stream ephemerally, then the committed
    // stroke lands in the CRDT.
    let sketch = note_a.create_sketch().unwrap();
    wait_for("sketch reaches B", || {
        note_b.sketch_ids().unwrap().contains(&sketch)
    });

    let stroke_id = note_a
        .begin_stroke(sketch.clone(), Tool::Pen, 0x1e3cc8ff, 3.0)
        .unwrap();
    note_a
        .append_points(
            stroke_id.clone(),
            1,
            vec![
                WetPoint {
                    x: 0.0,
                    y: 0.0,
                    force: 0.5,
                    width: None,
                },
                WetPoint {
                    x: 3.0,
                    y: 1.0,
                    force: 0.6,
                    width: None,
                },
            ],
        )
        .unwrap();
    note_a
        .finish_stroke(
            sketch.clone(),
            Stroke {
                id: stroke_id.clone(),
                tool: Tool::Pen,
                color: 0x1e3cc8ff,
                base_width: 3.0,
                kind: PointKind::PolylineSample,
                points: polyline(16),
                created_ms: 2,
            },
        )
        .unwrap();

    wait_for("stroke reaches B", || {
        note_b
            .strokes(sketch.clone())
            .map(|s| s.len() == 1)
            .unwrap_or(false)
    });
    let strokes = note_b.strokes(sketch.clone()).unwrap();
    assert_eq!(strokes[0].id, stroke_id);
    assert_eq!(strokes[0].points.len(), 16);
    assert!(rec_b.stroke_events.lock().unwrap().contains(&sketch));

    let wet = rec_b.wet.lock().unwrap().clone();
    assert!(
        wet.iter()
            .any(|e| e == &format!("begin:{stroke_id}:1e3cc8ff:3")),
        "wet begin missing: {wet:?}"
    );
    assert!(
        wet.iter().any(|e| e == &format!("points:{stroke_id}:2")),
        "wet points missing: {wet:?}"
    );
    assert!(
        wet.iter().any(|e| e == &format!("end:{stroke_id}")),
        "wet end missing: {wet:?}"
    );

    // B edits flow back to A.
    let len = note_b.text().unwrap().chars().count() as u64;
    note_b
        .apply_text_edit(len, 0, "\n\n- from B".into())
        .unwrap();
    wait_for("B's edit reaches A", || {
        note_a.text().unwrap().ends_with("- from B")
    });

    // Suspend is quiet and reconnect works.
    core_a.suspend();
    core_a.connect().unwrap();
    wait_for("A resynced after suspend", || {
        *rec_a.synced.lock().unwrap() >= 2
    });
}
