//! Replay the recorded strokes in `tests/corpus/brush/` through every
//! input model this build has and the tip evaluator, and report how they
//! behave on real pen data (`corpus::StrokeMetrics`). Run with
//! `--nocapture` to see the table; with `--features ism` the ISM model is
//! measured next to the EMA one, and `KRABINK_METRICS_JSON=path` writes
//! every row for the decision memo. Bounds are loose on purpose: this is
//! a regression net, not a tuning target. Recordings come from the iPad's
//! `-recordStrokes 1` console output; name them by tool and what was
//! drawn.

use std::path::Path;

use krabink_core::{InputModelKind, corpus};

#[test]
fn recorded_strokes_replay_within_bounds() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/brush");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus files in {}", dir.display());
    println!(
        "{:<24} {:<4} {:>7} {:>6} {:>7} {:>6} {:>6} {:>7} {:>13} {:>8} {:>7}",
        "file",
        "model",
        "samples",
        "points",
        "jitter",
        "lag",
        "dev",
        "over",
        "width",
        "verts",
        "µs"
    );
    let mut failures = Vec::new();
    let mut rows = Vec::new();
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let rec = corpus::parse(&std::fs::read_to_string(path).expect("read"))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(rec.samples.len() >= 2, "{name}: too short");
        for model in InputModelKind::available() {
            let m = corpus::StrokeMetrics::measure(&rec, model).expect("available model");
            println!(
                "{name:<24} {:<4} {:>7} {:>6} {:>7.3} {:>6.2} {:>6.2} {:>7.3} {:>6.2}..{:<5.2} {:>8} {:>7} (raw jitter {:.3})",
                model.name(),
                m.samples,
                m.points,
                m.jitter,
                m.lag,
                m.deviation,
                m.overshoot,
                m.width.0,
                m.width.1,
                m.vertices,
                m.micros,
                m.raw_jitter
            );
            rows.push(m.json(name, model));
            let mut check = |ok: bool, what: String| {
                if !ok {
                    failures.push(format!("{name} [{}]: {what}", model.name()));
                }
            };
            check(m.points >= 2, "no points emitted".into());
            check(m.vertices >= 3, "empty mesh".into());
            check(
                m.width.0 >= 0.05 && m.width.1 <= rec.size * 3.0,
                format!("width {:?} for size {}", m.width, rec.size),
            );
            check(
                m.jitter <= m.raw_jitter * 1.5 + 0.05,
                format!(
                    "smoothing added jitter: {} vs raw {}",
                    m.jitter, m.raw_jitter
                ),
            );
            check(m.overshoot < 1e-3, format!("overshoot {}", m.overshoot));
            match model {
                InputModelKind::Ema => {
                    // `finish` lands one more point on the last raw sample.
                    check(
                        m.points <= m.samples + 1,
                        format!("{} points from {} samples", m.points, m.samples),
                    );
                    check(
                        m.lag <= 4.0 * rec.size,
                        format!("lag {} for size {}", m.lag, rec.size),
                    );
                }
                InputModelKind::Ism => {
                    // Upsampled to 180 Hz at least; the spring trails a fast
                    // pen by its time constant (tens of units) but the
                    // shape still follows the path.
                    check(
                        m.points <= m.samples * 4 + 40,
                        format!("{} points from {} samples", m.points, m.samples),
                    );
                    check(m.lag <= 60.0, format!("lag {}", m.lag));
                    check(
                        m.deviation <= 2.0,
                        format!("deviation {} from the raw path", m.deviation),
                    );
                }
            }
            if rec.version >= 2 && rec.tool == krabink_core::Tool::Fountain {
                check(rec.has_tilt(), "fountain recording without tilt".into());
            }
        }
    }
    if let Ok(path) = std::env::var("KRABINK_METRICS_JSON") {
        std::fs::write(&path, format!("[\n{}\n]\n", rows.join(",\n"))).expect("write metrics");
        println!("wrote {path}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
