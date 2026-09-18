//! Brush model: turns raw pen samples into the points a stroke stores, and
//! stored points into ink. One [`BrushModeler`] lives per stroke in
//! progress.
//!
//! Three stages. [`input`] smooths raw samples into input points
//! ([`EmaModel`]); those are what a stroke stores and what the wet-ink
//! wire carries. [`dynamics`] folds input points into tip states, the
//! width, height, rotation and opacity at each point from force, tilt,
//! speed and the tapers ([`TipEvaluator`]), driven by a [`BrushSpec`].
//! Geometry ([`crate::stroke_mesh`]) turns tip states into triangles.
//! Stages two and three read only stored points, so the live stroke, a
//! receiver of wet ink and the committed stroke all draw the same ink.
//!
//! ```
//! use pendant_core::{BrushModeler, RawSample, Tool};
//!
//! let mut modeler = BrushModeler::new(Tool::Pen, 4.0);
//! for i in 0..10 {
//!     let raw = RawSample { x: i as f32 * 3.0, y: 0.0, force: 0.6, t_ms: i as f64 * 8.0, tilt: None, estimate: None };
//!     modeler.push(raw);
//! }
//! let points = modeler.finish();
//! assert!(points.iter().all(|p| p.size.is_none()), "widths are evaluated at render time");
//! ```

mod dynamics;
mod input;
mod spec;

pub(crate) use dynamics::MIN_TIP;
pub use dynamics::{StrokeEnd, TipEvaluator, TipState};
pub(crate) use input::distance;
pub use input::{EmaModel, Estimate, InputParams, RawSample};
pub use spec::{
    Behavior, Blend, BrushId, BrushSpec, Curve, CustomBrush, Grain, GrainMapping, GrainSource,
    Orient, Overlap, Paint, Source, Target, Tip,
};

use crate::stroke::{StrokePoint, Tilt, Tool};

/// Turns one stroke's raw samples into stored [`StrokePoint`]s. Owns the
/// points emitted so far; renderers draw [`points`](Self::points) plus a
/// [`predict`](Self::predict) tail every frame and commit
/// [`finish`](Self::finish) at pen-up.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushModeler {
    tool: Tool,
    spec: BrushSpec,
    base_width: f32,
    input: EmaModel,
    points: Vec<StrokePoint>,
    /// `(estimate id, index into points, revision still pending)` for
    /// emitted samples the platform may revise.
    estimates: Vec<(u32, usize, bool)>,
}

impl BrushModeler {
    /// A modeler for `tool`'s preset at `size` canvas units wide.
    pub fn new(tool: Tool, size: f32) -> Self {
        Self::for_brush(tool, BrushSpec::preset(tool), size)
    }

