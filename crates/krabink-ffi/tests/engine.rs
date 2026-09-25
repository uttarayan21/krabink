//! FFI engine tests: local persistence, then two `Core`s converging over
//! their nodes — directly on loopback, and through a real dev relay with
//! the headless replica in the middle.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use krabink_ffi::{
    AssetInfo, AssetKind, BrushInfo, Core, CoreListener, DeviceInfo, Element, NoteInfo,
    NoteListener, PageProbe, PairInfo, Point2, PointKind, Route, Shape, ShapeElement, Stroke,
    StrokePoint, StyleKind, SyncState, Tool, inline_min_height, inline_padding, style_runs,
};

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

const TOKEN: &str = "secret";

/// A dev relay (plain HTTP, ephemeral port) plus the workspace replica, on
/// their own thread + runtime. Returns the replica's pairing URI, exactly
/// what `krabink-server --dev` prints.
struct Cloud {
    pair: PairInfo,
    _thread: std::thread::JoinHandle<()>,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl Cloud {
    fn start(dir: &std::path::Path) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let dir = dir.to_path_buf();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let server = krabink_server::relay::spawn(
                        krabink_server::RelayOpts::dev(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                        vec![TOKEN.into()],
                    )
                    .await
                    .expect("relay spawns");
                    let url: iroh::RelayUrl =
                        format!("http://{}", server.http_addr().expect("http listener"))
                            .parse()
                            .unwrap();
                    let replica = krabink_server::replica::start(
                        &krabink_server::ReplicaConfig {
                            db: dir.join("replica.redb"),
                            key: dir.join("replica.key"),
                            udp_port: None,
                        },
                        url.clone(),
                        vec![TOKEN.into()],
                    )
                    .await
                    .expect("replica starts");
                    ready_tx
                        .send(PairInfo {
                            node: replica.id().to_string(),
                            token: TOKEN.into(),
                            relay: Some(url.to_string()),
                            addrs: Vec::new(),
                            replica: None,
                        })
                        .unwrap();
                    let _ = stop_rx.await;
                    let _ = replica.shutdown().await;
                    let _ = server.shutdown().await;
                });
        });
        Self {
            pair: ready_rx.recv().unwrap(),
            _thread: thread,
            stop,
        }
    }
}

fn connected(state: &SyncState) -> bool {
    matches!(state, SyncState::Connected { .. })
}

#[derive(Default)]
struct RecCore {
    notes: Mutex<Vec<NoteInfo>>,
    brushes: Mutex<Vec<BrushInfo>>,
    assets: Mutex<Vec<AssetInfo>>,
    devices: Mutex<Vec<DeviceInfo>>,
    states: Mutex<Vec<SyncState>>,
}

impl CoreListener for RecCore {
    fn notes_changed(&self, notes: Vec<NoteInfo>) {
        *self.notes.lock().unwrap() = notes;
    }

    fn brushes_changed(&self, brushes: Vec<BrushInfo>) {
        *self.brushes.lock().unwrap() = brushes;
    }

    fn assets_changed(&self, assets: Vec<AssetInfo>) {
        *self.assets.lock().unwrap() = assets;
    }

    fn devices_changed(&self, devices: Vec<DeviceInfo>) {
        *self.devices.lock().unwrap() = devices;
    }

    fn sync_state(&self, state: SyncState) {
        self.states.lock().unwrap().push(state);
    }
}

#[derive(Default)]
struct RecNote {
    synced: Mutex<u32>,
    text: Mutex<String>,
    page_events: Mutex<u32>,
    wet: Mutex<Vec<String>>,
    /// (stroke id, anchor) of every `wet_begin_anchored`.
    wet_anchored: Mutex<Vec<(String, Vec<u8>)>>,
    /// (sketch id, stroke id) of every `wet_begin`.
    wet_sketch: Mutex<Vec<(String, String)>>,
    /// Sketch ids of every `strokes_changed`.
    sketch_events: Mutex<Vec<String>>,
}

impl NoteListener for RecNote {
    fn synced(&self) {
        *self.synced.lock().unwrap() += 1;
    }

    fn text_changed(&self, text: String) {
        *self.text.lock().unwrap() = text;
    }

    fn page_changed(&self) {
        *self.page_events.lock().unwrap() += 1;
    }

