//! Golden meshes: a fixed set of strokes tessellated through the whole
//! pipeline (input model, tip evaluator, tessellation), hashed and compared
//! with `tests/golden/mesh.txt`. Any change to ink geometry shows up here
//! first; when the change is intended, regenerate with
//! `PENDANT_UPDATE_GOLDEN=1 cargo test -p pendant-core --test golden_mesh`
//! and review the diff of the golden file alongside the code.
//!
//! Hashes are bit-exact within one binary. Cross-platform runs compare
//! vertices at 1e-4 instead (see `docs/plans/brush-engine.md`).

use std::path::Path;

use pendant_core::{BrushModeler, DEFAULT_TOLERANCE, Ink, InkMesh, RawSample, Rgba, Tilt, Tool};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/mesh.txt");

fn tilt(azimuth: f32) -> Option<Tilt> {
    Some(Tilt {
        azimuth,
        altitude: 0.7,
        roll: 0.1,
    })
}

/// Deterministic strokes covering the tools: a fast S-curve with varying
/// force, a slow straight line, a dot, a hairpin, and a nib stroke that
/// turns against its orientation.
fn cases() -> Vec<(&'static str, Tool, f32, Vec<RawSample>)> {
    let s_curve = |force_swing: f32| -> Vec<RawSample> {
        (0..60)
            .map(|i| {
                let t = i as f32 / 59.0;
                RawSample {
                    x: t * 200.0,
                    y: (t * 6.0).sin() * 30.0,
                    force: 0.5 + force_swing * (t * 3.0).cos() * 0.5,
                    t_ms: 1000.0 + f64::from(i) * (4.0 + 8.0 * f64::from(t)),
                    tilt: tilt(t * 3.0),
                    estimate: None,
                }
            })
            .collect()
    };
    let line = |n: u32, step: f32, dt: f64| -> Vec<RawSample> {
        (0..n)
            .map(|i| RawSample {
                x: i as f32 * step,
                y: 10.0,
                force: 0.8,
                t_ms: 500.0 + f64::from(i) * dt,
                tilt: None,
                estimate: None,
            })
            .collect()
    };
    let hairpin: Vec<RawSample> = [
        (0.0, 0.0),
        (20.0, 0.0),
        (40.0, 0.0),
        (20.0, 1.0),
        (0.0, 2.0),
    ]
    .iter()
    .enumerate()
    .map(|(i, &(x, y))| RawSample {
        x,
        y,
        force: 1.0,
        t_ms: f64::from(u32::try_from(i).unwrap_or(0)) * 8.0,
        tilt: None,
        estimate: None,
    })
    .collect();
    vec![
        ("pen-s-curve", Tool::Pen, 4.0, s_curve(1.0)),
        ("pen-slow-line", Tool::Pen, 3.0, line(30, 1.0, 16.0)),
        ("pen-dot", Tool::Pen, 5.0, vec![line(1, 0.0, 0.0)[0]]),
        ("pen-hairpin", Tool::Pen, 4.0, hairpin.clone()),
        ("pencil-s-curve", Tool::Pencil, 3.0, s_curve(0.6)),
        ("marker-s-curve", Tool::Marker, 10.0, s_curve(0.0)),
        ("monoline-line", Tool::Monoline, 2.0, line(30, 3.0, 8.0)),
        ("fountain-s-curve", Tool::Fountain, 12.0, s_curve(0.3)),
    ]
}

fn mesh(tool: Tool, size: f32, samples: &[RawSample]) -> InkMesh {
    let mut modeler = BrushModeler::new(tool, size);
    for &s in samples {
        modeler.push(s);
    }
    Ink::preset(tool, Rgba::BLACK, size).mesh(
        &modeler.finish(),
        pendant_core::StrokeEnd::Complete,
        DEFAULT_TOLERANCE,
    )
}

/// FNV-1a over every vertex component's bits and every index.
fn fingerprint(mesh: &InkMesh) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: [u8; 4]| {
        for b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for v in &mesh.vertices {
        for c in v.pos.iter().chain(&v.uv).chain([&v.opacity]) {
            eat(c.to_bits().to_le_bytes());
        }
    }
    for i in &mesh.indices {
        eat(i.to_le_bytes());
    }
    h
}

#[test]
fn golden_meshes_match() {
    let actual: Vec<String> = cases()
        .iter()
        .map(|(name, tool, size, samples)| {
            let m = mesh(*tool, *size, samples);
            format!(
                "{name} vertices={} indices={} fnv={:016x}",
                m.vertices.len(),
                m.indices.len(),
                fingerprint(&m)
            )
        })
        .collect();
    let text = actual.join("\n") + "\n";
    if std::env::var_os("PENDANT_UPDATE_GOLDEN").is_some() {
        std::fs::write(GOLDEN, &text).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(Path::new(GOLDEN))
        .expect("tests/golden/mesh.txt exists; run with PENDANT_UPDATE_GOLDEN=1 to create it");
    let mismatched: Vec<String> = expected
        .lines()
        .zip(actual.iter())
        .filter(|(e, a)| *e != a.as_str())
        .map(|(e, a)| format!("  expected: {e}\n  actual:   {a}"))
        .collect();
    assert!(
        mismatched.is_empty() && expected.lines().count() == actual.len(),
        "golden meshes changed; if intended, regenerate with PENDANT_UPDATE_GOLDEN=1\n{}",
        mismatched.join("\n")
    );
}
