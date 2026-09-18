//! Replay the recorded strokes in `tests/corpus/brush/` through the input
//! model and the tip evaluator and report how the model behaves on real
//! pen data: point count, jitter, lag behind the pen, overshoot at pen-up.
//! Run with `--nocapture` to see the table. Bounds are loose on purpose:
//! this is a regression net, not a tuning target. Recordings come from
//! the iPad's `-recordStrokes 1` console output; name them by tool and
//! what was drawn.

use std::path::Path;

use pendant_core::{
    BrushModeler, DEFAULT_TOLERANCE, Ink, Rgba, StrokeEnd, StrokePoint, TipEvaluator, corpus,
};

struct Metrics {
    samples: usize,
    points: usize,
    /// Mean second difference of position over mean step: hand jitter
    /// left after smoothing, independent of how densely points fall.
    jitter: f32,
    /// Mean distance between each emitted point and the raw sample nearest
    /// in time, canvas units.
    lag: f32,
    /// Distance from the finished stroke's last point to the last raw
    /// sample.
    overshoot: f32,
    /// Lowest and highest tip width, canvas units.
    width: (f32, f32),
    vertices: usize,
}

fn jitter(points: impl Iterator<Item = [f32; 2]>) -> f32 {
    let pts: Vec<[f32; 2]> = points.collect();
    if pts.len() < 3 {
        return 0.0;
    }
    let second: f32 = pts
        .windows(3)
        .map(|w| {
            let ax = w[0][0] - 2.0 * w[1][0] + w[2][0];
            let ay = w[0][1] - 2.0 * w[1][1] + w[2][1];
            ax.hypot(ay)
        })
        .sum();
    let step: f32 = pts
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum();
    let steps = (pts.len() - 1) as f32;
    (second / (pts.len() - 2) as f32) / (step / steps).max(1e-3)
}

fn measure(rec: &corpus::Recording) -> Metrics {
    let mut modeler = BrushModeler::new(rec.tool, rec.size);
    for &s in &rec.samples {
        modeler.push(s);
    }
    let points = modeler.finish();
    let origin = rec.samples[0].t_ms;
    let lag = points
        .iter()
        .map(|p| {
            let t = origin + f64::from(p.t_ms);
            let nearest = rec
                .samples
                .iter()
                .min_by(|a, b| (a.t_ms - t).abs().total_cmp(&(b.t_ms - t).abs()))
                .expect("non-empty");
            (p.x - nearest.x).hypot(p.y - nearest.y)
        })
        .sum::<f32>()
        / points.len().max(1) as f32;
    let last_raw = rec.samples.last().expect("non-empty");
    let overshoot = points
        .last()
        .map_or(0.0, |p| (p.x - last_raw.x).hypot(p.y - last_raw.y));
    let ink = Ink::preset(rec.tool, rec.color.unwrap_or(Rgba::BLACK), rec.size);
    let tips = TipEvaluator::evaluate(&ink.spec, rec.size, &points, StrokeEnd::Complete);
    let width = tips.iter().fold((f32::INFINITY, 0.0_f32), |(lo, hi), t| {
        (lo.min(t.w), hi.max(t.w))
    });
    let mesh = ink.mesh(&points, StrokeEnd::Complete, DEFAULT_TOLERANCE);
    Metrics {
        samples: rec.samples.len(),
        points: points.len(),
        jitter: jitter(points.iter().map(|p: &StrokePoint| [p.x, p.y])),
        lag,
        overshoot,
        width,
        vertices: mesh.vertices.len(),
    }
}

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
        "{:<24} {:>7} {:>6} {:>7} {:>6} {:>7} {:>13} {:>8}",
        "file", "samples", "points", "jitter", "lag", "over", "width", "verts"
    );
    let mut failures = Vec::new();
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let rec = corpus::parse(&std::fs::read_to_string(path).expect("read"))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(rec.samples.len() >= 2, "{name}: too short");
        let raw_jitter = jitter(rec.samples.iter().map(|s| [s.x, s.y]));
        let m = measure(&rec);
        println!(
            "{name:<24} {:>7} {:>6} {:>7.3} {:>6.2} {:>7.3} {:>6.2}..{:<5.2} {:>8} (raw jitter {raw_jitter:.3})",
            m.samples, m.points, m.jitter, m.lag, m.overshoot, m.width.0, m.width.1, m.vertices
        );
        let mut check = |ok: bool, what: String| {
            if !ok {
                failures.push(format!("{name}: {what}"));
            }
        };
        check(m.points >= 2, "no points emitted".into());
        // `finish` lands one more point on the last raw sample.
        check(
            m.points <= m.samples + 1,
            format!("{} points from {} samples", m.points, m.samples),
        );
        check(
            m.jitter <= raw_jitter * 1.5 + 0.05,
            format!("smoothing added jitter: {} vs raw {raw_jitter}", m.jitter),
        );
        check(
            m.lag <= 4.0 * rec.size,
            format!("lag {} for size {}", m.lag, rec.size),
        );
        check(m.overshoot < 1e-3, format!("overshoot {}", m.overshoot));
        check(m.vertices >= 3, "empty mesh".into());
        check(
            m.width.0 >= 0.05 && m.width.1 <= rec.size * 3.0,
            format!("width {:?} for size {}", m.width, rec.size),
        );
        if rec.version >= 2 && rec.tool == pendant_core::Tool::Fountain {
            check(rec.has_tilt(), "fountain recording without tilt".into());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
