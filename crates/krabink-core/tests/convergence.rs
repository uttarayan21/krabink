//! Property test: two replicas applying arbitrary interleaved edits and
//! exchanging updates in arbitrary order always converge.

use krabink_core::{
    Binding, Element, ElementId, ElementMove, NoteDoc, NoteId, PointKind, Rgba, Shape,
    ShapeElement, SketchId, Stroke, StrokeId, StrokePoint, Style, Tool,
};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Insert {
        at: usize,
        text: String,
    },
    Delete {
        at: usize,
        len: usize,
    },
    AddStroke {
        seed: u32,
    },
    AddShape {
        seed: u32,
    },
    AddPageStroke {
        seed: u32,
        at: usize,
    },
    RemovePage {
        nth: usize,
    },
    MoveSketch {
        nth: usize,
        dx: f32,
        dy: f32,
    },
    MovePage {
        nth: usize,
        dx: f32,
        dy: f32,
        reanchor: Option<usize>,
    },
    Connect {
        from: usize,
        to: usize,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..64, "[a-z #\n]{1,8}").prop_map(|(at, text)| Op::Insert { at, text }),
        (0usize..64, 1usize..8).prop_map(|(at, len)| Op::Delete { at, len }),
        (0u32..1000).prop_map(|seed| Op::AddStroke { seed }),
        (0u32..1000).prop_map(|seed| Op::AddShape { seed }),
        (0u32..1000, 0usize..64).prop_map(|(seed, at)| Op::AddPageStroke { seed, at }),
        (0usize..8).prop_map(|nth| Op::RemovePage { nth }),
        (0usize..8, -50.0f32..50.0, -50.0f32..50.0).prop_map(|(nth, dx, dy)| Op::MoveSketch {
            nth,
            dx,
            dy
        }),
        (
            0usize..8,
            -50.0f32..50.0,
            -50.0f32..50.0,
            proptest::option::of(0usize..64)
        )
            .prop_map(|(nth, dx, dy, reanchor)| Op::MovePage {
                nth,
                dx,
                dy,
                reanchor
            }),
        (0usize..8, 0usize..8).prop_map(|(from, to)| Op::Connect { from, to }),
    ]
}

fn sample_stroke(seed: u32) -> Stroke {
    let points = (0..10)
        .map(|i| StrokePoint {
            x: (seed as f32) + i as f32,
            y: (seed % 37) as f32 * 2.0,
            force: 0.5,
            t_ms: i * 8,
            tilt: None,
            size: None,
        })
        .collect();
    Stroke {
        id: StrokeId::new(),
        tool: Tool::Pen,
        brush: None,
        color: Rgba::BLACK,
        base_width: 2.0,
        kind: PointKind::PolylineSample,
        points,
        created_ms: u64::from(seed),
    }
}

const STYLE: Style = Style {
    tool: Tool::Pen,
    color: Rgba::BLACK,
    width: 2.0,
};

fn shape_element(shape: Shape, created_ms: u64) -> ShapeElement {
    ShapeElement {
        id: ElementId::new(),
        shape,
        style: STYLE,
        start: None,
        end: None,
        created_ms,
    }
}

fn bind(element: ElementId) -> Option<Binding> {
    Some(Binding {
        element,
        fixed_point: [0.5, 0.5],
        gap: 4.0,
    })
}

