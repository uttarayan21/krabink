//! Stage 2 of the brush pipeline: input points in, tip states out. A tip
//! state is what the pen leaves at one point: width, height, rotation and
//! opacity. [`TipEvaluator`] is a pure fold over [`StrokePoint`]s (never
//! raw samples), so the live tail, a receiver of wet ink, the committed
//! stroke and a thumbnail all get the same ink from the same stored points.

use super::input::distance;
use super::spec::{Behavior, BrushSpec, Orient, Source, Target};
use crate::stroke::StrokePoint;

/// EMA factor for the speed estimate: share of each new measurement.
const SPEED_SMOOTHING: f32 = 0.5;
/// Narrowest tip that still gets a mesh, canvas units.
pub(crate) const MIN_TIP: f32 = 0.05;

/// What the tip leaves at one input point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TipState {
    /// Ink width across the stroke, canvas units.
    pub w: f32,
    /// Ink height along the stroke, canvas units.
    pub h: f32,
    /// Tip rotation, radians; meaningful for oriented tips.
    pub rot: f32,
    /// 0..=1, multiplied into the paint opacity.
    pub opacity: f32,
    /// Arc length of the input path at this point.
    pub arc: f32,
}

/// Whether a run of points is still being drawn or is the whole stroke.
/// Only a finished stroke knows where its end is, which the end taper
/// needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeEnd {
    Live,
    Complete,
}

/// Where the fold is along the stroke.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Track {
    prev: [f32; 2],
    t_ms: u32,
    /// Smoothed speed, canvas units per ms.
    speed: f32,
    arc: f32,
    w: f32,
    h: f32,
}

/// Folds input points into [`TipState`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct TipEvaluator<'a> {
    spec: &'a BrushSpec,
    base_width: f32,
    track: Option<Track>,
    /// Damped source value per behaviour, parallel to `spec.dynamics`.
    damped: Vec<Option<f32>>,
}

impl<'a> TipEvaluator<'a> {
    pub fn new(spec: &'a BrushSpec, base_width: f32) -> Self {
        Self {
            spec,
            base_width: base_width.max(0.0),
            track: None,
            damped: vec![None; spec.dynamics.len()],
        }
    }

    /// The tip at `p`, given every point pushed before it. A point that
    /// carries its own `size` keeps it: that is a stroke authored by
    /// PencilKit or an earlier build, whose widths are already final.
    pub fn push(&mut self, p: &StrokePoint) -> TipState {
        let at = [p.x, p.y];
        let (moved, dt, prev) = match self.track {
            None => (0.0, 0, None),
            Some(t) => (distance(t.prev, at), p.t_ms.saturating_sub(t.t_ms), Some(t)),
        };
        // u32 -> f32 is exact up to 2^24 ms (4.6 hours of one stroke).
        let dt_ms = dt.max(1) as f32; // ast-grep-ignore: no-as-cast
        let speed = match prev {
            None => 0.0,
            Some(t) => t.speed + (moved / dt_ms - t.speed) * SPEED_SMOOTHING,
        };
        let arc = prev.map_or(0.0, |t| t.arc + moved);
        let size = self.base_width;

        let mut w = size;
        let mut h = size * self.spec.tip.aspect;
        let mut opacity = 1.0;
        let mut rot = 0.0;
        for (i, b) in self.spec.dynamics.iter().enumerate() {
            let raw = self.source(b, p, speed, arc);
            let damped = damp(self.damped[i], raw, b.damping_ms, dt_ms);
            self.damped[i] = Some(damped);
            let out = b.output(damped);
            match b.target {
                Target::Width => w *= out,
                Target::Height => h *= out,
                Target::Size => {
                    w *= out;
                    h *= out;
                }
                Target::Opacity => opacity *= out,
                Target::Rotation => rot += out,
            }
        }
        if let Some(fixed) = p.size {
            (w, h) = (fixed.w, fixed.h);
        } else if let Some(t) = prev {
            let step = self.spec.tip.max_size_rate * moved;
            w = w.clamp(t.w - step, t.w + step);
            h = h.clamp(t.h - step, t.h + step);
        }
        let floor = (size * self.spec.tip.min_size).max(MIN_TIP);
        w = w.max(floor);
        h = h.max(floor * self.spec.tip.aspect).max(MIN_TIP);
        rot += match self.spec.tip.orient {
            Orient::Motion => 0.0,
            Orient::Nib { fallback } => p.tilt.map_or(fallback, |t| t.nib_angle()),
            Orient::Fixed(angle) => angle,
        };

        self.track = Some(Track {
            prev: at,
            t_ms: p.t_ms,
            speed,
            arc,
            w,
            h,
        });
        TipState {
            w,
            h,
            rot,
            opacity: opacity.clamp(0.0, 1.0),
            arc,
        }
    }

