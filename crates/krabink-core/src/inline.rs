//! Inline sketches: a legacy `sketches` container shown as a box in the
//! text flow at its `![sketch](krabink://sketch/<id>)` line. The box's
//! height comes from the ink alone, so every platform lays the line out
//! the same way; its width is the text container's. Element points are
//! relative to the box's top-left corner plus [`INLINE_PADDING`].

use crate::element::Element;

/// Height of a box with no ink, and the least any box gets.
pub const INLINE_MIN_HEIGHT: f32 = 160.0;
/// Gap between the box edge and the sketch's origin (top-left) and, at
/// the bottom, between the lowest ink and the box edge.
pub const INLINE_PADDING: f32 = 8.0;

/// Axis-aligned bounds `(min, max)` of `points`; `None` when empty.
pub fn points_bounds<'a>(
    points: impl IntoIterator<Item = &'a [f32; 2]>,
) -> Option<([f32; 2], [f32; 2])> {
    let mut bounds: Option<([f32; 2], [f32; 2])> = None;
    for p in points {
        bounds = Some(match bounds {
            None => (*p, *p),
            Some((min, max)) => (
                [min[0].min(p[0]), min[1].min(p[1])],
                [max[0].max(p[0]), max[1].max(p[1])],
            ),
        });
    }
    bounds
}

/// Bounds of the finished ink outlines of `elements` (what the SVG export
/// and the inline box size are measured from); `None` when nothing has ink.
pub fn outline_bounds(elements: &[Element]) -> Option<([f32; 2], [f32; 2])> {
    let outlines: Vec<Vec<[f32; 2]>> = elements
        .iter()
        .map(|el| el.ink().outline(&el.outline()))
        .collect();
    points_bounds(outlines.iter().flatten())
}

/// Height of the inline box holding `elements`: the lowest ink plus the
/// padding above and below it, whole points, never under
/// [`INLINE_MIN_HEIGHT`]. Ink above the origin (negative y) does not count;
/// the box only ever grows downward.
pub fn sketch_box_height(elements: &[Element]) -> f32 {
    let max_y = outline_bounds(elements).map_or(0.0, |(_, max)| max[1]);
    (max_y + 2.0 * INLINE_PADDING).ceil().max(INLINE_MIN_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{ShapeElement, Style};
    use crate::shape::Shape;
    use crate::stroke::{PointKind, Rgba, Stroke, StrokePoint, Tool};
    use crate::{ElementId, StrokeId};

    fn stroke(ys: &[f32]) -> Element {
        Element::Stroke(Stroke {
            id: StrokeId::new(),
            tool: Tool::Monoline,
            brush: None,
            color: Rgba::BLACK,
            base_width: 2.0,
            kind: PointKind::PolylineSample,
            points: ys
                .iter()
                .enumerate()
                .map(|(i, &y)| StrokePoint {
                    x: i as f32 * 10.0,
                    y,
                    force: 1.0,
                    t_ms: i as u32 * 16,
                    tilt: None,
                    size: None,
                })
                .collect(),
            created_ms: 0,
        })
    }

    #[test]
    fn empty_and_small_sketches_get_the_minimum() {
        assert_eq!(sketch_box_height(&[]), INLINE_MIN_HEIGHT);
        assert_eq!(
            sketch_box_height(&[stroke(&[0.0, 20.0, 40.0])]),
            INLINE_MIN_HEIGHT
        );
    }

    #[test]
    fn tall_ink_grows_the_box_in_whole_points() {
        let h = sketch_box_height(&[stroke(&[0.0, 300.0])]);
        assert!(h > 300.0 + 2.0 * INLINE_PADDING, "{h}");
        assert!(h < 300.0 + 2.0 * INLINE_PADDING + 4.0, "{h}");
        assert_eq!(h, h.ceil());
    }

    #[test]
    fn ink_above_the_origin_does_not_count() {
        assert_eq!(
            sketch_box_height(&[stroke(&[-500.0, -10.0])]),
            INLINE_MIN_HEIGHT
        );
    }

    #[test]
    fn shapes_count_too() {
        let rect = Element::Shape(ShapeElement {
            id: ElementId::new(),
            shape: Shape::Rect {
                center: [0.0, 200.0],
                size: [10.0, 400.0],
                angle: 0.0,
            },
            style: Style {
                tool: Tool::Monoline,
                color: Rgba::BLACK,
                width: 2.0,
            },
            start: None,
            end: None,
            created_ms: 0,
        });
        let h = sketch_box_height(&[rect]);
        assert!(h >= 400.0 + 2.0 * INLINE_PADDING, "{h}");
        let (min, max) = outline_bounds(&[stroke(&[5.0, 15.0])]).unwrap();
        assert!(min[1] < 5.0 && max[1] > 15.0);
        assert!(points_bounds(std::iter::empty()).is_none());
    }
}
