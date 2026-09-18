//! Replay recorded pen strokes through the brush model and the shape
//! recognizer. Each `tests/corpus/shapes/<name>.txt` holds one stroke as
//! the iPad recorder writes it with `-recordStrokes 1`:
//!
//! ```text
//! # expect: rect|ellipse|line|arrow|none
//! # tool: pen size: 4
//! x y force t_ms
//! …
//! ```
//!
//! A `# kind: modeled` header marks points that already went through the
//! brush model (a stroke dumped by the app at hold time); they are fed to
//! the recognizer as they are.
//!
//! Name recordings by what they are (`rect-hand-03`, `letter-s-01`) and
//! copy them in by hand; every file is asserted.

use std::path::Path;

use pendant_core::{BrushModeler, RawSample, Shape, StrokePoint, Tool, recognize};

struct Case {
    expect: String,
    tool: Tool,
    size: f32,
    modeled: bool,
    samples: Vec<RawSample>,
}

fn parse(text: &str) -> Case {
    let mut expect = None;
    let mut tool = Tool::Pen;
    let mut size = 4.0;
    let mut modeled = false;
    let mut samples = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("# expect:") {
            expect = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("# kind:") {
            modeled = rest.trim() == "modeled";
        } else if let Some(rest) = line.strip_prefix("# tool:") {
            let mut words = rest.split_whitespace();
            tool = words
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(Tool::Pen);
            if words.next() == Some("size:") {
                size = words.next().and_then(|s| s.parse().ok()).unwrap_or(4.0);
            }
        } else if !line.starts_with('#') {
            let f: Vec<f32> = line
                .split_whitespace()
                .map(|v| v.parse().expect("number"))
                .collect();
            samples.push(RawSample {
                x: f[0],
                y: f[1],
                force: f[2],
                t_ms: f64::from(f[3]),
                tilt: None,
                estimate: None,
            });
        }
    }
    Case {
        expect: expect.expect("# expect: header"),
        tool,
        size,
        modeled,
        samples,
    }
}

fn variant(shape: Option<Shape>) -> &'static str {
    match shape {
        Some(Shape::Line { .. }) => "line",
        Some(Shape::Arrow { .. }) => "arrow",
        Some(Shape::Rect { .. }) => "rect",
        Some(Shape::Ellipse { .. }) => "ellipse",
        None => "none",
    }
}

#[test]
fn recorded_strokes_recognise_as_expected() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/shapes");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus files in {}", dir.display());
    let mut failures = Vec::new();
    for path in &files {
        let case = parse(&std::fs::read_to_string(path).expect("read corpus file"));
        let points: Vec<StrokePoint> = if case.modeled {
            case.samples
                .iter()
                .map(|s| StrokePoint {
                    x: s.x,
                    y: s.y,
                    force: s.force,
                    // f64 -> u32 has no TryFrom; corpus timestamps are small.
                    t_ms: s.t_ms.max(0.0) as u32, // ast-grep-ignore: no-as-cast
                    tilt: None,
                    size: None,
                })
                .collect()
        } else {
            let mut modeler = BrushModeler::new(case.tool, case.size);
            for &s in &case.samples {
                modeler.push(s);
            }
            modeler.points().to_vec()
        };
        let got = variant(recognize(&points).map(|r| r.shape));
        if got != case.expect {
            failures.push(format!(
                "{}: expected {}, got {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                case.expect,
                got
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