    fn strokes_changed(&self, sketch: String) {
        self.sketch_events.lock().unwrap().push(sketch);
    }

    fn wet_begin(
        &self,
        sketch: String,
        stroke: String,
        _tool: Tool,
        color: u32,
        width: f32,
        spec: Option<Vec<u8>>,
    ) {
        let custom = if spec.is_some() { ":custom" } else { "" };
        self.wet
            .lock()
            .unwrap()
            .push(format!("begin:{stroke}:{color:08x}:{width}{custom}"));
        self.wet_sketch.lock().unwrap().push((sketch, stroke));
    }

    fn wet_begin_anchored(
        &self,
        stroke: String,
        anchor: Vec<u8>,
        _tool: Tool,
        color: u32,
        width: f32,
        spec: Option<Vec<u8>>,
    ) {
        let custom = if spec.is_some() { ":custom" } else { "" };
        self.wet
            .lock()
            .unwrap()
            .push(format!("begin:{stroke}:{color:08x}:{width}{custom}"));
        self.wet_anchored.lock().unwrap().push((stroke, anchor));
    }

    fn wet_points(&self, stroke: String, _sent_ms: u64, points: Vec<StrokePoint>) {
        self.wet
            .lock()
            .unwrap()
            .push(format!("points:{stroke}:{}", points.len()));
    }

    fn wet_cancel(&self, _stroke: String) {}

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
    // Ink on the "milk" line (scalar 13).
    let anchor = note.anchor_at(13).unwrap();
    let stroke_id = note
        .begin_page_stroke(anchor.clone(), Tool::Pen, 0x1e3cc8ff, 3.0, None)
        .unwrap();
    note.finish_page_stroke(
        Stroke {
            id: stroke_id.clone(),
            tool: Tool::Pen,
            color: 0x1e3cc8ff,
            base_width: 3.0,
            kind: PointKind::PolylineSample,
            points: polyline(16),
            created_ms: 1,
            brush: None,
        },
        anchor,
        Vec::new(),
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
    let page = note.page_elements().unwrap();
    assert_eq!(page.len(), 1);
    let Element::Stroke(stroke) = &page[0].element else {
        panic!("expected a stroke, got {:?}", page[0].element);
    };
    assert_eq!(stroke.id, stroke_id);
    assert_eq!(stroke.color, 0x1e3cc8ff);
    assert_eq!(stroke.points.len(), 16);
    assert_eq!(page[0].char_index, Some(13), "anchor resolves after reopen");
}

#[test]
fn bad_ids_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(dir.path().to_str().unwrap().into()).unwrap();
    assert!(core.clone().open_note("not-a-ulid".into()).is_err());
    let note = core.clone().create_note("x".into()).unwrap();
    assert!(note.remove_page_element("nope".into()).is_err());
    assert!(note.resolve_anchor(vec![1, 2, 3]).unwrap().is_none());
    assert!(
        core.set_pairing(PairInfo {
            node: "not-a-key".into(),
            token: "t".into(),
            relay: None,
            addrs: Vec::new(),
            replica: None,
        })
        .is_err(),
        "bad node id must be rejected"
    );
    assert!(core.add_peer_addr(core.node_id(), "nope".into()).is_err());
    // Unpaired: nothing to connect to, but the node is up and dialable.
    assert!(core.bound_port().is_some());
    assert_eq!(core.sync_state(), SyncState::Disconnected);
}

/// Two cores on one machine, no relay: B scans A's QR (with a loopback
/// address for the test) and connects straight to it.
/// The device registry must flow both ways, including a row written
/// before the connection is up (the app registers right after pairing).
#[test]
fn device_rows_converge_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    // Registered straight after construction, before the in-process link
    // has even finished its handshake, like the iPad's init -> adoptPair.
    core_b.register_device("pad".into(), "ipad".into()).unwrap();
    core_a
        .register_device("desk".into(), "linux".into())
        .unwrap();

    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));

    let has = |core: &Core, id: &str| core.list_devices().iter().any(|d| d.id == id);
    wait_for("B lists A", || has(&core_b, &core_a.device_id()));
    wait_for("A lists B", || has(&core_a, &core_b.device_id()));
}

