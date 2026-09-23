//! `pendant replay`: a headless node that pairs with a target, creates a
//! note with a sketch and streams synthetic 120Hz pen strokes to it — wet
//! ink over the ephemeral lane, authoritative strokes committed to the
//! CRDT at pen-up. This is the latency test rig for the desktop renderer.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use pendant_core::{
    ClientDocs, ClientEffect, DeviceId, DocKey, NoteDoc, NoteId, NoteMeta, PairInfo, PointKind,
    Rgba, SKETCH_URI_PREFIX, SketchId, Stroke, StrokeId, StrokePoint, Tool, WetInk, WorkspaceDoc,
};
use pendant_local::{
    LocalLink, Node, NodeConfig, PeerKind, PeerState, PeerTarget, RelayTarget, Role,
};

use crate::docs::now_ms;
use crate::errors::{Error, Result, ResultExt};

/// 120Hz pen sampling, batched every ~66ms like the iPad streamer will.
const SAMPLE_DT: Duration = Duration::from_micros(8_333);
const BATCH_EVERY: usize = 8;
const STROKE_SAMPLES: usize = 144; // ~1.2s of ink
const PAUSE_BETWEEN: Duration = Duration::from_millis(300);

pub struct ReplayArgs {
    /// The target's `pendant://pair` URI.
    pub pair: String,
    pub strokes: usize,
    /// Send pen pointers too: hover to each stroke's start, then ride the
    /// tip while it draws.
    pub pointer: bool,
}

/// In-memory docs for the replay client; nothing touches disk.
struct MemDocs {
    workspace: WorkspaceDoc,
    notes: HashMap<NoteId, NoteDoc>,
}

