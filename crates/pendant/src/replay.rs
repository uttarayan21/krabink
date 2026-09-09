//! `pendant replay`: a headless client that creates a note with a sketch and
//! streams synthetic 120Hz pen strokes through the relay — wet ink over the
//! ephemeral channel, authoritative strokes committed to the CRDT at pen-up.
//! This is the latency test rig for the desktop renderer (M5 gate).

use std::collections::HashMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pendant_core::{
    ClientDocs, ClientEffect, ClientMsg, DeviceId, DocKey, NoteDoc, NoteId, NoteMeta, PointKind,
    Rgba, SKETCH_URI_PREFIX, SketchId, Stroke, StrokeId, StrokePoint, Tool, WetInk, WetPoint,
    WorkspaceDoc,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::docs::now_ms;
use crate::errors::{Error, Result, ResultExt};

/// 120Hz pen sampling, batched every ~66ms like the iPad streamer will.
const SAMPLE_DT: Duration = Duration::from_micros(8_333);
const BATCH_EVERY: usize = 8;
const STROKE_SAMPLES: usize = 144; // ~1.2s of ink
const PAUSE_BETWEEN: Duration = Duration::from_millis(300);

pub struct ReplayArgs {
    pub server: String,
    pub token: String,
    pub strokes: usize,
}

/// In-memory docs for the replay client; nothing touches disk.
struct MemDocs {
    workspace: WorkspaceDoc,
    notes: HashMap<NoteId, NoteDoc>,
}

impl ClientDocs for MemDocs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> pendant_core::Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        if doc == DocKey::WORKSPACE {
            self.workspace.import_update(payload)
        } else if let Some((_, note)) = self.notes.iter().find(|(id, _)| DocKey::from(**id) == doc)
        {
            note.import_update(payload)
        } else {
            Ok(())
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .change_context(Error)
        .attach("building tokio runtime")?;
    runtime.block_on(replay(args))
}

async fn replay(args: ReplayArgs) -> Result<()> {
    let mut request = args
        .server
        .as_str()
        .into_client_request()
        .change_context(Error)
        .attach("parsing server url")?;
    let auth = format!("Bearer {}", args.token)
        .parse()
        .change_context(Error)
        .attach("token not header-safe")?;
    request.headers_mut().insert("Authorization", auth);

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .change_context(Error)
        .attach("connecting to relay")?;
    let (mut sink, mut stream) = socket.split();

    let mut docs = MemDocs {
        workspace: WorkspaceDoc::new(),
        notes: HashMap::new(),
    };
    let mut session = pendant_core::ClientSession::new(DeviceId::new(), args.token.clone());

    // Handshake, then pull the workspace before touching it.
    send_all(&mut sink, session.connect()).await?;
    wait_ready(&mut session, &mut stream, &mut sink, &mut docs).await?;
    let have = docs.workspace.version();
    send_all(&mut sink, session.subscribe(DocKey::WORKSPACE, have)).await?;
    wait_synced(
        &mut session,
        &mut stream,
        &mut sink,
        &mut docs,
        DocKey::WORKSPACE,
    )
    .await?;

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
    send_all(&mut sink, session.subscribe(note_key, Vec::new())).await?;
    wait_synced(&mut session, &mut stream, &mut sink, &mut docs, note_key).await?;

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
    send_all(
        &mut sink,
        session.local_update(DocKey::WORKSPACE, ws_payload),
    )
    .await?;

    println!(
        "replay note {note_id} / sketch {sketch}; open it in the desktop app (or run with --follow-latest)"
    );

    for i in 0..args.strokes {
        stream_stroke(
            &mut session,
            &mut sink,
            &mut docs,
            note_id,
            note_key,
            sketch,
            i,
        )
        .await?;
        tokio::time::sleep(PAUSE_BETWEEN).await;
    }

    println!("replayed {} strokes", args.strokes);
    Ok(())
}

type Sink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;
type Stream = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn send_all(sink: &mut Sink, effects: Vec<ClientEffect>) -> Result<()> {
    for effect in effects {
        match effect {
            ClientEffect::Send(msg) => send_msg(sink, &msg).await?,
            ClientEffect::Fatal(err) => {
                return Err(crate::errors::Report::new(Error).attach(format!("session: {err}")));
            }
            _ => {}
        }
    }
    Ok(())
}

async fn send_msg(sink: &mut Sink, msg: &ClientMsg) -> Result<()> {
    let frame = msg
        .encode()
        .change_context(Error)
        .attach("encoding frame")?;
    sink.send(WsMessage::Binary(frame.into()))
        .await
        .change_context(Error)
        .attach("socket send")
}

/// Pump incoming frames until the predicate effect shows up (5s deadline).
async fn pump_until(
    session: &mut pendant_core::ClientSession,
    stream: &mut Stream,
    sink: &mut Sink,
    docs: &mut MemDocs,
    mut done: impl FnMut(&ClientEffect) -> bool,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .change_context(Error)
            .attach("timed out waiting for server")?;
        let Some(Ok(WsMessage::Binary(bytes))) = msg else {
            match msg {
                Some(Ok(_)) => continue,
                _ => return Err(crate::errors::Report::new(Error).attach("socket closed")),
            }
        };
        let server_msg = pendant_core::ServerMsg::decode(&bytes)
            .change_context(Error)
            .attach("decoding server frame")?;
        let effects = session.handle(server_msg, docs);
        let hit = effects.iter().any(&mut done);
        send_all(sink, effects).await?;
        if hit {
            return Ok(());
        }
    }
}

