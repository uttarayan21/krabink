//! Desktop-class Rust peer for cross-device sync checks (used by the iOS UI
//! test harness). Joins a workspace from a pairing URI with a fresh store,
//! waits for the newest note, optionally asserts its text contains a
//! substring and/or appends text, and can draw or count page (overlay)
//! ink and inline-sketch ink.
//!
//! cargo run -p krabink-ffi --example probe -- \
//!     --pair 'krabink://pair?node=…&token=…&relay=…' \
//!     [--expect SUBSTRING] [--append TEXT] [--timeout-secs 15] \
//!     [--add-page-stroke LINE] [--expect-page-elements N] \
//!     [--add-sketch-stroke EMBED] [--expect-sketch-elements N]
//!
//! `EMBED` is the 0-based index of the `![…](krabink://sketch/…)` embed
//! line in text order; `--expect-sketch-elements` waits for the first
//! embed's sketch to hold `N` elements.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use krabink_ffi::{
    Core, NoteListener, NoteSession, PointKind, Stroke, StrokePoint, StyleKind, Tool,
    parse_pair_uri, style_runs,
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
    fn page_changed(&self) {}
    fn strokes_changed(&self, _sketch: String) {}
    fn wet_begin(
        &self,
        _sketch: String,
        _st: String,
        _t: Tool,
        _c: u32,
        _w: f32,
        _spec: Option<Vec<u8>>,
    ) {
        self.wet.lock().unwrap().begins += 1;
        eprintln!("wet begin (inline)");
    }
    fn wet_begin_anchored(
        &self,
        _st: String,
        _anchor: Vec<u8>,
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
    /// Draw a page stroke anchored to this 0-based source line.
    add_page_stroke: Option<usize>,
    expect_page_elements: Option<usize>,
    /// Draw a stroke inside the nth inline sketch embed (text order).
    add_sketch_stroke: Option<usize>,
    /// Wait until the first embed's sketch holds this many elements.
    expect_sketch_elements: Option<usize>,
    wet_watch: Option<Duration>,
    timeout: Duration,
    /// Print the device registry as it changes, then exit; no note needed.
    devices: bool,
    register: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        pair: String::new(),
        expect: None,
        append: None,
        add_page_stroke: None,
        expect_page_elements: None,
        add_sketch_stroke: None,
        expect_sketch_elements: None,
        wet_watch: None,
        timeout: Duration::from_secs(15),
        devices: false,
        register: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--pair" => args.pair = value(),
            "--expect" => args.expect = Some(value()),
            "--append" => args.append = Some(value()),
            "--add-page-stroke" => args.add_page_stroke = Some(value().parse().unwrap()),
            "--expect-page-elements" => args.expect_page_elements = Some(value().parse().unwrap()),
            "--add-sketch-stroke" => args.add_sketch_stroke = Some(value().parse().unwrap()),
            "--expect-sketch-elements" => {
                args.expect_sketch_elements = Some(value().parse().unwrap());
            }
            "--wet-watch" => args.wet_watch = Some(Duration::from_secs(value().parse().unwrap())),
            "--timeout-secs" => args.timeout = Duration::from_secs(value().parse().unwrap()),
            "--devices" => args.devices = true,
            "--register" => args.register = Some(value()),
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

/// Sketch ids of the note's inline embeds, in text order.
fn embeds(session: &NoteSession) -> Vec<String> {
    let text = session.text().unwrap_or_default();
    style_runs(text)
        .into_iter()
        .filter_map(|r| match r.kind {
            StyleKind::SketchEmbed { sketch } => Some(sketch),
            _ => None,
        })
        .collect()
}

fn main() {
    let args = parse_args();
    let dir = tempfile::tempdir().expect("tempdir");

    let core = Core::new(dir.path().to_str().unwrap().into()).expect("core");
    let pair = parse_pair_uri(args.pair.clone()).expect("--pair is not a krabink://pair URI");
    core.set_pairing(pair).expect("pairing");

    if args.devices {
        if let Some(name) = &args.register {
            core.register_device(name.clone(), "probe".into())
                .expect("register");
        }
        wait_for("connection", args.timeout, || {
            matches!(core.sync_state(), krabink_ffi::SyncState::Connected { .. }).then_some(())
        });
        println!("connected: {:?}", core.sync_state());
        let deadline = Instant::now() + args.timeout;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let now: Vec<String> = core
                .list_devices()
                .into_iter()
                .map(|d| format!("{} {:?} {}", d.id, d.name, d.platform))
                .collect();
            if now != seen {
                seen = now;
                println!("devices ({}): {seen:#?}", seen.len());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        return;
    }

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

    if let Some(want) = args.expect_page_elements {
        wait_for(&format!("{want} page elements"), args.timeout, || {
            let n = session.page_elements().ok()?.len();
            (n == want).then_some(())
        });
    }

    if let Some(want) = args.expect_sketch_elements {
        wait_for(
            &format!("{want} inline sketch elements"),
            args.timeout,
            || {
                let sketch = embeds(&session).into_iter().next()?;
                let n = session.elements(sketch).ok()?.len();
                (n == want).then_some(())
            },
        );
    }

    if let Some(index) = args.add_sketch_stroke {
        let sketch = wait_for(&format!("embed #{index}"), args.timeout, || {
            embeds(&session).into_iter().nth(index)
        });
        let id = session
            .begin_stroke(sketch.clone(), Tool::Pen, 0xc81e_3cff, 6.0, None)
            .expect("begin sketch stroke");
        let points = (0..6u32)
            .map(|i| StrokePoint {
                x: 20.0 + 30.0 * i as f32,
                y: 20.0 + 6.0 * i as f32,
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
                    color: 0xc81e_3cff,
                    base_width: 6.0,
                    kind: PointKind::BsplineControl,
                    points,
                    created_ms: now_unix_ms(),
                    brush: None,
                },
                Vec::new(),
            )
            .expect("finish sketch stroke");
        // Give the network task a moment to flush the update.
        std::thread::sleep(Duration::from_millis(750));
    }

    if let Some(line) = args.add_page_stroke {
        // Anchor to the first char of the requested source line (clamped
        // to the end of the text when there are fewer lines).
        let text = session.text().expect("text");
        let start = text
            .split_inclusive('\n')
            .take(line)
            .map(|l| l.chars().count())
            .sum::<usize>() as u64;
        let anchor = session.anchor_at(start).expect("anchor");
        let id = session
            .begin_page_stroke(anchor.clone(), Tool::Pen, 0x1e3c_c8ff, 6.0, None)
            .expect("begin page stroke");
        let points = (0..6u32)
            .map(|i| StrokePoint {
                x: 20.0 + 30.0 * i as f32,
                y: 4.0 + 2.0 * i as f32,
                force: 1.0,
                t_ms: i * 16,
                tilt: None,
                size: None,
            })
            .collect();
        session
            .finish_page_stroke(
                Stroke {
                    id,
                    tool: Tool::Pen,
                    color: 0x1e3c_c8ff,
                    base_width: 6.0,
                    kind: PointKind::BsplineControl,
                    points,
                    created_ms: now_unix_ms(),
                    brush: None,
                },
                anchor,
                Vec::new(),
            )
            .expect("finish page stroke");
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

    let inline: Vec<String> = embeds(&session)
        .into_iter()
        .map(|sketch| {
            let n = session
                .elements(sketch.clone())
                .map(|e| e.len())
                .unwrap_or(0);
            format!("{sketch}={n}")
        })
        .collect();
    println!(
        "probe OK — note {} title {:?}\npage elements {}\ninline sketches [{}]\n{}",
        newest.id,
        newest.title,
        session.page_elements().map(|p| p.len()).unwrap_or(0),
        inline.join(" "),
        session.text().expect("text")
    );
}