impl ClientDocs for MemDocs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> pendant_core::Result<bool> {
        if payload.is_empty() {
            return Ok(false);
        }
        if doc == DocKey::WORKSPACE {
            self.workspace.import_update(payload)
        } else if let Some((_, note)) = self.notes.iter().find(|(id, _)| DocKey::from(**id) == doc)
        {
            note.import_update(payload)
        } else {
            Ok(false)
        }
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> pendant_core::Result<Vec<u8>> {
        if doc == DocKey::WORKSPACE {
            self.workspace.export_updates_since(have)
        } else {
            self.notes
                .iter()
                .find(|(id, _)| DocKey::from(**id) == doc)
                .map(|(_, note)| note.export_updates_since(have))
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }
}

pub fn run(args: ReplayArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .change_context(Error)
        .attach("building tokio runtime")?;
    runtime.block_on(replay(args))
}

async fn replay(args: ReplayArgs) -> Result<()> {
    let info = PairInfo::parse(&args.pair)
        .ok_or_else(|| crate::errors::Report::new(Error).attach("not a pendant://pair URI"))?;
    let target = PeerTarget::from_pair(&info, PeerKind::Desktop)
        .map_err(|err| crate::errors::Report::new(Error).attach(err))?;
    let mut peers = vec![target.clone()];
    if let Ok(Some(replica)) = PeerTarget::replica_from_pair(&info) {
        peers.push(replica);
    }

    // A throwaway node: fresh key, empty store, gone at exit.
    let dir = std::env::temp_dir().join(format!("pendant-replay-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).change_context(Error)?;
    let device = DeviceId::new();
    let node = Node::start(NodeConfig {
        key_path: dir.join("node_key"),
        store_path: dir.join("node.redb"),
        device,
        tokens: vec![info.token.clone()],
        relay: target.relay.clone().map(|url| RelayTarget {
            url,
            token: info.token.clone(),
        }),
        bind_port: None,
        role: Role::Device,
    })
    .await
    .change_context(Error)
    .attach("starting node")?;
    node.set_peers(peers).await;

    let mut link = node.local_link();
    let mut docs = MemDocs {
        workspace: WorkspaceDoc::new(),
        notes: HashMap::new(),
    };
    let mut session = pendant_core::ClientSession::new(device, info.token.clone());

    // Handshake with our node, then wait for the target to answer.
    send_all(&link, session.connect())?;
    wait_ready(&mut session, &mut link, &mut docs).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let connected = node
            .peers()
            .iter()
            .any(|p| p.id == Some(target.id) && matches!(p.state, PeerState::Connected { .. }));
        if connected {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            return Err(crate::errors::Report::new(Error)
                .attach(format!("target never connected: {:?}", node.peers())));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let have = docs.workspace.version();
    send_all(&link, session.subscribe(DocKey::WORKSPACE, have))?;
    wait_synced(&mut session, &mut link, &mut docs, DocKey::WORKSPACE).await?;

    // Create the target note + sketch and announce it via the workspace.
    let note_id = NoteId::new();
    let note = NoteDoc::new(note_id);
    note.set_title("replay").change_context(Error)?;
    let sketch = note.create_sketch(now_ms()).change_context(Error)?;
    note.splice_text(
        0,
        0,
        &format!("# replay\n\nlive ink below:\n\n![ink]({SKETCH_URI_PREFIX}{sketch})\n"),
    )
    .change_context(Error)?;
    docs.notes.insert(note_id, note);
    let note_key = DocKey::from(note_id);

    // Subscribing uploads the whole doc as SubscribeAck backfill.
    send_all(&link, session.subscribe(note_key, Vec::new()))?;
    wait_synced(&mut session, &mut link, &mut docs, note_key).await?;

    let ws_before = docs.workspace.version();
    docs.workspace
        .upsert(&NoteMeta {
            id: note_id,
            title: "replay".into(),
            archived: false,
            updated_ms: now_ms(),
        })
        .change_context(Error)?;
    let ws_payload = docs
        .workspace
        .export_updates_since(&ws_before)
        .change_context(Error)?;
    send_all(&link, session.local_update(DocKey::WORKSPACE, ws_payload))?;

    writeln!(
        std::io::stdout(),
        "replay note {note_id} / sketch {sketch}; open it in the desktop app (or run with --follow-latest)"
    )
    .change_context(Error)?;

    for i in 0..args.strokes {
        if args.pointer {
            hover_to(&mut session, &link, note_key, sketch, i).await?;
        }
        stream_stroke(
            &mut session,
            &link,
            &mut docs,
            note_id,
            note_key,
            sketch,
            i,
            args.pointer,
        )
        .await?;
        tokio::time::sleep(PAUSE_BETWEEN).await;
    }
    if args.pointer {
        let gone = WetInk::PointerGone { sketch }
            .encode()
            .change_context(Error)?;
        send_all(&link, session.ephemeral(note_key, gone))?;
    }

    writeln!(std::io::stdout(), "replayed {} strokes", args.strokes).change_context(Error)?;
    // Give the last commit a moment to leave before tearing down.
    tokio::time::sleep(Duration::from_millis(500)).await;
    node.shutdown().await.change_context(Error)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn send_all(link: &LocalLink, effects: Vec<ClientEffect>) -> Result<()> {
    for effect in effects {
        match effect {
            ClientEffect::Send(msg) => {
                let frame = msg
                    .encode()
                    .change_context(Error)
                    .attach("encoding frame")?;
                link.send(frame);
            }
            ClientEffect::Fatal(err) => {
                return Err(crate::errors::Report::new(Error).attach(format!("session: {err}")));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Pump incoming frames until the predicate effect shows up (5s deadline).
async fn pump_until(
    session: &mut pendant_core::ClientSession,
    link: &mut LocalLink,
    docs: &mut MemDocs,
    mut done: impl FnMut(&ClientEffect) -> bool,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let frame = tokio::time::timeout_at(deadline, link.from_node.recv())
            .await
            .change_context(Error)
            .attach("timed out waiting for the node")?
            .ok_or_else(|| crate::errors::Report::new(Error).attach("link closed"))?;
        let server_msg = pendant_core::ServerMsg::decode(&frame)
            .change_context(Error)
            .attach("decoding frame")?;
        let effects = session.handle(server_msg, docs);
        let hit = effects.iter().any(&mut done);
        send_all(link, effects)?;
        if hit {
            return Ok(());
        }
    }
}

async fn wait_ready(
    session: &mut pendant_core::ClientSession,
    link: &mut LocalLink,
    docs: &mut MemDocs,
) -> Result<()> {
    pump_until(session, link, docs, |e| {
        matches!(e, ClientEffect::Connected)
    })
    .await
}

async fn wait_synced(
    session: &mut pendant_core::ClientSession,
    link: &mut LocalLink,
    docs: &mut MemDocs,
    doc: DocKey,
) -> Result<()> {
    pump_until(
        session,
        link,
        docs,
        |e| matches!(e, ClientEffect::DocSynced(d) if *d == doc),
    )
    .await
}

/// One synthetic stroke: a pressure-modulated sine sweep.
fn sample_stroke(index: usize) -> Vec<StrokePoint> {
    let y_base = 40.0 + index as f32 * 60.0;
    (0..STROKE_SAMPLES)
        .map(|i| {
            let t = i as f32 / STROKE_SAMPLES as f32;
            StrokePoint {
                x: 20.0 + t * 400.0,
                y: y_base + (t * core::f32::consts::TAU * 2.0).sin() * 25.0,
                force: 0.35 + 0.6 * (t * core::f32::consts::PI).sin(),
                t_ms: (i as f32 * 8.333) as u32,
                tilt: None,
                size: None,
            }
        })
        .collect()
}

const POINTER_COLOR: Rgba = Rgba([30, 60, 200, 255]);
const POINTER_WIDTH: f32 = 3.0;

/// A `Pointer` frame for the replay pen at (`x`, `y`).
fn pointer_frame(sketch: SketchId, x: f32, y: f32, down: bool) -> Result<Vec<u8>> {
    WetInk::Pointer {
        sketch,
        x,
        y,
        tilt: None,
        tool: Some(Tool::Pen),
        color: POINTER_COLOR,
        base_width: POINTER_WIDTH,
        down,
        sent_ms: now_ms(),
    }
    .encode()
    .change_context(Error)
}

/// Glide the hovering pointer from the previous stroke's end to this
/// stroke's start over ~0.5s at the iPad's ~30Hz pointer cadence.
async fn hover_to(
    session: &mut pendant_core::ClientSession,
    link: &LocalLink,
    note_key: DocKey,
    sketch: SketchId,
    index: usize,
) -> Result<()> {
    const STEPS: usize = 15;
    let from = index
        .checked_sub(1)
        .and_then(|prev| sample_stroke(prev).last().copied())
        .map_or((40.0, 40.0), |p| (p.x, p.y));
    let to = sample_stroke(index)
        .first()
        .map_or((40.0, 40.0), |p| (p.x, p.y));
    for step in 0..=STEPS {
        let t = step as f32 / STEPS as f32;
        let x = from.0 + (to.0 - from.0) * t;
        let y = from.1 + (to.1 - from.1) * t;
        send_all(
            link,
            session.ephemeral(note_key, pointer_frame(sketch, x, y, false)?),
        )?;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn stream_stroke(
    session: &mut pendant_core::ClientSession,
    link: &LocalLink,
    docs: &mut MemDocs,
    note_id: NoteId,
    note_key: DocKey,
    sketch: SketchId,
    index: usize,
    pointer: bool,
) -> Result<()> {
    let stroke_id = StrokeId::new();
    let points = sample_stroke(index);
    let color = POINTER_COLOR;
    let base_width = POINTER_WIDTH;

    let begin = WetInk::Begin {
        sketch,
        stroke: stroke_id,
        tool: Tool::Pen,
        color,
        base_width,
        spec: None,
    }
    .encode()
    .change_context(Error)?;
    send_all(link, session.ephemeral(note_key, begin))?;

    // Pace batches in real time: 8 samples per ~66ms tick.
    for (i, batch) in points.chunks(BATCH_EVERY).enumerate() {
        tokio::time::sleep(SAMPLE_DT * batch.len() as u32).await;
        let payload = WetInk::points(stroke_id, i as u32 + 1, now_ms(), batch)
            .change_context(Error)?
            .encode()
            .change_context(Error)?;
        send_all(link, session.ephemeral(note_key, payload))?;
        if pointer && let Some(tip) = batch.last() {
            send_all(
                link,
                session.ephemeral(note_key, pointer_frame(sketch, tip.x, tip.y, true)?),
            )?;
        }
    }

    let end = WetInk::end(stroke_id, now_ms(), &[])
        .change_context(Error)?
        .encode()
        .change_context(Error)?;
    send_all(link, session.ephemeral(note_key, end))?;

    // Pen-up: commit the authoritative stroke.
    let note = docs.notes.get(&note_id).expect("note created above");
    let before = note.version();
    note.add_stroke(
        sketch,
        &Stroke {
            id: stroke_id,
            tool: Tool::Pen,
            brush: None,
            color,
            base_width,
            kind: PointKind::PolylineSample,
            points,
            created_ms: now_ms(),
        },
    )
    .change_context(Error)?;
    let payload = note.export_updates_since(&before).change_context(Error)?;
    send_all(link, session.local_update(note_key, payload))?;
    writeln!(
        std::io::stdout(),
        "stroke {} committed ({stroke_id})",
        index + 1
    )
    .change_context(Error)?;
    Ok(())
}
