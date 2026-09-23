//! Stamped tips: the tip's rectangle laid down as one quad per dab at
//! even intervals along the path, each carrying its tip space in `uv` so
//! the fragment shader can cut the rounded shape and feather its edge.
//! Nothing here depends on zoom: the same quads are exact at every
//! magnification. Jitter comes from [`rng`], a pure function of the
//! stroke's seed and the dab's index, so a stroke drawn point by point
//! lays exactly the dabs the committed stroke lays.

use super::{InkMesh, InkStyle, InkVertex};
use crate::brush::{MIN_TIP, Stamped, TipState, rng};

/// Ceiling on dabs per mesh: a stroke long enough to hit it is a runaway
/// (a spacing far too fine for its width), and the renderer is better off
/// with a truncated stroke than a frozen frame.
pub const MAX_DABS: usize = 50_000;

/// Floor on the spacing in sizes, whatever the spec says.
const MIN_SPACING: f32 = 0.02;

/// Random channels per dab.
const SCATTER_X: u32 = 0;
const SCATTER_Y: u32 = 1;
const ROTATION: u32 = 2;
const SIZE: u32 = 3;
const OPACITY: u32 = 4;

/// Dabs along `pts`. `along_motion` says the tip is `Orient::Motion`:
/// its width then lies across the direction of travel (as on a ribbon),
/// otherwise the tip state's rotation is already the canvas angle.
pub(super) fn stamped_mesh(
    pts: &[([f32; 2], TipState)],
    emit: &Stamped,
    base_width: f32,
    seed: u32,
    along_motion: bool,
    style: InkStyle,
) -> InkMesh {
    let mut mesh = InkMesh::empty(style);
    mesh.zoom_independent = true;
    let Some(&(first, first_tip)) = pts.first() else {
        return mesh;
    };
    let spacing = (emit.spacing.max(MIN_SPACING) * base_width).max(MIN_TIP);
    let mut dabs = Dabs {
        mesh: &mut mesh,
        emit,
        base_width,
        seed,
        along_motion,
        count: 0,
    };
    let first_dir = pts
        .iter()
        .skip(1)
        .map(|&(p, _)| p)
        .find(|p| *p != first)
        .map_or(0.0, |p| (p[1] - first[1]).atan2(p[0] - first[0]));
    dabs.push(first, &first_tip, first_dir);
    // Distance still to travel before the next dab, carried across segments
    // so the rhythm does not restart at every input point.
    let mut until_next = spacing;
    for w in pts.windows(2) {
        let ((a, ta), (b, tb)) = (w[0], w[1]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = dx.hypot(dy);
        if len <= 0.0 {
            continue;
        }
        let dir = dy.atan2(dx);
        let mut s = until_next;
        while s <= len {
            let t = s / len;
            let c = [a[0] + dx * t, a[1] + dy * t];
            let tip = TipState {
                w: lerp(ta.w, tb.w, t),
                h: lerp(ta.h, tb.h, t),
                rot: lerp(ta.rot, tb.rot, t),
                opacity: lerp(ta.opacity, tb.opacity, t),
                arc: lerp(ta.arc, tb.arc, t),
            };
            if !dabs.push(c, &tip, dir) {
                tracing::warn!(MAX_DABS, "stamped stroke truncated");
                return mesh;
            }
            s += spacing;
        }
        until_next = s - len;
    }
    mesh
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

struct Dabs<'a> {
    mesh: &'a mut InkMesh,
    emit: &'a Stamped,
    base_width: f32,
    seed: u32,
    along_motion: bool,
    count: usize,
}

impl Dabs<'_> {
    /// One dab at `c`; `dir` is the path direction there. `false` once the
    /// mesh is full.
    fn push(&mut self, c: [f32; 2], tip: &TipState, dir: f32) -> bool {
        if self.count >= MAX_DABS {
            return false;
        }
        let i = u32::try_from(self.count).unwrap_or(u32::MAX);
        self.count += 1;
        let (seed, e) = (self.seed, self.emit);
        let scatter = e.scatter * self.base_width;
        let c = [
            c[0] + rng::signed(seed, i, SCATTER_X) * scatter,
            c[1] + rng::signed(seed, i, SCATTER_Y) * scatter,
        ];
        let mut rot = tip.rot + rng::signed(seed, i, ROTATION) * e.rotation_jitter;
        if self.along_motion {
            rot += dir + core::f32::consts::FRAC_PI_2;
        }
        let size = 1.0 + rng::signed(seed, i, SIZE) * e.size_jitter;
        let (l, t) = (
            (tip.w * size / 2.0).max(MIN_TIP),
            (tip.h * size / 2.0).max(MIN_TIP),
        );
        let opacity = tip.opacity * (1.0 - rng::unit(seed, i, OPACITY) * e.opacity_jitter);
        let (ux, uy) = (rot.cos(), rot.sin());
        let base = u32::try_from(self.mesh.vertices.len()).unwrap_or(u32::MAX);
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            self.mesh.vertices.push(InkVertex {
                pos: [
                    c[0] + ux * l * sx - uy * t * sy,
                    c[1] + uy * l * sx + ux * t * sy,
                ],
                uv: [sx, sy],
                opacity: opacity.clamp(0.0, 1.0),
            });
        }
        self.mesh
            .indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::MaskStyle;

    fn tip(w: f32, arc: f32) -> TipState {
        TipState {
            w,
            h: w,
            rot: 0.0,
            opacity: 1.0,
            arc,
        }
    }

    fn plain() -> Stamped {
        Stamped {
            spacing: 0.25,
            scatter: 0.0,
            rotation_jitter: 0.0,
            size_jitter: 0.0,
            opacity_jitter: 0.0,
        }
    }

    fn dab_count(mesh: &InkMesh) -> usize {
        assert_eq!(mesh.indices.len() % 6, 0);
        assert_eq!(mesh.vertices.len(), mesh.indices.len() / 6 * 4);
        mesh.indices.len() / 6
    }

    #[test]
    fn one_point_is_one_dab_in_tip_space() {
        let mesh = stamped_mesh(
            &[([5.0, 5.0], tip(4.0, 0.0))],
            &plain(),
            4.0,
            1,
            true,
            InkStyle::PLAIN,
        );
        assert_eq!(dab_count(&mesh), 1);
        assert!(mesh.zoom_independent);
        let uvs: Vec<[f32; 2]> = mesh.vertices.iter().map(|v| v.uv).collect();
        assert_eq!(uvs, [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]]);
        for v in &mesh.vertices {
            assert!((v.pos[0] - 5.0).abs() <= 2.0 + 1e-5 && (v.pos[1] - 5.0).abs() <= 2.0 + 1e-5);
        }
    }

    #[test]
    fn spacing_walks_arc_length_across_segments() {
        // 10 units of path at spacing 1 (0.25 × width 4): first dab plus ten.
        let pts = [
            ([0.0, 0.0], tip(4.0, 0.0)),
            ([4.5, 0.0], tip(4.0, 4.5)),
            ([10.0, 0.0], tip(4.0, 10.0)),
        ];
        let mesh = stamped_mesh(&pts, &plain(), 4.0, 1, true, InkStyle::PLAIN);
        assert_eq!(dab_count(&mesh), 11);
        let xs: Vec<f32> = mesh
            .vertices
            .chunks(4)
            .map(|q| (q[0].pos[0] + q[2].pos[0]) / 2.0)
            .collect();
        for (i, x) in xs.iter().enumerate() {
            assert!((x - i as f32).abs() < 1e-4, "dab {i} at {x}");
        }
    }

    #[test]
    fn spacing_is_floored_and_dabs_are_capped() {
        let emit = Stamped {
            spacing: 0.0,
            ..plain()
        };
        let pts = [([0.0, 0.0], tip(1.0, 0.0)), ([100.0, 0.0], tip(1.0, 100.0))];
        let mesh = stamped_mesh(&pts, &emit, 1.0, 1, true, InkStyle::PLAIN);
        // Floor: 0.02 sizes at width 1 is MIN_TIP-limited; 100 units of
        // path would want 2000 dabs at 0.05 or 5000 at 0.02, both under the cap.
        assert!(dab_count(&mesh) > 1000 && dab_count(&mesh) <= MAX_DABS);
        let long = [([0.0, 0.0], tip(1.0, 0.0)), ([1.0e5, 0.0], tip(1.0, 1.0e5))];
        let mesh = stamped_mesh(&long, &emit, 1.0, 1, true, InkStyle::PLAIN);
        assert_eq!(dab_count(&mesh), MAX_DABS);
    }

    #[test]
    fn jitter_is_a_function_of_seed_and_index() {
        let emit = Stamped {
            scatter: 0.5,
            rotation_jitter: 1.0,
            size_jitter: 0.5,
            opacity_jitter: 0.5,
            ..plain()
        };
        let pts = [([0.0, 0.0], tip(4.0, 0.0)), ([20.0, 3.0], tip(4.0, 20.2))];
        let a = stamped_mesh(&pts, &emit, 4.0, 7, true, InkStyle::PLAIN);
        let b = stamped_mesh(&pts, &emit, 4.0, 7, true, InkStyle::PLAIN);
        let c = stamped_mesh(&pts, &emit, 4.0, 8, true, InkStyle::PLAIN);
        assert_eq!(a, b);
        assert_ne!(a, c);
        // The prefix of a longer path lays the same dabs: live == committed.
        let longer = [pts[0], pts[1], ([40.0, 3.0], tip(4.0, 40.2))];
        let d = stamped_mesh(&longer, &emit, 4.0, 7, true, InkStyle::PLAIN);
        assert_eq!(&d.vertices[..a.vertices.len()], &a.vertices[..]);
        assert!(a.vertices.iter().any(|v| v.opacity < 1.0));
    }

    #[test]
    fn motion_tips_lie_across_the_path() {
        // Height 1 along the path, width 4 across: on a horizontal path the
        // quad spans 4 in y and 1 in x.
        let pts = [
            (
                [0.0, 0.0],
                TipState {
                    h: 1.0,
                    ..tip(4.0, 0.0)
                },
            ),
            (
                [10.0, 0.0],
                TipState {
                    h: 1.0,
                    ..tip(4.0, 10.0)
                },
            ),
        ];
        let mesh = stamped_mesh(&pts, &plain(), 4.0, 0, true, InkStyle::PLAIN);
        let q = &mesh.vertices[..4];
        let (xs, ys): (Vec<f32>, Vec<f32>) = q.iter().map(|v| (v.pos[0], v.pos[1])).unzip();
        let span = |v: &[f32]| {
            v.iter().cloned().fold(f32::MIN, f32::max) - v.iter().cloned().fold(f32::MAX, f32::min)
        };
        assert!((span(&xs) - 1.0).abs() < 1e-4, "{xs:?}");
        assert!((span(&ys) - 4.0).abs() < 1e-4, "{ys:?}");
        let _ = MaskStyle::Ribbon;
    }
}