async fn wait_ready(
    session: &mut pendant_core::ClientSession,
    stream: &mut Stream,
    sink: &mut Sink,
    docs: &mut MemDocs,
) -> Result<()> {
    pump_until(session, stream, sink, docs, |e| {
        matches!(e, ClientEffect::Connected)
    })
    .await
}

async fn wait_synced(
    session: &mut pendant_core::ClientSession,
    stream: &mut Stream,
    sink: &mut Sink,
    docs: &mut MemDocs,
    doc: DocKey,
) -> Result<()> {
    pump_until(
        session,
        stream,
        sink,
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

async fn stream_stroke(
    session: &mut pendant_core::ClientSession,
    sink: &mut Sink,
    docs: &mut MemDocs,
    note_id: NoteId,
    note_key: DocKey,
    sketch: SketchId,
    index: usize,
) -> Result<()> {
    let stroke_id = StrokeId::new();
    let points = sample_stroke(index);
    let color = Rgba([30, 60, 200, 255]);
    let base_width = 3.0;

    let begin = WetInk::Begin {
        sketch,
        stroke: stroke_id,
        tool: Tool::Pen,
        color,
        base_width,
    }
    .encode()
    .change_context(Error)?;
    send_all(sink, session.ephemeral(note_key, begin)).await?;

    // Pace batches in real time: 8 samples per ~66ms tick.
    for (i, batch) in points.chunks(BATCH_EVERY).enumerate() {
        tokio::time::sleep(SAMPLE_DT * batch.len() as u32).await;
        let payload = WetInk::Points {
            stroke: stroke_id,
            seq: i as u32 + 1,
            sent_ms: now_ms(),
            points: batch
                .iter()
                .map(|p| WetPoint {
                    x: p.x,
                    y: p.y,
                    force: p.force,
                    width: None,
                    nib: None,
                })
                .collect(),
        }
        .encode()
        .change_context(Error)?;
        send_all(sink, session.ephemeral(note_key, payload)).await?;
    }

    let end = WetInk::End {
        stroke: stroke_id,
        sent_ms: now_ms(),
    }
    .encode()
    .change_context(Error)?;
    send_all(sink, session.ephemeral(note_key, end)).await?;

    // Pen-up: commit the authoritative stroke.
    let note = docs.notes.get(&note_id).expect("note created above");
    let before = note.version();
    note.add_stroke(
        sketch,
        &Stroke {
            id: stroke_id,
            tool: Tool::Pen,
            color,
            base_width,
            kind: PointKind::PolylineSample,
            points,
            created_ms: now_ms(),
        },
    )
    .change_context(Error)?;
    let payload = note.export_updates_since(&before).change_context(Error)?;
    send_all(sink, session.local_update(note_key, payload)).await?;
    println!("stroke {} committed ({stroke_id})", index + 1);
    Ok(())
}