/// Renaming is an upsert of our own row: the peer's listener gets the new
/// name. Unpairing drops our row on the peer and leaves the pairing behind
/// (own token in the QR, nothing dialled); a re-pair rebuilds it.
#[test]
fn rename_propagates_and_unpair_removes_row() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let rec_a = Arc::new(RecCore::default());
    core_a.set_listener(rec_a.clone());
    core_a
        .register_device("desk".into(), "linux".into())
        .unwrap();

    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair.clone()).unwrap();
    core_b.register_device("pad".into(), "ipad".into()).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));

    let name_of = |rec: &RecCore, id: &str| {
        rec.devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
    };
    let b_id = core_b.device_id();
    wait_for("A's listener sees pad", || {
        name_of(&rec_a, &b_id).as_deref() == Some("pad")
    });

    core_b
        .register_device("kitchen ipad".into(), "ipad".into())
        .unwrap();
    wait_for("A's listener sees the rename", || {
        name_of(&rec_a, &b_id).as_deref() == Some("kitchen ipad")
    });
    assert_eq!(
        core_b.list_devices().len(),
        2,
        "rename must not add a row on the renaming device"
    );

    core_b.unpair().unwrap();
    wait_for("A drops B's row", || name_of(&rec_a, &b_id).is_none());
    wait_for("B has nothing to dial", || {
        core_b.sync_state() == SyncState::Disconnected && core_b.peers().is_empty()
    });
    assert_ne!(
        core_b.pair_info().token,
        pair.token,
        "QR falls back to B's own token"
    );
    assert!(core_b.list_devices().iter().all(|d| d.id != b_id));

    // Re-pairing works again after leaving.
    core_b.set_pairing(pair).unwrap();
    core_b.register_device("pad".into(), "ipad".into()).unwrap();
    wait_for("B reconnects to A", || connected(&core_b.sync_state()));
    wait_for("A lists B again", || name_of(&rec_a, &b_id).is_some());
}

/// remove_device is a mutual, per-peer unpair: the removed peer hears it
/// over the wire, forgets its pairing, and both device lists drop the row.
#[test]
fn remove_device_unpairs_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    core_a
        .register_device("desk".into(), "linux".into())
        .unwrap();

    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    core_b.register_device("pad".into(), "ipad".into()).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));
    let has = |core: &Core, id: &str| core.list_devices().iter().any(|d| d.id == id);
    wait_for("A lists B", || has(&core_a, &core_b.device_id()));

    // A removes B from its list.
    core_a.remove_device(core_b.device_id()).unwrap();

    // A's list drops B, and B is evicted: it stops connecting.
    wait_for("A drops B", || !has(&core_a, &core_b.device_id()));
    wait_for("B is disconnected after eviction", || {
        core_b.sync_state() == SyncState::Disconnected
    });
    // B no longer dials A: reconnect only via a fresh pairing.
    assert!(core_b.peers().iter().all(|p| !p.connected));
}