fn apply(doc: &NoteDoc, sketch: SketchId, op: &Op) {
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
            doc.add_stroke(sketch, &sample_stroke(*seed)).unwrap();
        }
        Op::AddPageStroke { seed, at } => {
            let anchor = doc.anchor_at((*at).min(doc.text_len())).unwrap();
            doc.add_page_stroke(&sample_stroke(*seed), &anchor).unwrap();
        }
        Op::RemovePage { nth } => {
            let page = doc.page_elements();
            if let Some(entry) = page.get(*nth % page.len().max(1)) {
                doc.remove_page_element(entry.element.id()).unwrap();
            }
        }
        Op::MoveSketch { nth, dx, dy } => {
            let elements = doc.elements(sketch).unwrap();
            if let Some(el) = elements.get(*nth % elements.len().max(1)) {
                doc.move_elements(sketch, &[el.id()], [*dx, *dy]).unwrap();
            }
        }
        Op::MovePage {
            nth,
            dx,
            dy,
            reanchor,
        } => {
            let page = doc.page_elements();
            if let Some(entry) = page.get(*nth % page.len().max(1)) {
                let anchor = reanchor.map(|at| doc.anchor_at(at.min(doc.text_len())).unwrap());
                let moves = [ElementMove {
                    id: entry.element.id(),
                    delta: [*dx, *dy],
                    anchor,
                }];
                doc.move_page_elements(&moves, |_| [0.0, 0.0]).unwrap();
            }
        }
        Op::Connect { from, to } => {
            let closed: Vec<ElementId> = doc
                .elements(sketch)
                .unwrap()
                .into_iter()
                .filter_map(|el| match el {
                    Element::Shape(s) if s.shape.is_closed() => Some(s.id),
                    _ => None,
                })
                .collect();
            if closed.is_empty() {
                return;
            }
            let arrow = ShapeElement {
                start: bind(closed[*from % closed.len()]),
                end: bind(closed[*to % closed.len()]),
                ..shape_element(
                    Shape::Arrow {
                        a: [0.0, 0.0],
                        b: [1.0, 1.0],
                    },
                    0,
                )
            };
            doc.add_shape(sketch, &arrow).unwrap();
        }
        Op::AddShape { seed } => {
            let s = *seed as f32;
            let shape = match seed % 5 {
                0 => Shape::Line {
                    a: [s, 0.0],
                    b: [s + 10.0, 5.0],
                },
                1 => Shape::Arrow {
                    a: [0.0, s],
                    b: [20.0, s],
                },
                2 => Shape::Rect {
                    center: [s, s],
                    size: [30.0, 20.0],
                    angle: 0.1,
                },
                3 => Shape::Diamond {
                    center: [0.0, s],
                    size: [30.0, 20.0],
                    angle: 0.0,
                },
                _ => Shape::Ellipse {
                    center: [s, 0.0],
                    radii: [15.0, 10.0],
                    angle: 0.0,
                },
            };
            doc.add_shape(sketch, &shape_element(shape, u64::from(*seed)))
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

        // Shared baseline so both replicas know the sketch container, and
        // two boxes joined by an arrow for concurrent moves to pull apart.
        let sketch = a.create_sketch(0).unwrap();
        a.splice_text(0, 0, "baseline text\n").unwrap();
        let rect = |x: f32| shape_element(Shape::Rect { center: [x, 0.0], size: [40.0, 40.0], angle: 0.0 }, 0);
        let (x, y) = (rect(0.0), rect(200.0));
        a.add_shape(sketch, &x).unwrap();
        a.add_shape(sketch, &y).unwrap();
        let arrow = ShapeElement {
            start: bind(x.id),
            end: bind(y.id),
            ..shape_element(Shape::Arrow { a: [20.0, 0.0], b: [180.0, 0.0] }, 0)
        };
        a.add_shape(sketch, &arrow).unwrap();
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
        prop_assert_eq!(a.elements(sketch).unwrap(), b.elements(sketch).unwrap());
        let (page_a, page_b) = (a.page_elements(), b.page_elements());
        prop_assert_eq!(&page_a, &page_b);
        // Anchors resolve to the same line on both replicas.
        let at_a: Vec<_> = page_a.iter().map(|p| a.resolve_anchor(&p.anchor)).collect();
        let at_b: Vec<_> = page_b.iter().map(|p| b.resolve_anchor(&p.anchor)).collect();
        prop_assert_eq!(at_a, at_b);
    }
}