    /// The normalised value of a behaviour's source at `p`.
    fn source(&self, b: &Behavior, p: &StrokePoint, speed: f32, arc: f32) -> f32 {
        let size = self.base_width.max(MIN_TIP);
        match b.source {
            Source::Pressure => p.force.clamp(self.spec.input.min_force, 1.0),
            Source::Speed { max } => {
                if max > 0.0 {
                    (speed * 1000.0 / size / max).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            Source::Tilt => p.tilt.map_or(0.0, |t| {
                1.0 - (t.altitude / core::f32::consts::FRAC_PI_2).clamp(0.0, 1.0)
            }),
            Source::DistanceFromStart { over } => fraction(arc, over * size),
            // Unknown until the stroke is complete; see `evaluate`.
            Source::DistanceToEnd { .. } => 1.0,
        }
    }

    /// Every tip state of `points`, folded from the start. For a complete
    /// stroke the end taper is applied over the last `over` sizes, or
    /// half the stroke when it is shorter, so a dot or a short tick keeps
    /// its width instead of vanishing.
    pub fn evaluate(
        spec: &'a BrushSpec,
        base_width: f32,
        points: &[StrokePoint],
        end: StrokeEnd,
    ) -> Vec<TipState> {
        let mut tip = Self::new(spec, base_width);
        let mut states: Vec<TipState> = points.iter().map(|p| tip.push(p)).collect();
        if end == StrokeEnd::Complete {
            tip.taper_end(&mut states);
        }
        states
    }

    fn taper_end(&self, states: &mut [TipState]) {
        let Some(total) = states.last().map(|s| s.arc) else {
            return;
        };
        let size = self.base_width.max(MIN_TIP);
        let floor = (self.base_width * self.spec.tip.min_size).max(MIN_TIP);
        for b in &self.spec.dynamics {
            let Source::DistanceToEnd { over } = b.source else {
                continue;
            };
            let length = (over * size).min(total / 2.0);
            if length <= 0.0 {
                continue;
            }
            for s in states.iter_mut() {
                let out = b.output(fraction(total - s.arc, length));
                match b.target {
                    Target::Width => s.w = (s.w * out).max(floor),
                    Target::Height => s.h = (s.h * out).max(floor),
                    Target::Size => {
                        s.w = (s.w * out).max(floor);
                        s.h = (s.h * out).max(floor);
                    }
                    Target::Opacity => s.opacity = (s.opacity * out).clamp(0.0, 1.0),
                    Target::Rotation => s.rot += out,
                }
            }
        }
    }
}

/// 0 at the start, 1 once `distance` reaches `length`; 1 when there is no
/// length to taper over.
fn fraction(distance: f32, length: f32) -> f32 {
    if length > 0.0 {
        (distance / length).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// First-order lowpass: after `tau_ms` about two thirds of a step has
/// come through. No time constant, or no previous value, passes `raw`.
fn damp(prev: Option<f32>, raw: f32, tau_ms: f32, dt_ms: f32) -> f32 {
    match prev {
        Some(v) if tau_ms > 0.0 => v + (raw - v) * (1.0 - (-dt_ms / tau_ms).exp()),
        _ => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stroke::{PointSize, Tilt, Tool};

    fn pt(x: f32, force: f32, t_ms: u32) -> StrokePoint {
        StrokePoint {
            x,
            y: 0.0,
            force,
            t_ms,
            tilt: None,
            size: None,
        }
    }

    fn line(n: u32, step: f32, dt: u32, force: f32) -> Vec<StrokePoint> {
        (0..n).map(|i| pt(i as f32 * step, force, i * dt)).collect()
    }

    fn widths(spec: &BrushSpec, size: f32, pts: &[StrokePoint], end: StrokeEnd) -> Vec<f32> {
        TipEvaluator::evaluate(spec, size, pts, end)
            .iter()
            .map(|s| s.w)
            .collect()
    }

    #[test]
    fn pen_thins_with_light_force_and_speed() {
        let pen = BrushSpec::preset(Tool::Pen);
        let hard = widths(&pen, 4.0, &line(30, 2.0, 8, 1.0), StrokeEnd::Live);
        let soft = widths(&pen, 4.0, &line(30, 2.0, 8, 0.3), StrokeEnd::Live);
        let fast = widths(&pen, 4.0, &line(30, 6.0, 4, 1.0), StrokeEnd::Live);
        let mid = |w: &[f32]| w[w.len() / 2];
        assert!(
            mid(&soft) < mid(&hard),
            "soft {} hard {}",
            mid(&soft),
            mid(&hard)
        );
        assert!(
            mid(&fast) < mid(&hard),
            "fast {} hard {}",
            mid(&fast),
            mid(&hard)
        );
        assert!(mid(&hard) <= 4.0 + 1e-5);
        assert!(
            mid(&fast) >= 1.0 && mid(&soft) >= 1.0,
            "never below the floor"
        );
    }

    #[test]
    fn constant_tools_ignore_force_and_speed() {
        for tool in [Tool::Marker, Tool::Monoline] {
            let spec = BrushSpec::preset(tool);
            let varied: Vec<StrokePoint> = (0..30)
                .map(|i| {
                    let force = if i % 3 == 0 { 0.2 } else { 1.0 };
                    let step = if i % 2 == 0 { 1.0 } else { 9.0 };
                    pt(i as f32 * step, force, i * 8)
                })
                .collect();
            let w = widths(&spec, 6.0, &varied, StrokeEnd::Complete);
            assert!(w.iter().all(|w| (w - 6.0).abs() < 1e-5), "{tool:?}: {w:?}");
        }
    }

    #[test]
    fn end_taper_only_on_complete_strokes() {
        let pen = BrushSpec::preset(Tool::Pen);
        let pts = line(30, 2.0, 8, 0.8);
        let live = widths(&pen, 4.0, &pts, StrokeEnd::Live);
        let done = widths(&pen, 4.0, &pts, StrokeEnd::Complete);
        assert_eq!(live[..5], done[..5], "the start is untouched");
        assert!(done[29] < live[29], "the tail tapers: {done:?}");
        assert!(done[29] >= 1.0, "taper stops at the floor");
        assert!(
            (live[29] - live[15]).abs() < 0.05,
            "live tail keeps full width: {} vs {}",
            live[29],
            live[15]
        );
    }

    #[test]
    fn dot_keeps_its_width() {
        let pen = BrushSpec::preset(Tool::Pen);
        let s = TipEvaluator::evaluate(&pen, 4.0, &[pt(1.0, 1.0, 0)], StrokeEnd::Complete);
        assert_eq!((s[0].w, s[0].h, s[0].opacity), (4.0, 4.0, 1.0));
    }

    #[test]
    fn stored_size_overrides_dynamics() {
        let pen = BrushSpec::preset(Tool::Pen);
        let sized: Vec<StrokePoint> = line(3, 5.0, 8, 0.1)
            .into_iter()
            .map(|p| StrokePoint {
                size: Some(PointSize { w: 6.0, h: 6.0 }),
                ..p
            })
            .collect();
        let w = widths(&pen, 2.0, &sized, StrokeEnd::Live);
        assert!(w.iter().all(|w| *w == 6.0), "{w:?}");
    }

    #[test]
    fn pencil_widens_and_lightens_when_tilted() {
        let pencil = BrushSpec::preset(Tool::Pencil);
        let tilted = |altitude: f32| -> Vec<StrokePoint> {
            line(10, 2.0, 8, 0.6)
                .into_iter()
                .map(|p| StrokePoint {
                    tilt: Some(Tilt {
                        azimuth: 0.0,
                        altitude,
                        roll: 0.0,
                    }),
                    ..p
                })
                .collect()
        };
        let upright = TipEvaluator::evaluate(&pencil, 3.0, &tilted(1.5), StrokeEnd::Live);
        let flat = TipEvaluator::evaluate(&pencil, 3.0, &tilted(0.2), StrokeEnd::Live);
        assert!(
            flat[9].w > upright[9].w * 1.5,
            "{} vs {}",
            flat[9].w,
            upright[9].w
        );
        assert!(flat[9].opacity < upright[9].opacity);
        assert!(
            upright[9].opacity < 1.0,
            "pressure 0.6 is lighter than full"
        );
    }

    #[test]
    fn nib_rotation_follows_roll_and_falls_back() {
        let fountain = BrushSpec::preset(Tool::Fountain);
        let plain = TipEvaluator::evaluate(&fountain, 8.0, &[pt(0.0, 1.0, 0)], StrokeEnd::Live);
        assert_eq!(plain[0].rot, -core::f32::consts::FRAC_PI_4);
        let rolled = StrokePoint {
            tilt: Some(Tilt {
                azimuth: 1.0,
                altitude: 0.5,
                roll: 0.25,
            }),
            ..pt(0.0, 1.0, 0)
        };
        let s = TipEvaluator::evaluate(&fountain, 8.0, &[rolled], StrokeEnd::Live);
        assert!(
            (s[0].rot - 0.75).abs() < 1e-6,
            "roll turns against the azimuth"
        );
        assert!(
            (s[0].h - 8.0 * 0.15).abs() < 1e-6,
            "nib thickness is aspect * width"
        );
    }

    #[test]
    fn size_changes_are_rate_limited() {
        let pen = BrushSpec::preset(Tool::Pen);
        // Full pressure to the lightest touch over one 0.5-unit step.
        let pts = [pt(0.0, 1.0, 0), pt(0.5, 0.0, 8), pt(1.0, 0.0, 16)];
        let w = widths(&pen, 4.0, &pts, StrokeEnd::Live);
        assert!(w[0] - w[1] <= pen.tip.max_size_rate * 0.5 + 1e-5, "{w:?}");
        assert!(w[2] < w[1]);
    }

    #[test]
    fn damping_smooths_a_step() {
        assert_eq!(damp(None, 0.5, 40.0, 8.0), 0.5);
        assert_eq!(damp(Some(0.0), 1.0, 0.0, 8.0), 1.0);
        let d = damp(Some(0.0), 1.0, 40.0, 40.0);
        assert!((d - (1.0 - (-1.0_f32).exp())).abs() < 1e-6);
    }
}
