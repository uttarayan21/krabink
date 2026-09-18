//! Stage 1 of the brush pipeline: raw pen samples in, smoothed input
//! points out. Nothing about width or opacity lives here; the points an
//! input model emits are what a stroke stores, and every later stage
//! ([`TipEvaluator`](super::TipEvaluator), tessellation) reads only those,
//! so the live stroke, a remote receiver and the committed stroke all
//! start from the same data.
//!
//! [`EmaModel`] is the exponential-moving-average model: the smoothed
//! point trails the pen by `streamline` of the remaining distance each
//! sample, which removes hand jitter at the cost of a small lag that
//! [`EmaModel::landing`] cancels at pen-up.

use serde::{Deserialize, Serialize};

use crate::stroke::{StrokePoint, Tilt};

/// One raw input sample, before smoothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSample {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1. Pens without pressure report 1.
    pub force: f32,
    /// Milliseconds on any monotonic clock; only differences matter.
    pub t_ms: f64,
    pub tilt: Option<Tilt>,
    /// Set when the platform may revise `force` or `tilt` after the fact
    /// (Apple Pencil reports estimates first). See
    /// [`BrushModeler::update`](super::BrushModeler::update).
    pub estimate: Option<Estimate>,
}

/// A sample's estimated-property bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// The platform's update index for this sample.
    pub id: u32,
    /// Whether a revision is still expected.
    pub pending: bool,
}

/// How raw samples become input points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputParams {
    /// Streamline factor: 0 follows the pen exactly, 1 never moves. The
    /// smoothed point covers `1 - streamline` of the distance to each raw
    /// sample.
    pub streamline: f32,
    /// Smoothed samples closer than this to the previous emitted point are
    /// dropped: a slow pen at 240 Hz would otherwise emit near-duplicates.
    pub min_distance: f32,
    /// Pressure below this counts as this, so a light touch still inks.
    pub min_force: f32,
}

/// Everything the smoothing needs to continue from; small so
/// [`EmaModel::predict`] can copy it per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    /// Timestamp of the first sample; emitted `t_ms` count from here.
    origin_ms: f64,
    /// Last emitted (smoothed) position.
    prev: [f32; 2],
    last_raw: RawSample,
}

/// Exponential-moving-average input model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmaModel {
    params: InputParams,
    state: Option<State>,
}

impl EmaModel {
    pub fn new(params: InputParams) -> Self {
        Self {
            params,
            state: None,
        }
    }

    pub fn params(&self) -> &InputParams {
        &self.params
    }

    /// Feed one raw sample; the point it produced, if it moved far enough
    /// from the previous one to be worth emitting.
    pub fn push(&mut self, raw: RawSample) -> Option<StrokePoint> {
        let (state, point) = self.step(self.state, raw);
        self.state = Some(state);
        point
    }

    /// The points `raw` would produce if pushed now, without pushing them.
    /// For Apple's predicted touches: draw as a tail, discard next frame.
    pub fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint> {
        let mut state = self.state;
        raw.iter()
            .filter_map(|&sample| {
                let (next, point) = self.step(state, sample);
                state = Some(next);
                point
            })
            .collect()
    }

    /// The point that lands the pen exactly on the last raw sample, when
    /// the smoothed path stopped short of it (streamline always lags the
    /// pen). `None` when there is nothing to catch up.
    pub fn landing(&self) -> Option<StrokePoint> {
        let state = self.state?;
        let raw = state.last_raw;
        ([raw.x, raw.y] != state.prev).then(|| point(&state, raw, [raw.x, raw.y]))
    }

    /// Advance `state` by one sample. Pure: the caller decides whether to
    /// keep the new state, which is what lets `predict` share the code.
    fn step(&self, state: Option<State>, raw: RawSample) -> (State, Option<StrokePoint>) {
        let Some(prev) = state else {
            let state = State {
                origin_ms: raw.t_ms,
                prev: [raw.x, raw.y],
                last_raw: raw,
            };
            let first = point(&state, raw, [raw.x, raw.y]);
            return (state, Some(first));
        };

        let mut state = State {
            last_raw: raw,
            ..prev
        };
        let follow = 1.0 - self.params.streamline.clamp(0.0, 1.0);
        let smoothed = [
            prev.prev[0] + (raw.x - prev.prev[0]) * follow,
            prev.prev[1] + (raw.y - prev.prev[1]) * follow,
        ];
        if distance(prev.prev, smoothed) < self.params.min_distance {
            return (state, None);
        }
        state.prev = smoothed;
        let emitted = point(&state, raw, smoothed);
        (state, Some(emitted))
    }
}

fn point(state: &State, raw: RawSample, at: [f32; 2]) -> StrokePoint {
    StrokePoint {
        x: at[0],
        y: at[1],
        force: raw.force.clamp(0.0, 1.0),
        t_ms: ms_since(state.origin_ms, raw.t_ms),
        tilt: raw.tilt,
        size: None,
    }
}

pub(crate) fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

/// Whole milliseconds from `origin` to `t`, clamped into the point's
/// `t_ms`. Float-to-int has no `From`/`TryFrom`; `as` saturates, which is
/// the clamp we want for a clock that ran backwards or a stroke held for
/// 49 days.
fn ms_since(origin: f64, t: f64) -> u32 {
    (t - origin).round() as u32 // ast-grep-ignore: no-as-cast
}