#[test]
fn two_cores_converge_direct() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let rec_b = Arc::new(RecCore::default());
    core_b.set_listener(rec_b.clone());

    let mut pair = core_a.pair_info();
    assert_eq!(pair.node, core_a.node_id());
    assert!(pair.relay.is_none());
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));
    match core_b.sync_state() {
        SyncState::Connected { peer, route } => {
            assert_eq!(peer, core_a.node_id());
            assert!(
                matches!(route, Some(Route::Direct { .. })),
                "loopback must be direct: {route:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(rec_b.states.lock().unwrap().iter().any(connected));
    // A sees B as an inbound peer.
    wait_for("A lists B", || {
        core_a
            .peers()
            .iter()
            .any(|p| p.inbound && p.node == core_b.node_id())
    });

    let note_a = core_a.clone().create_note("direct".into()).unwrap();
    note_a.apply_text_edit(0, 0, "hi".into()).unwrap();
    wait_for("note reaches B", || {
        core_b.list_notes().iter().any(|n| n.title == "direct")
    });
    let note_b = core_b.clone().open_note(note_a.id()).unwrap();
    wait_for("text reaches B", || note_b.text().unwrap() == "hi");

    // Suspend drops the connection; connect brings it back.
    core_b.suspend();
    assert_eq!(core_b.sync_state(), SyncState::Disconnected);
    note_a.apply_text_edit(2, 0, "!".into()).unwrap();
    core_b.connect().unwrap();
    wait_for("B reconnects", || connected(&core_b.sync_state()));
    wait_for("offline edit reaches B", || note_b.text().unwrap() == "hi!");
}

/// A state reached before the app installs its listener (a peer dialled
/// in during launch) is pushed the moment the listener lands, and a
/// listener that dropped in and out never sees a stale label.
#[test]
fn late_listener_gets_current_state() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();

    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    // A is dialled by B; both are connected before either has a listener.
    wait_for("B connects to A", || connected(&core_b.sync_state()));
    wait_for("A lists B", || core_a.peers().iter().any(|p| p.inbound));

    let rec_a = Arc::new(RecCore::default());
    core_a.set_listener(rec_a.clone());
    let rec_b = Arc::new(RecCore::default());
    core_b.set_listener(rec_b.clone());
    wait_for("A's listener hears connected", || {
        rec_a.states.lock().unwrap().iter().any(connected)
    });
    wait_for("B's listener hears connected", || {
        rec_b.states.lock().unwrap().iter().any(connected)
    });
    // No "connecting" was ever reported to B: it was told the state as it
    // stood when it started listening.
    let states = rec_b.states.lock().unwrap();
    assert!(!states.contains(&SyncState::Connecting), "{states:?}");
}

#[test]
fn two_cores_converge_through_relay() {
    let dir = tempfile::tempdir().unwrap();
    let cloud = Cloud::start(dir.path());

    // A: pair with the cloud (the `krabink-server --dev` URI), create a note.
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    core_a.set_pairing(cloud.pair.clone()).unwrap();
    wait_for("A reaches the replica", || connected(&core_a.sync_state()));
    let note_a = core_a.clone().create_note("shared".into()).unwrap();
    let rec_a = Arc::new(RecNote::default());
    note_a.set_listener(rec_a.clone());
    wait_for("A note synced", || *rec_a.synced.lock().unwrap() > 0);

    // B: scan A's QR. It carries the relay, so B reaches A even without a
    // usable direct address; the replica is not named, so A is B's only peer.
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let rec_core_b = Arc::new(RecCore::default());
    core_b.set_listener(rec_core_b.clone());
    let mut pair = core_a.pair_info();
    assert_eq!(pair.relay, cloud.pair.relay);
    pair.addrs.clear();
    core_b.set_pairing(pair).unwrap();
    wait_for("B discovers the note", || {
        core_b.list_notes().iter().any(|n| n.title == "shared")
    });
    assert!(rec_core_b.states.lock().unwrap().iter().any(connected));

    let note_b = core_b.clone().open_note(note_a.id()).unwrap();
    let rec_b = Arc::new(RecNote::default());
    note_b.set_listener(rec_b.clone());
    wait_for("B note synced", || *rec_b.synced.lock().unwrap() > 0);

    // Brush library: A adds a brush, B lists it and its listener hears;
    // a removal syncs too.
    let spec = krabink_ffi::builtin_brushes()[0].spec.clone();
    core_a
        .upsert_brush("user:soft".into(), "Soft".into(), spec)
        .unwrap();
    wait_for("brush reaches B", || {
        core_b.list_brushes().iter().any(|b| b.id == "user:soft")
    });
    assert!(
        rec_core_b
            .brushes
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.name == "Soft")
    );
    assert!(
        core_a
            .upsert_brush("user:bad".into(), "Bad".into(), vec![9, 9])
            .is_err()
    );
    core_a.remove_brush("user:soft".into()).unwrap();
    wait_for("removal reaches B", || core_b.list_brushes().is_empty());

    // Assets: PNG bytes sync with their content id; junk is rejected.
    let paper = krabink_ffi::builtin_assets()[0].clone();
    let id = core_a
        .put_asset("paper copy".into(), AssetKind::Grain, paper.png.clone())
        .unwrap();
    assert_eq!(id, paper.id, "id is the content hash");
    assert!(
        core_a
            .put_asset("junk".into(), AssetKind::Mask, vec![1, 2, 3])
            .is_err()
    );
    wait_for("asset reaches B", || {
        core_b.list_assets().iter().any(|a| a.id == id)
    });
    assert!(
        rec_core_b
            .assets
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.png == paper.png)
    );
    core_a.remove_asset(id).unwrap();
    wait_for("asset removal reaches B", || {
        core_b.list_assets().is_empty()
    });

    // Text: A types, B observes via listener and direct read.
    note_a
        .apply_text_edit(0, 0, "# hello from A".into())
        .unwrap();
    wait_for("text reaches B", || {
        note_b.text().unwrap() == "# hello from A"
    });
    assert_eq!(*rec_b.text.lock().unwrap(), "# hello from A");

    // Page ink + live ink: wet events stream ephemerally, then the committed
    // stroke lands in the page layer, anchored to its line.
    let anchor = note_a.anchor_at(0).unwrap();
    let stroke_id = note_a
        .begin_page_stroke(anchor.clone(), Tool::Pen, 0x1e3cc8ff, 3.0, None)
        .unwrap();
    note_a
        .append_points(stroke_id.clone(), 1, polyline(2))
        .unwrap();
    note_a
        .finish_page_stroke(
            Stroke {
                id: stroke_id.clone(),
                tool: Tool::Pen,
                color: 0x1e3cc8ff,
                base_width: 3.0,
                kind: PointKind::PolylineSample,
                points: polyline(16),
                created_ms: 2,
                brush: None,
            },
            anchor.clone(),
            polyline(1),
        )
        .unwrap();

    wait_for("stroke reaches B", || {
        note_b
            .page_elements()
            .map(|p| p.len() == 1)
            .unwrap_or(false)
    });
    let page = note_b.page_elements().unwrap();
    assert_eq!(page[0].element.id(), stroke_id);
    assert!(matches!(&page[0].element, Element::Stroke(s) if s.points.len() == 16));
    assert_eq!(page[0].anchor, anchor);
    assert_eq!(page[0].char_index, Some(0));
    assert!(*rec_b.page_events.lock().unwrap() > 0);

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

    // A snapped shape: wet ink streams under an id, the shape commits
    // under the same id, and B is told the page changed (the change
    // detection must count shapes, not just strokes).
    let events_before = *rec_b.page_events.lock().unwrap();
    let shape_id = note_a
        .begin_page_stroke(anchor.clone(), Tool::Marker, 0xff0000ff, 5.0, None)
        .unwrap();
    note_a
        .finish_page_shape(
            ShapeElement {
                id: shape_id.clone(),
                shape: Shape::Rect {
                    center: Point2 { x: 50.0, y: 40.0 },
                    size: Point2 { x: 100.0, y: 80.0 },
                    angle: 0.0,
                },
                tool: Tool::Marker,
                color: 0xff0000ff,
                width: 5.0,
                start: None,
                end: None,
                created_ms: 3,
            },
            anchor.clone(),
        )
        .unwrap();
    wait_for("shape reaches B", || {
        note_b
            .page_elements()
            .map(|e| e.len() == 2)
            .unwrap_or(false)
    });
    let page = note_b.page_elements().unwrap();
    assert!(matches!(&page[0].element, Element::Stroke(s) if s.id == stroke_id));
    match &page[1].element {
        Element::Shape(s) => {
            assert_eq!(s.id, shape_id);
            assert!(matches!(s.shape, Shape::Rect { size, .. } if size.x == 100.0));
            assert_eq!((s.tool, s.color, s.width), (Tool::Marker, 0xff0000ff, 5.0));
        }
        other => panic!("expected the shape, got {other:?}"),
    }
    assert_eq!(page[1].char_index, Some(0));
    assert!(
        *rec_b.page_events.lock().unwrap() > events_before,
        "page_changed must fire for a remote shape"
    );
    assert!(
        rec_b
            .wet
            .lock()
            .unwrap()
            .contains(&format!("end:{shape_id}")),
        "wet end missing for the shape"
    );

    // B edits flow back to A.
    let len = note_b.text().unwrap().chars().count() as u64;
    note_b
        .apply_text_edit(len, 0, "\n\n- from B".into())
        .unwrap();
    wait_for("B's edit reaches A", || {
        note_a.text().unwrap().ends_with("- from B")
    });

    // A goes to the background; B's edit waits in the replica; A comes
    // back and catches up through it.
    core_a.suspend();
    let len = note_b.text().unwrap().chars().count() as u64;
    note_b
        .apply_text_edit(len, 0, " (while A slept)".into())
        .unwrap();
    core_a.connect().unwrap();
    wait_for("A catches up after suspend", || {
        note_a.text().unwrap().ends_with("(while A slept)")
    });

    // A wrong token is fatal, not an endless retry.
    let core_c = Core::new(dir.path().join("c").to_str().unwrap().into()).unwrap();
    let mut bad = cloud.pair.clone();
    bad.token = "wrong".into();
    core_c.set_pairing(bad).unwrap();
    wait_for("C is rejected", || {
        matches!(core_c.sync_state(), SyncState::Fatal { .. })
    });

    let _ = cloud.stop.send(());
}

