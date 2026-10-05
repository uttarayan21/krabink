//! Arrow bindings, Excalidraw style: a line or arrow end bound to a closed
//! shape sits where the ray toward the shape's aim point crosses its
//! outline, pulled back by the binding's gap. The dependency runs one way
//! (target → arrow) and has a closed form, so there is nothing to solve:
//! resolving is deterministic and every peer derives the same endpoints
//! from the same targets.
//!
//! ```
//! use krabink_core::{Binding, Element, ElementId, Rgba, Shape, ShapeElement, Style, Tool, resolve_bindings};
//!
//! let style = Style { tool: Tool::Monoline, color: Rgba::BLACK, width: 2.0 };
//! let rect = |x: f32| ShapeElement {
//!     id: ElementId::new(),
//!     shape: Shape::Rect { center: [x, 0.0], size: [40.0, 40.0], angle: 0.0 },
//!     style, start: None, end: None, created_ms: 0,
//! };
//! let (a, b) = (rect(0.0), rect(200.0));
//! let bind = |target: &ShapeElement| Some(Binding { element: target.id, fixed_point: [0.5, 0.5], gap: 4.0 });
//! let arrow = ShapeElement {
//!     id: ElementId::new(),
//!     shape: Shape::Arrow { a: [0.0, 0.0], b: [200.0, 0.0] },
//!     style, start: bind(&a), end: bind(&b), created_ms: 0,
//! };
//! let mut elements = vec![Element::Shape(a), Element::Shape(b), Element::Shape(arrow)];
//! resolve_bindings(&mut elements, |_| [0.0, 0.0]);
//! let Element::Shape(ShapeElement { shape: Shape::Arrow { a, b }, .. }) = elements[2] else { panic!() };
//! assert!((a[0] - 24.0).abs() < 1e-3 && (b[0] - 176.0).abs() < 1e-3);
//! ```

use std::collections::HashMap;

use crate::element::{Binding, Element, ShapeElement};
use crate::geom::segment_distance2;
use crate::ids::ElementId;
use crate::shape::{
    P, Shape, add, cross, diamond_corners, dot, norm, rect_corners, rotate, scale, sub,
};

/// Distance a new binding keeps from its target's outline, canvas units.
pub const BINDING_GAP: f32 = 4.0;
/// Where new bindings aim: the target's center, so an arrow meets the
/// outline on the side facing its other end wherever the shapes move.
pub const BINDING_CENTER: [f32; 2] = [0.5, 0.5];

/// The point at `fixed_point` of the target's unit square (its unrotated
/// box, 0..=1 on each axis, clamped) in canvas space.
pub fn aim_point(target: &Shape, fixed_point: [f32; 2]) -> P {
    let f = fixed_point.map(|v| {
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.5
        }
    });
    let local = |center: P, size: P, angle: f32| {
        add(
            center,
            rotate([(f[0] - 0.5) * size[0], (f[1] - 0.5) * size[1]], angle),
        )
    };
    match *target {
        Shape::Rect {
            center,
            size,
            angle,
        }
        | Shape::Diamond {
            center,
            size,
            angle,
        } => local(center, size, angle),
        Shape::Ellipse {
            center,
            radii,
            angle,
        } => local(center, scale(radii, 2.0), angle),
        Shape::Line { .. } | Shape::Arrow { .. } => {
            let (lo, hi) = target.bounds();
            [
                lo[0] + f[0] * (hi[0] - lo[0]),
                lo[1] + f[1] * (hi[1] - lo[1]),
            ]
        }
    }
}

/// The corners of a rect or diamond, in outline order.
fn polygon(target: &Shape) -> Option<[P; 4]> {
    match *target {
        Shape::Rect {
            center,
            size,
            angle,
        } => Some(rect_corners(center, size, angle)),
        Shape::Diamond {
            center,
            size,
            angle,
        } => Some(diamond_corners(center, size, angle)),
        _ => None,
    }
}

/// `p` in the frame where the ellipse is the unit circle.
fn unit_frame(center: P, radii: P, angle: f32, p: P) -> P {
    let q = rotate(sub(p, center), -angle);
    [q[0] / radii[0], q[1] / radii[1]]
}

