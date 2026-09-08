//! Property test: two replicas applying arbitrary interleaved edits and
//! exchanging updates in arbitrary order always converge.

use pendant_core::{NoteDoc, NoteId, PointKind, Rgba, Stroke, StrokeId, StrokePoint, Tool};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Insert { at: usize, text: String },
    Delete { at: usize, len: usize },
    AddStroke { seed: u32 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..64, "[a-z #\n]{1,8}").prop_map(|(at, text)| Op::Insert { at, text }),
        (0usize..64, 1usize..8).prop_map(|(at, len)| Op::Delete { at, len }),
        (0u32..1000).prop_map(|seed| Op::AddStroke { seed }),
    ]
}

fn apply(doc: &NoteDoc, sketch: pendant_core::SketchId, op: &Op) {
    match op {
        Op::Insert { at, text } => {
            let len = doc.text_len();
            doc.splice_text(at.min(&len).to_owned(), 0, text).unwrap();
        }
        Op::Delete { at, len } => {
            let text_len = doc.text_len();
            let at = (*at).min(text_len);
            let len = (*len).min(text_len - at);
            if len > 0 {
                doc.splice_text(at, len, "").unwrap();
            }
        }
        Op::AddStroke { seed } => {
            let points = (0..10)
                .map(|i| StrokePoint {
                    x: (*seed as f32) + i as f32,
                    y: (*seed % 37) as f32 * 2.0,
                    force: 0.5,
                    t_ms: i * 8,
                    tilt: None,
                    size: None,
                })
                .collect();
            doc.add_stroke(
                sketch,
                &Stroke {
                    id: StrokeId::new(),
                    tool: Tool::Pen,
                    color: Rgba::BLACK,
                    base_width: 2.0,
                    kind: PointKind::PolylineSample,
                    points,
                    created_ms: u64::from(*seed),
                },
            )
            .unwrap();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn replicas_converge(
        ops_a in prop::collection::vec(op_strategy(), 0..12),
        ops_b in prop::collection::vec(op_strategy(), 0..12),
        b_first in any::<bool>(),
    ) {
        let id = NoteId::new();
        let a = NoteDoc::new(id);

        // Shared baseline so both replicas know the sketch container.
        let sketch = a.create_sketch(0).unwrap();
        a.splice_text(0, 0, "baseline text\n").unwrap();
        let b = NoteDoc::new(id);
        b.import_update(&a.export_updates_since(&[]).unwrap()).unwrap();

        let (vv_a, vv_b) = (a.version(), b.version());
        for op in &ops_a { apply(&a, sketch, op); }
        for op in &ops_b { apply(&b, sketch, op); }

        // Exchange deltas in either order; import must be order-insensitive.
        let update_a = a.export_updates_since(&vv_a).unwrap();
        let update_b = b.export_updates_since(&vv_b).unwrap();
        if b_first {
            a.import_update(&update_b).unwrap();
            b.import_update(&update_a).unwrap();
        } else {
            b.import_update(&update_a).unwrap();
            a.import_update(&update_b).unwrap();
        }

        prop_assert_eq!(a.text(), b.text());
        prop_assert_eq!(a.strokes(sketch).unwrap(), b.strokes(sketch).unwrap());
    }
}