/// A pen stroke of `n` points shifted by (`dx`, `dy`), for page ink.
fn page_stroke(id: String, n: u32, dx: f32, dy: f32) -> Stroke {
    let points = (0..n)
        .map(|i| StrokePoint {
            x: dx + i as f32 * 3.0,
            y: dy + i as f32,
            force: 0.5,
            t_ms: i * 8,
            tilt: None,
            size: None,
        })
        .collect();
    Stroke {
        id,
        tool: Tool::Pen,
        color: 0x1122_33ff,
        base_width: 4.0,
        kind: PointKind::PolylineSample,
        points,
        created_ms: 1,
        brush: None,
    }
}

/// Page ink drawn on A reaches B as anchored wet ink, then as a committed
/// page element whose anchor resolves to the same line; an edit above it
/// on B shifts A's resolution.
#[test]
fn page_ink_converges_direct() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));

    let note_a = core_a.clone().create_note("page".into()).unwrap();
    note_a.apply_text_edit(0, 0, "one\ntwo\n".into()).unwrap();
    wait_for("note reaches B", || {
        core_b.list_notes().iter().any(|n| n.title == "page")
    });
    let note_b = core_b.clone().open_note(note_a.id()).unwrap();
    let rec_b = Arc::new(RecNote::default());
    note_b.set_listener(rec_b.clone());
    wait_for("text reaches B", || note_b.text().unwrap() == "one\ntwo\n");

    // A draws on the line "two" (starts at scalar 4).
    let anchor = note_a.anchor_at(4).unwrap();
    assert_eq!(note_a.resolve_anchor(anchor.clone()).unwrap(), Some(4));
    let id = note_a
        .begin_page_stroke(anchor.clone(), Tool::Pen, 0x1122_33ff, 4.0, None)
        .unwrap();
    note_a.append_points(id.clone(), 1, polyline(3)).unwrap();
    note_a
        .finish_page_stroke(
            page_stroke(id.clone(), 5, 0.0, 0.0),
            anchor.clone(),
            Vec::new(),
        )
        .unwrap();

    wait_for("B hears the anchored wet begin", || {
        !rec_b.wet_anchored.lock().unwrap().is_empty()
    });
    {
        let wet = rec_b.wet_anchored.lock().unwrap();
        assert_eq!(wet[0].0, id);
        assert_eq!(wet[0].1, anchor, "anchor bytes cross unchanged");
    }
    wait_for("B hears page_changed", || {
        *rec_b.page_events.lock().unwrap() > 0
    });
    let page_b = note_b.page_elements().unwrap();
    assert_eq!(page_b.len(), 1);
    assert_eq!(page_b[0].element.id(), id);
    assert_eq!(page_b[0].anchor, anchor);
    assert_eq!(page_b[0].char_index, Some(4), "B resolves the same line");
    assert_eq!(note_b.resolve_anchor(anchor.clone()).unwrap(), Some(4));
    assert_eq!(
        note_b
            .resolve_anchors(vec![anchor.clone(), vec![1, 2, 3]])
            .unwrap(),
        vec![Some(4), None]
    );
    assert!(
        rec_b
            .wet
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == &format!("end:{id}")),
        "wet stream ended: {:?}",
        rec_b.wet.lock().unwrap()
    );

    // B inserts a line above: the ink on A follows "two".
    note_b.apply_text_edit(0, 0, "zero\n".into()).unwrap();
    wait_for("edit reaches A", || {
        note_a.text().unwrap() == "zero\none\ntwo\n"
    });
    assert_eq!(note_a.resolve_anchor(anchor.clone()).unwrap(), Some(9));
    assert_eq!(note_a.page_elements().unwrap()[0].char_index, Some(9));

    // Removal propagates as another page_changed.
    note_a.remove_page_element(id).unwrap();
    wait_for("removal reaches B", || {
        note_b.page_elements().unwrap().is_empty()
    });
    assert!(*rec_b.page_events.lock().unwrap() >= 2);
}