/// Whether `p` lies inside (or on) a closed shape; never for lines.
pub fn contains(target: &Shape, p: P) -> bool {
    if let Some(corners) = polygon(target) {
        let sides: Vec<f32> = (0..4)
            .map(|i| cross(sub(corners[(i + 1) % 4], corners[i]), sub(p, corners[i])))
            .collect();
        return sides.iter().all(|&s| s >= 0.0) || sides.iter().all(|&s| s <= 0.0);
    }
    match *target {
        Shape::Ellipse {
            center,
            radii,
            angle,
        } => radii[0] > 0.0 && radii[1] > 0.0 && norm(unit_frame(center, radii, angle, p)) <= 1.0,
        _ => false,
    }
}

/// Where the segment `from → to` first crosses the outline, nearest `from`.
pub fn outline_hit(target: &Shape, from: P, to: P) -> Option<P> {
    let d = sub(to, from);
    if let Some(corners) = polygon(target) {
        let mut best: Option<f32> = None;
        for i in 0..4 {
            let (e0, e1) = (corners[i], corners[(i + 1) % 4]);
            let e = sub(e1, e0);
            let denom = cross(d, e);
            if denom.abs() <= f32::EPSILON {
                continue;
            }
            let w = sub(e0, from);
            let t = cross(w, e) / denom;
            let u = cross(w, d) / denom;
            if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
                best = Some(best.map_or(t, |b: f32| b.min(t)));
            }
        }
        return best.map(|t| add(from, scale(d, t)));
    }
    match *target {
        Shape::Ellipse {
            center,
            radii,
            angle,
        } => {
            if radii[0] <= 0.0 || radii[1] <= 0.0 {
                return None;
            }
            // |f + s·g|² = 1 in the unit-circle frame; the smaller root in
            // 0..=1 is the first crossing.
            let f = unit_frame(center, radii, angle, from);
            let g = sub(unit_frame(center, radii, angle, to), f);
            let (a, b, c) = (dot(g, g), 2.0 * dot(f, g), dot(f, f) - 1.0);
            if a <= f32::EPSILON {
                return None;
            }
            let disc = b * b - 4.0 * a * c;
            if disc < 0.0 {
                return None;
            }
            let root = disc.sqrt();
            [(-b - root) / (2.0 * a), (-b + root) / (2.0 * a)]
                .into_iter()
                .find(|s| (0.0..=1.0).contains(s))
                .map(|s| add(from, scale(d, s)))
        }
        _ => None,
    }
}

/// A bound end: the outline crossing on the way from `toward` to the
/// binding's aim point, pulled back `gap` toward `toward`. From inside the
/// target (or when the ray misses it) the end sits on the aim point.
pub fn bound_end(target: &Shape, binding: &Binding, toward: P) -> P {
    let aim = aim_point(target, binding.fixed_point);
    if contains(target, toward) {
        return aim;
    }
    let Some(hit) = outline_hit(target, toward, aim) else {
        return aim;
    };
    let back = sub(toward, hit);
    let len = norm(back);
    if len <= f32::EPSILON {
        return hit;
    }
    let gap = binding.gap.max(0.0).min(len);
    add(hit, scale(back, gap / len))
}

/// Re-derive a line's or arrow's bound ends from its targets, looked up
/// already placed in the arrow's own space. An end whose target is missing
/// (or not a closed shape) keeps its stored point. Each bound end aims
/// from the other end's aim point, or from its free point when unbound.
pub fn resolve_arrow(shape: &mut ShapeElement, target_of: impl Fn(ElementId) -> Option<Shape>) {
    let (a, b) = match shape.shape {
        Shape::Line { a, b } | Shape::Arrow { a, b } => (a, b),
        _ => return,
    };
    let lookup = |binding: Option<Binding>| {
        let binding = binding?;
        let target = target_of(binding.element).filter(Shape::is_closed)?;
        Some((target, binding))
    };
    let (start, end) = (lookup(shape.start), lookup(shape.end));
    if start.is_none() && end.is_none() {
        return;
    }
    let from_a = start.map_or(a, |(t, bd)| aim_point(&t, bd.fixed_point));
    let from_b = end.map_or(b, |(t, bd)| aim_point(&t, bd.fixed_point));
    let a = start.map_or(a, |(t, bd)| bound_end(&t, &bd, from_b));
    let b = end.map_or(b, |(t, bd)| bound_end(&t, &bd, from_a));
    shape.shape = match shape.shape {
        Shape::Line { .. } => Shape::Line { a, b },
        _ => Shape::Arrow { a, b },
    };
}