    /// A modeler for any spec; `tool` is what the stroke records.
    pub fn for_brush(tool: Tool, spec: BrushSpec, size: f32) -> Self {
        Self {
            tool,
            input: EmaModel::new(spec.input),
            spec,
            base_width: size.max(0.0),
            points: Vec::new(),
            estimates: Vec::new(),
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn spec(&self) -> &BrushSpec {
        &self.spec
    }

    pub fn base_width(&self) -> f32 {
        self.base_width
    }

    /// Points emitted so far.
    pub fn points(&self) -> &[StrokePoint] {
        &self.points
    }

    /// Feed one raw sample; the point it produced, if it moved far enough
    /// from the previous one to be worth emitting.
    pub fn push(&mut self, raw: RawSample) -> Option<StrokePoint> {
        let point = self.input.push(raw)?;
        if let Some(e) = raw.estimate {
            self.estimates.push((e.id, self.points.len(), e.pending));
        }
        self.points.push(point);
        Some(point)
    }

    /// The points `raw` would produce if pushed now, without pushing them.
    /// For Apple's predicted touches: draw as a tail, discard next frame.
    pub fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint> {
        self.input.predict(raw)
    }

    /// Revise the force and tilt of the point that the sample with
    /// estimate `id` produced. `false` when no emitted point came from that
    /// sample (it was dropped as a near-duplicate, or never pushed).
    pub fn update(&mut self, id: u32, force: Option<f32>, tilt: Option<Tilt>) -> bool {
        let Some(entry) = self.estimates.iter_mut().find(|(e, _, _)| *e == id) else {
            return false;
        };
        entry.2 = false;
        let point = &mut self.points[entry.1];
        if let Some(force) = force {
            point.force = force.clamp(0.0, 1.0);
        }
        if let Some(tilt) = tilt {
            point.tilt = Some(tilt);
        }
        true
    }

    /// Estimate ids of emitted points whose revision has not arrived.
    pub fn pending_estimates(&self) -> Vec<u32> {
        self.estimates
            .iter()
            .filter(|(_, _, pending)| *pending)
            .map(|(id, _, _)| *id)
            .collect()
    }

    /// The finished stroke: every emitted point plus the last raw sample
    /// landed exactly (streamline always lags the pen).
    pub fn finish(&self) -> Vec<StrokePoint> {
        let mut points = self.points.clone();
        points.extend(self.input.landing());
        points
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn raw(x: f32, y: f32, force: f32, t_ms: f64) -> RawSample {
        RawSample {
            x,
            y,
            force,
            t_ms,
            tilt: None,
            estimate: None,
        }
    }

    /// A straight line of `n` samples `step` apart in space and `dt` ms in time.
    fn line(n: usize, step: f32, dt: f64, force: f32) -> Vec<RawSample> {
        (0..n)
            .map(|i| raw(i as f32 * step, 0.0, force, 1000.0 + i as f64 * dt))
            .collect()
    }

    fn run(tool: Tool, size: f32, samples: &[RawSample]) -> Vec<StrokePoint> {
        let mut m = BrushModeler::new(tool, size);
        for &s in samples {
            m.push(s);
        }
        m.finish()
    }

    fn mean_second_difference(points: &[StrokePoint]) -> f32 {
        let ys: Vec<f32> = points.iter().map(|p| p.y).collect();
        let n = ys.len().saturating_sub(2);
        if n == 0 {
            return 0.0;
        }
        ys.windows(3)
            .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs())
            .sum::<f32>()
            / n as f32
    }

    #[test]
    fn deterministic() {
        let samples = line(20, 2.0, 8.0, 0.6);
        assert_eq!(run(Tool::Pen, 4.0, &samples), run(Tool::Pen, 4.0, &samples));
    }

    #[test]
    fn streamline_reduces_jitter() {
        // A horizontal line with alternating ±1 vertical noise.
        let noisy: Vec<RawSample> = (0..40)
            .map(|i| {
                let jitter = if i % 2 == 0 { 1.0 } else { -1.0 };
                raw(i as f32 * 2.0, jitter, 0.5, 1000.0 + i as f64 * 8.0)
            })
            .collect();
        let raw_points: Vec<StrokePoint> = noisy
            .iter()
            .map(|r| StrokePoint {
                x: r.x,
                y: r.y,
                force: r.force,
                t_ms: 0,
                tilt: None,
                size: None,
            })
            .collect();
        let smoothed = run(Tool::Pen, 4.0, &noisy);
        assert!(smoothed.len() > 10, "smoothing dropped too many points");
        assert!(
            mean_second_difference(&smoothed) < mean_second_difference(&raw_points) / 2.0,
            "streamline should at least halve the jitter"
        );
    }

    #[test]
    fn predict_never_mutates() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for s in line(10, 2.0, 8.0, 0.7) {
            m.push(s);
        }
        let before = m.clone();
        let tail: Vec<RawSample> = (10..14)
            .map(|i| raw(i as f32 * 2.0, 1.0, 0.7, 1000.0 + i as f64 * 8.0))
            .collect();
        let predicted = m.predict(&tail);
        assert_eq!(m, before);
        assert!(!predicted.is_empty());

        // And prediction is exactly what pushing would have produced.
        let pushed: Vec<StrokePoint> = tail.iter().filter_map(|&s| m.push(s)).collect();
        assert_eq!(predicted, pushed);
    }

    #[test]
    fn finish_lands_on_last_raw_sample() {
        let samples = line(30, 2.0, 8.0, 0.8);
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for &s in &samples {
            m.push(s);
        }
        let last_raw = samples[samples.len() - 1];
        let live = m.points().to_vec();
        let done = m.finish();

        // Streamline lags: the live tail is short of the pen.
        assert!(live[live.len() - 1].x < last_raw.x);
        assert_eq!(&done[..live.len()], &live[..]);
        let end = done[done.len() - 1];
        assert_eq!((end.x, end.y), (last_raw.x, last_raw.y));
        assert_eq!(end.t_ms, 29 * 8);
        assert!(done.iter().all(|p| p.size.is_none()));
    }

    #[test]
    fn estimates_revise_force_and_tilt_in_place() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        let estimated = |i: u32, pending: bool| RawSample {
            estimate: Some(Estimate { id: i, pending }),
            ..raw(i as f32 * 5.0, 0.0, 0.2, f64::from(i) * 8.0)
        };
        m.push(estimated(0, true));
        m.push(estimated(1, false));
        // Dropped as a near-duplicate (the smoothed point sits at 2.5, this
        // moves it 0.05): its estimate never maps to a point, so nothing
        // waits for it.
        m.push(RawSample {
            estimate: Some(Estimate {
                id: 2,
                pending: true,
            }),
            ..raw(2.6, 0.0, 0.2, 20.0)
        });
        assert_eq!(m.pending_estimates(), vec![0]);

        let tilt = Tilt {
            azimuth: 1.0,
            altitude: 0.4,
            roll: 0.0,
        };
        assert!(m.update(0, Some(0.9), Some(tilt)));
        assert_eq!(m.points()[0].force, 0.9);
        assert_eq!(m.points()[0].tilt, Some(tilt));
        assert!(!m.update(2, Some(1.0), None));
        assert!(!m.update(7, None, None));
        assert!(m.pending_estimates().is_empty());
        assert_eq!(m.points()[1].force, 0.2, "other points untouched");
    }