/// Only the elements whose ink a probe touches are removed; probes are in
/// each element's own anchor space, so the same pen position hits or
/// misses depending on the element's origin.
#[test]
fn erase_page_hits_only_touched_elements() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(dir.path().to_str().unwrap().into()).unwrap();
    let note = core.clone().create_note("erase".into()).unwrap();
    note.apply_text_edit(0, 0, "a\nb\n".into()).unwrap();
    let top = note.anchor_at(0).unwrap();
    let bottom = note.anchor_at(2).unwrap();
    let near = note
        .begin_page_stroke(top.clone(), Tool::Pen, 0xff, 4.0, None)
        .unwrap();
    note.finish_page_stroke(page_stroke(near.clone(), 6, 0.0, 0.0), top, Vec::new())
        .unwrap();
    let far = note
        .begin_page_stroke(bottom.clone(), Tool::Pen, 0xff, 4.0, None)
        .unwrap();
    note.finish_page_stroke(
        page_stroke(far.clone(), 6, 500.0, 500.0),
        bottom,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(note.page_elements().unwrap().len(), 2);

    // A pen at the near stroke's start, expressed in both anchor spaces.
    let probes = vec![
        PageProbe {
            element: near.clone(),
            x: 0.0,
            y: 0.0,
        },
        PageProbe {
            element: far.clone(),
            x: 0.0,
            y: 0.0,
        },
        PageProbe {
            element: "not-an-id".into(),
            x: 0.0,
            y: 0.0,
        },
    ];
    let hit = note.erase_page_at(probes, 6.0).unwrap();
    assert_eq!(hit, vec![near]);
    let left = note.page_elements().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].element.id(), far);
    assert_eq!(left[0].char_index, Some(2));
}

