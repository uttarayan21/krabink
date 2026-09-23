//! Desktop-class Rust peer for cross-device sync checks (used by the iOS UI
//! test harness). Joins a workspace from a pairing URI with a fresh store,
//! waits for the newest note, optionally asserts its text contains a
//! substring and/or appends text.
//!
//! cargo run -p pendant-ffi --example probe -- \
//!     --pair 'pendant://pair?node=…&token=…&relay=…' \
//!     [--expect SUBSTRING] [--append TEXT] [--timeout-secs 15]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pendant_ffi::{
    Core, NoteListener, NoteSession, PointKind, Stroke, StrokePoint, Tool, parse_pair_uri,
};

#[derive(Default)]
struct Recorder {
    synced: Mutex<bool>,
    /// (recv-sent) millis per wet batch, plus point/stroke totals.
    wet: Mutex<WetStats>,
}

#[derive(Default)]
struct WetStats {
    latencies_ms: Vec<i64>,
    points: usize,
    begins: usize,
    ends: usize,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

impl NoteListener for Recorder {
    fn synced(&self) {
        *self.synced.lock().unwrap() = true;
    }
    fn text_changed(&self, _text: String) {}
    fn strokes_changed(&self, _sketch: String) {}
    fn wet_begin(
        &self,
        _s: String,
        _st: String,
        _t: Tool,
        _c: u32,
        _w: f32,
        _spec: Option<Vec<u8>>,
    ) {
        self.wet.lock().unwrap().begins += 1;
        eprintln!("wet begin");
    }
    fn wet_points(&self, _s: String, sent_ms: u64, p: Vec<StrokePoint>) {
        let latency = now_unix_ms() as i64 - sent_ms as i64;
        let mut wet = self.wet.lock().unwrap();
        wet.latencies_ms.push(latency);
        wet.points += p.len();
        eprintln!("wet batch: points={} latency_ms={}", p.len(), latency);
    }
    fn wet_cancel(&self, _stroke: String) {}
    fn wet_end(&self, _s: String) {
        self.wet.lock().unwrap().ends += 1;
    }
}

struct Args {
    pair: String,
    expect: Option<String>,
    append: Option<String>,
    expect_strokes: Option<usize>,
    add_stroke: bool,
    wet_watch: Option<Duration>,
    timeout: Duration,
}

fn parse_args() -> Args {
    let mut args = Args {
        pair: String::new(),
        expect: None,
        append: None,
        expect_strokes: None,
        add_stroke: false,
        wet_watch: None,
        timeout: Duration::from_secs(15),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--pair" => args.pair = value(),
            "--expect" => args.expect = Some(value()),
            "--append" => args.append = Some(value()),
            "--expect-strokes" => args.expect_strokes = Some(value().parse().unwrap()),
            "--add-stroke" => args.add_stroke = true,
            "--wet-watch" => args.wet_watch = Some(Duration::from_secs(value().parse().unwrap())),
            "--timeout-secs" => args.timeout = Duration::from_secs(value().parse().unwrap()),
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(!args.pair.is_empty(), "--pair is required");
    args
}

fn wait_for<T>(what: &str, timeout: Duration, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(v) = poll() {
            return v;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("probe: timed out waiting for {what}");
    std::process::exit(1);
}

fn main() {
    let args = parse_args();
    let dir = tempfile::tempdir().expect("tempdir");

    let core = Core::new(dir.path().to_str().unwrap().into()).expect("core");
    let pair = parse_pair_uri(args.pair.clone()).expect("--pair is not a pendant://pair URI");
    core.set_pairing(pair).expect("pairing");

    let newest = wait_for("a note in the workspace", args.timeout, || {
        core.list_notes().into_iter().next()
    });
    let session = core.clone().open_note(newest.id.clone()).expect("open");
    let recorder = Arc::new(Recorder::default());
    session.set_listener(recorder.clone());
    wait_for("note catch-up", args.timeout, || {
        (*recorder.synced.lock().unwrap()).then_some(())
    });

    if let Some(expect) = &args.expect {
        wait_for(&format!("text containing {expect:?}"), args.timeout, || {
            session.text().ok().filter(|t| t.contains(expect.as_str()))
        });
    }

    if let Some(want) = args.expect_strokes {
        let sketch = first_sketch(&session, args.timeout);
        wait_for(
            &format!("{want} strokes in the sketch"),
            args.timeout,
            || {
                let n = session.strokes(sketch.clone()).ok()?.len();
                (n == want).then_some(())
            },
        );
    }

    if args.add_stroke {
        let sketch = first_sketch(&session, args.timeout);
        let id = session
            .begin_stroke(sketch.clone(), Tool::Pen, 0x1e3c_c8ff, 10.0, None)
            .expect("begin stroke");
        let points = (0..6u32)
            .map(|i| StrokePoint {
                x: 40.0 + 30.0 * i as f32,
                y: 40.0 + 20.0 * i as f32,
                force: 1.0,
                t_ms: i * 16,
                tilt: None,
                size: None,
            })
            .collect();
        session
            .finish_stroke(
                sketch,
                Stroke {
                    id,
                    tool: Tool::Pen,
                    color: 0x1e3c_c8ff,
                    base_width: 10.0,
                    kind: PointKind::BsplineControl,
                    points,
                    created_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                    brush: None,
                },
                Vec::new(),
            )
            .expect("finish stroke");
        // Give the network task a moment to flush the update.
        std::thread::sleep(Duration::from_millis(750));
    }

    if let Some(window) = args.wet_watch {
        // Latency gate rig: sit on the ephemeral channel while someone draws
        // on another device, then report receive-latency percentiles.
        // Cross-device numbers assume NTP-synced clocks.
        println!("wet-watch: draw now — collecting for {}s", window.as_secs());
        std::thread::sleep(window);
        let mut wet = recorder.wet.lock().unwrap();
        wet.latencies_ms.sort_unstable();
        let at = |q: f64| {
            wet.latencies_ms
                .get(((wet.latencies_ms.len() as f64 - 1.0) * q) as usize)
                .copied()
                .unwrap_or(0)
        };
        println!(
            "wet-watch: batches={} points={} strokes(begin/end)={}/{} \
             latency ms p50={} p95={} max={}",
            wet.latencies_ms.len(),
            wet.points,
            wet.begins,
            wet.ends,
            at(0.5),
            at(0.95),
            wet.latencies_ms.last().copied().unwrap_or(0),
        );
    }

    if let Some(append) = &args.append {
        let len = session.text().expect("text").chars().count() as u64;
        session
            .apply_text_edit(len, 0, append.clone())
            .expect("append");
        // Give the network task a moment to flush the update.
        std::thread::sleep(Duration::from_millis(750));
    }

    let mut sketch_summary = String::new();
    for sketch in session.sketch_ids().unwrap_or_default() {
        let count = session
            .strokes(sketch.clone())
            .map(|s| s.len())
            .unwrap_or(0);
        sketch_summary.push_str(&format!("\nsketch {sketch} strokes {count}"));
    }
    println!(
        "probe OK — note {} title {:?}{}\n{}",
        newest.id,
        newest.title,
        sketch_summary,
        session.text().expect("text")
    );
}

fn first_sketch(session: &NoteSession, timeout: Duration) -> String {
    wait_for("a sketch on the note", timeout, || {
        session.sketch_ids().ok()?.into_iter().next()
    })
}