    #[test]
    fn near_duplicates_are_dropped() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        assert!(m.push(raw(0.0, 0.0, 0.5, 0.0)).is_some());
        // 0.1 unit raw step, halved by streamline: below min_distance.
        assert!(m.push(raw(0.1, 0.0, 0.5, 4.0)).is_none());
        assert!(m.push(raw(5.0, 0.0, 0.5, 8.0)).is_some());
        assert_eq!(m.points().len(), 2);
    }

    #[test]
    fn timestamps_count_from_first_sample() {
        let points = run(Tool::Monoline, 2.0, &line(4, 3.0, 8.5, 1.0));
        let t: Vec<u32> = points.iter().map(|p| p.t_ms).collect();
        assert_eq!(t[0], 0);
        assert!(t.windows(2).all(|w| w[0] <= w[1]), "{t:?}");
        assert_eq!(t[t.len() - 1], 26);
    }

    #[test]
    fn tilt_passes_through() {
        let tilt = Tilt {
            azimuth: 1.0,
            altitude: 0.5,
            roll: 0.2,
        };
        let mut m = BrushModeler::new(Tool::Fountain, 8.0);
        m.push(RawSample {
            tilt: Some(tilt),
            ..raw(0.0, 0.0, 1.0, 0.0)
        });
        assert_eq!(m.points()[0].tilt, Some(tilt));
    }

    fn arb_sample() -> impl Strategy<Value = RawSample> {
        (-1e4_f32..1e4, -1e4_f32..1e4, -0.5_f32..1.5, 0.0_f64..1e6)
            .prop_map(|(x, y, force, t_ms)| raw(x, y, force, t_ms))
    }

    proptest! {
        #[test]
        fn output_is_bounded_and_finite(
            tool in prop_oneof![Just(Tool::Pen), Just(Tool::Pencil), Just(Tool::Marker), Just(Tool::Monoline), Just(Tool::Fountain)],
            size in 0.5_f32..40.0,
            samples in proptest::collection::vec(arb_sample(), 0..64),
        ) {
            let mut m = BrushModeler::new(tool, size);
            for &s in &samples {
                m.push(s);
            }
            let done = m.finish();
            prop_assert_eq!(done.is_empty(), samples.is_empty());
            for p in &done {
                prop_assert!(p.x.is_finite() && p.y.is_finite());
                prop_assert!((0.0..=1.0).contains(&p.force));
            }
            if let (Some(last), Some(raw)) = (done.last(), samples.last()) {
                prop_assert_eq!((last.x, last.y), (raw.x, raw.y));
            }
            let states = TipEvaluator::evaluate(m.spec(), size, &done, StrokeEnd::Complete);
            let floor = (size * m.spec().tip.min_size).max(0.05);
            for s in &states {
                prop_assert!(s.w.is_finite() && s.h.is_finite() && s.rot.is_finite());
                prop_assert!(s.w >= floor - 1e-4, "width {} under the floor {floor}", s.w);
                prop_assert!((0.0..=1.0).contains(&s.opacity));
            }
        }
    }
}