/// An inline sketch drawn on A reaches B as sketch-keyed wet ink, then as
/// a committed element under `strokes_changed`; both sides agree on the
/// box height; erasing removes it; the embed line styles as one run.
#[test]
fn inline_sketch_converges_direct() {
    let dir = tempfile::tempdir().unwrap();
    let core_a = Core::new(dir.path().join("a").to_str().unwrap().into()).unwrap();
    let core_b = Core::new(dir.path().join("b").to_str().unwrap().into()).unwrap();
    let mut pair = core_a.pair_info();
    pair.addrs = vec![format!("127.0.0.1:{}", core_a.bound_port().unwrap())];
    core_b.set_pairing(pair).unwrap();
    wait_for("B connects to A", || connected(&core_b.sync_state()));

    let note_a = core_a.clone().create_note("inline".into()).unwrap();
    let sketch = note_a.create_sketch().unwrap();
    assert_eq!(note_a.sketch_ids().unwrap(), vec![sketch.clone()]);
    assert_eq!(
        note_a.sketch_box_height(sketch.clone()).unwrap(),
        inline_min_height(),
        "empty sketch gets the minimum box"
    );
    let text = format!("intro\n![sketch](krabink://sketch/{sketch})\nafter\n");
    note_a.apply_text_edit(0, 0, text.clone()).unwrap();
    wait_for("note reaches B", || {
        core_b.list_notes().iter().any(|n| n.title == "inline")
    });
    let note_b = core_b.clone().open_note(note_a.id()).unwrap();
    let rec_b = Arc::new(RecNote::default());
    note_b.set_listener(rec_b.clone());
    wait_for("text reaches B", || note_b.text().unwrap() == text);
    assert_eq!(note_b.sketch_ids().unwrap(), vec![sketch.clone()]);

    // A draws a tall stroke inside the box.
    let id = note_a
        .begin_stroke(sketch.clone(), Tool::Pen, 0x1122_33ff, 4.0, None)
        .unwrap();
    note_a.append_points(id.clone(), 1, polyline(3)).unwrap();
    let mut stroke = page_stroke(id.clone(), 5, 0.0, 300.0);
    stroke.points.push(StrokePoint {
        x: 12.0,
        y: 300.0 + 4.0,
        force: 0.5,
        t_ms: 40,
        tilt: None,
        size: None,
    });
    note_a
        .finish_stroke(sketch.clone(), stroke, Vec::new())
        .unwrap();

    wait_for("B hears the sketch wet begin", || {
        !rec_b.wet_sketch.lock().unwrap().is_empty()
    });
    assert_eq!(
        rec_b.wet_sketch.lock().unwrap()[0],
        (sketch.clone(), id.clone())
    );
    // The container's arrival may already have fired one strokes_changed
    // (its length went from "absent" to 0); wait for the commit itself.
    wait_for("B hears strokes_changed for the stroke", || {
        !rec_b.sketch_events.lock().unwrap().is_empty()
            && note_b.elements(sketch.clone()).unwrap().len() == 1
    });
    assert!(
        rec_b
            .sketch_events
            .lock()
            .unwrap()
            .iter()
            .all(|s| s == &sketch)
    );
    let elements_b = note_b.elements(sketch.clone()).unwrap();
    assert_eq!(elements_b[0].id(), id);
    let height_a = note_a.sketch_box_height(sketch.clone()).unwrap();
    let height_b = note_b.sketch_box_height(sketch.clone()).unwrap();
    assert_eq!(height_a, height_b, "box height agrees on both replicas");
    assert!(
        height_a >= 304.0 + 2.0 * inline_padding(),
        "ink at y=304 grows the box: {height_a}"
    );
    assert_eq!(height_a, height_a.ceil(), "whole points");
    assert!(
        rec_b
            .wet
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == &format!("end:{id}")),
        "wet stream ended: {:?}",
        rec_b.wet.lock().unwrap()
    );

    // The embed line is one run on B, without markers inside it.
    let runs = style_runs(note_b.text().unwrap());
    let embed = runs
        .iter()
        .find(|r| {
            r.kind
                == StyleKind::SketchEmbed {
                    sketch: sketch.clone(),
                }
        })
        .unwrap_or_else(|| panic!("no embed run in {runs:?}"));
    assert_eq!(embed.start, 6);
    assert_eq!(
        embed.end,
        6 + 10 + 17 + 26 + 1,
        "![sketch]( + prefix + ulid + )"
    );
    assert!(
        !runs
            .iter()
            .any(|r| r.kind == StyleKind::Marker && r.start >= embed.start && r.end <= embed.end),
        "{runs:?}"
    );

    // Erasing at the stroke's start removes it on both sides.
    let events_before = rec_b.sketch_events.lock().unwrap().len();
    let hit = note_a.erase_at(sketch.clone(), 0.0, 300.0, 6.0).unwrap();
    assert_eq!(hit, vec![id]);
    wait_for("removal reaches B", || {
        note_b.elements(sketch.clone()).unwrap().is_empty()
    });
    assert!(rec_b.sketch_events.lock().unwrap().len() > events_before);
    assert_eq!(
        note_b.sketch_box_height(sketch.clone()).unwrap(),
        inline_min_height()
    );
    // A missing container is not an error for the height query.
    assert_eq!(
        note_b
            .sketch_box_height("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())
            .unwrap(),
        inline_min_height()
    );
    assert!(note_b.elements("nope".into()).is_err());
}

#[test]
fn style_runs_crosses_ffi() {
    let runs = style_runs("# Hi **there**\n".into());
    assert!(
        runs.iter()
            .any(|r| r.start == 0 && r.kind == StyleKind::Heading { level: 1 }),
        "{runs:?}"
    );
    assert!(
        runs.iter()
            .any(|r| r.start == 5 && r.end == 14 && r.kind == StyleKind::Strong),
        "{runs:?}"
    );
    assert!(
        runs.iter()
            .any(|r| r.start == 0 && r.end == 1 && r.kind == StyleKind::Marker),
        "{runs:?}"
    );
    assert!(style_runs(String::new()).is_empty());

    let embed = "![s](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAV)\n";
    let runs = style_runs(embed.into());
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        runs[0].kind,
        StyleKind::SketchEmbed {
            sketch: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into()
        }
    );
    assert_eq!((runs[0].start, runs[0].end), (0, 49));
}