/// Resolve every bound line and arrow in `elements`. `origin_of` gives the
/// origin of each element's coordinate space in a shared one: all zero for
/// a sketch, each anchor line's layout origin for the page layer.
pub fn resolve_bindings(elements: &mut [Element], origin_of: impl Fn(ElementId) -> [f32; 2]) {
    let targets: HashMap<ElementId, Shape> = elements
        .iter()
        .filter_map(|el| match el {
            Element::Shape(s) if s.shape.is_closed() => {
                Some((s.id, s.shape.translated(origin_of(s.id))))
            }
            _ => None,
        })
        .collect();
    for el in elements.iter_mut() {
        if let Element::Shape(s) = el
            && (s.start.is_some() || s.end.is_some())
        {
            let o = origin_of(s.id);
            resolve_arrow(s, |id| {
                targets.get(&id).map(|t| t.translated([-o[0], -o[1]]))
            });
        }
    }
}

/// Whether the element is a line or arrow with an end bound to any of `ids`.
pub fn bound_to(element: &Element, ids: &[ElementId]) -> bool {
    match element {
        Element::Shape(s) => [s.start, s.end]
            .into_iter()
            .flatten()
            .any(|b| ids.contains(&b.element)),
        Element::Stroke(_) => false,
    }
}

/// The binding an arrow end at `p` (shared space, see
/// [`resolve_bindings`]) would take: the topmost closed shape containing
/// `p` or within `reach` of its outline, aimed at its center. `exclude`
/// skips the arrow being drawn.
pub fn binding_at(
    elements: &[Element],
    p: P,
    origin_of: impl Fn(ElementId) -> [f32; 2],
    reach: f32,
    exclude: Option<ElementId>,
) -> Option<Binding> {
    elements.iter().rev().find_map(|el| {
        let Element::Shape(s) = el else {
            return None;
        };
        if !s.shape.is_closed() || Some(s.id) == exclude {
            return None;
        }
        let shape = s.shape.translated(origin_of(s.id));
        let outline: Vec<P> = shape.outline().iter().map(|q| [q.x, q.y]).collect();
        let near = outline
            .windows(2)
            .any(|w| segment_distance2(w[0], w[1], p) <= reach * reach);
        (near || contains(&shape, p)).then_some(Binding {
            element: s.id,
            fixed_point: BINDING_CENTER,
            gap: BINDING_GAP,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Style;
    use crate::shape::dist;
    use crate::stroke::{Rgba, Tool};
    use proptest::prelude::*;

    const STYLE: Style = Style {
        tool: Tool::Monoline,
        color: Rgba::BLACK,
        width: 2.0,
    };

    fn element(shape: Shape) -> ShapeElement {
        ShapeElement {
            id: ElementId::new(),
            shape,
            style: STYLE,
            start: None,
            end: None,
            created_ms: 0,
        }
    }

    fn bind(target: &ShapeElement) -> Option<Binding> {
        Some(Binding {
            element: target.id,
            fixed_point: BINDING_CENTER,
            gap: BINDING_GAP,
        })
    }

    fn rect(center: P) -> Shape {
        Shape::Rect {
            center,
            size: [60.0, 40.0],
            angle: 0.0,
        }
    }

    fn ends(el: &Element) -> (P, P) {
        match el {
            Element::Shape(ShapeElement {
                shape: Shape::Arrow { a, b } | Shape::Line { a, b },
                ..
            }) => (*a, *b),
            other => panic!("not an arrow: {other:?}"),
        }
    }

    fn connected(boxes: [Shape; 2]) -> Vec<Element> {
        let (x, y) = (element(boxes[0]), element(boxes[1]));
        let mut arrow = element(Shape::Arrow {
            a: [0.0, 0.0],
            b: [1.0, 1.0],
        });
        arrow.start = bind(&x);
        arrow.end = bind(&y);
        vec![x.into(), y.into(), arrow.into()]
    }

    #[test]
    fn arrow_follows_a_moved_box() {
        let mut elements = connected([rect([0.0, 0.0]), rect([200.0, 0.0])]);
        resolve_bindings(&mut elements, |_| [0.0, 0.0]);
        let (a, b) = ends(&elements[2]);
        assert!((a[0] - 34.0).abs() < 1e-3 && a[1].abs() < 1e-3, "{a:?}");
        assert!((b[0] - 166.0).abs() < 1e-3 && b[1].abs() < 1e-3, "{b:?}");

        // Move the second box below the first: the arrow now leaves the
        // bottom edge and enters the top one.
        let Element::Shape(s) = &mut elements[1] else {
            unreachable!()
        };
        s.shape = rect([0.0, 200.0]);
        resolve_bindings(&mut elements, |_| [0.0, 0.0]);
        let (a, b) = ends(&elements[2]);
        assert!(a[0].abs() < 1e-3 && (a[1] - 24.0).abs() < 1e-3, "{a:?}");
        assert!(b[0].abs() < 1e-3 && (b[1] - 176.0).abs() < 1e-3, "{b:?}");
    }

    #[test]
    fn origins_place_elements_on_different_lines() {
        // Same layout as above, but the second box and the arrow are
        // anchored to lines whose origins differ.
        let mut elements = connected([rect([0.0, 0.0]), rect([200.0, -100.0])]);
        let ids: Vec<ElementId> = elements.iter().map(Element::id).collect();
        let origin = |id: ElementId| {
            if id == ids[1] {
                [0.0, 100.0]
            } else if id == ids[2] {
                [10.0, 50.0]
            } else {
                [0.0, 0.0]
            }
        };
        resolve_bindings(&mut elements, origin);
        let (a, b) = ends(&elements[2]);
        // Arrow space is shifted by (10, 50).
        assert!(
            (a[0] - 24.0).abs() < 1e-3 && (a[1] + 50.0).abs() < 1e-3,
            "{a:?}"
        );
        assert!(
            (b[0] - 156.0).abs() < 1e-3 && (b[1] + 50.0).abs() < 1e-3,
            "{b:?}"
        );
    }

    #[test]
    fn one_bound_end_aims_from_the_free_end() {
        let target = element(Shape::Ellipse {
            center: [100.0, 0.0],
            radii: [20.0, 20.0],
            angle: 0.0,
        });
        let mut arrow = element(Shape::Arrow {
            a: [0.0, 0.0],
            b: [100.0, 0.0],
        });
        arrow.end = bind(&target);
        let mut elements = vec![target.into(), arrow.into()];
        resolve_bindings(&mut elements, |_| [0.0, 0.0]);
        let (a, b) = ends(&elements[1]);
        assert_eq!(a, [0.0, 0.0]);
        assert!((b[0] - 76.0).abs() < 1e-3 && b[1].abs() < 1e-3, "{b:?}");
    }

    #[test]
    fn overlapping_targets_collapse_to_the_aim_points() {
        let mut elements = connected([rect([0.0, 0.0]), rect([10.0, 0.0])]);
        resolve_bindings(&mut elements, |_| [0.0, 0.0]);
        assert_eq!(ends(&elements[2]), ([0.0, 0.0], [10.0, 0.0]));
    }

    #[test]
    fn missing_targets_leave_the_arrow_alone() {
        let mut elements = connected([rect([0.0, 0.0]), rect([200.0, 0.0])]);
        elements.remove(0);
        elements.remove(0);
        resolve_bindings(&mut elements, |_| [0.0, 0.0]);
        assert_eq!(ends(&elements[0]), ([0.0, 0.0], [1.0, 1.0]));
    }

    #[test]
    fn binding_at_picks_the_topmost_shape_near_the_pen() {
        let low = element(rect([0.0, 0.0]));
        let high = element(Shape::Diamond {
            center: [10.0, 0.0],
            size: [40.0, 40.0],
            angle: 0.0,
        });
        let elements: Vec<Element> = vec![low.into(), high.into()];
        let hit = |p: P| binding_at(&elements, p, |_| [0.0, 0.0], 6.0, None).map(|b| b.element);
        assert_eq!(hit([10.0, 0.0]), Some(high.id));
        assert_eq!(hit([-28.0, 0.0]), Some(low.id));
        assert_eq!(hit([-35.0, 0.0]), Some(low.id), "within reach outside");
        assert_eq!(hit([-45.0, 0.0]), None);
        assert_eq!(
            binding_at(&elements, [10.0, 0.0], |_| [0.0, 0.0], 6.0, Some(high.id))
                .map(|b| b.element),
            Some(low.id)
        );
    }

    fn on_outline(shape: &Shape, p: P) -> bool {
        match *shape {
            Shape::Ellipse {
                center,
                radii,
                angle,
            } => (norm(unit_frame(center, radii, angle, p)) - 1.0).abs() < 1e-3,
            _ => {
                let c = polygon(shape).expect("closed");
                (0..4).any(|i| segment_distance2(c[i], c[(i + 1) % 4], p) < 1e-3)
            }
        }
    }

    fn arb_closed() -> impl Strategy<Value = Shape> {
        let center = (-300.0f32..300.0, -300.0f32..300.0).prop_map(|(x, y)| [x, y]);
        let size = (10.0f32..200.0, 10.0f32..200.0).prop_map(|(w, h)| [w, h]);
        (center, size, -3.2f32..3.2, 0..3).prop_map(|(center, size, angle, kind)| match kind {
            0 => Shape::Rect {
                center,
                size,
                angle,
            },
            1 => Shape::Diamond {
                center,
                size,
                angle,
            },
            _ => Shape::Ellipse {
                center,
                radii: scale(size, 0.5),
                angle,
            },
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn bound_ends_keep_the_gap_from_the_outline(
            target in arb_closed(),
            angle in 0.0f32..core::f32::consts::TAU,
            far in 400.0f32..1000.0,
        ) {
            let binding = Binding { element: ElementId::new(), fixed_point: BINDING_CENTER, gap: BINDING_GAP };
            let aim = aim_point(&target, BINDING_CENTER);
            let toward = add(aim, rotate([far, 0.0], angle));
            let end = bound_end(&target, &binding, toward);
            prop_assert!(!contains(&target, end), "end {end:?} inside {target:?}");
            // One gap further toward the aim point is the outline.
            let dir = sub(aim, end);
            let hit = add(end, scale(dir, BINDING_GAP / norm(dir)));
            prop_assert!(on_outline(&target, hit), "{hit:?} off {target:?}");
        }

        #[test]
        fn bound_ends_follow_a_turned_or_scaled_target(
            target in arb_closed(),
            angle in 0.0f32..core::f32::consts::TAU,
            by in -3.0f32..3.0,
            // Not below 1: `on_outline`'s tolerance is too tight for tiny
            // ellipses hit from far away.
            k in 1.0f32..3.0,
            far in 400.0f32..1000.0,
        ) {
            let binding = Binding { element: ElementId::new(), fixed_point: BINDING_CENTER, gap: BINDING_GAP };
            let pivot = add(target.frame().center, [50.0, -20.0]);
            for target in [target.rotated(pivot, by, None), target.scaled(pivot, k)] {
                let aim = aim_point(&target, BINDING_CENTER);
                let toward = add(aim, rotate([far, 0.0], angle));
                let end = bound_end(&target, &binding, toward);
                prop_assert!(!contains(&target, end), "end {end:?} inside {target:?}");
                let dir = sub(aim, end);
                let hit = add(end, scale(dir, BINDING_GAP / norm(dir)));
                prop_assert!(on_outline(&target, hit), "{hit:?} off {target:?}");
            }
        }

        #[test]
        fn resolving_commutes_with_translation(
            x in arb_closed(),
            y in arb_closed(),
            d in (-500.0f32..500.0, -500.0f32..500.0),
        ) {
            let d = [d.0, d.1];
            let mut here = connected([x, y]);
            let mut there = connected([x.translated(d), y.translated(d)]);
            if let (Element::Shape(s), Element::Shape(t)) = (&here[0], &mut there[0]) { t.id = s.id; }
            if let (Element::Shape(s), Element::Shape(t)) = (&here[1], &mut there[1]) { t.id = s.id; }
            if let (Element::Shape(s), Element::Shape(t)) = (&here[2], &mut there[2]) {
                t.start = s.start;
                t.end = s.end;
            }
            resolve_bindings(&mut here, |_| [0.0, 0.0]);
            resolve_bindings(&mut there, |_| [0.0, 0.0]);
            let ((a0, b0), (a1, b1)) = (ends(&here[2]), ends(&there[2]));
            for (p, q) in [(a0, a1), (b0, b1)] {
                prop_assert!(dist(add(p, d), q) < 0.05, "{p:?} + {d:?} != {q:?}");
            }
        }

        #[test]
        fn from_inside_an_end_sits_on_the_aim_point(target in arb_closed()) {
            let binding = Binding { element: ElementId::new(), fixed_point: BINDING_CENTER, gap: BINDING_GAP };
            let aim = aim_point(&target, BINDING_CENTER);
            prop_assert_eq!(bound_end(&target, &binding, aim), aim);
        }
    }
}
