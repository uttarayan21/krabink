//! Replay recorded pen strokes through the brush model and the shape
//! recognizer. Each `tests/corpus/shapes/<name>.txt` holds one stroke as
//! the iPad recorder writes it with `-recordStrokes 1`:
//!
//! `# expect: rect|ellipse|line|arrow|none` header, then the format
//! `pendant_core::corpus` reads. A `# kind: modeled` header marks points
//! that already went through the brush model (a stroke dumped by the app
//! at hold time); they are fed to the recognizer as they are.
//!
//! Name recordings by what they are (`rect-hand-03`, `letter-s-01`) and
//! copy them in by hand; every file is asserted.

use std::path::Path;

use pendant_core::{BrushModeler, Shape, StrokePoint, corpus, recognize};

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
        let case = corpus::parse(&std::fs::read_to_string(path).expect("read corpus file"))
            .expect("parse corpus file");
        let expect = case.expect.as_deref().expect("# expect: header");
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
        if got != expect {
            failures.push(format!(
                "{}: expected {}, got {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                expect,
                got
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
