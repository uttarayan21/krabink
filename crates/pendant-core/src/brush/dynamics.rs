//! Stage 2 of the brush pipeline: input points in, tip states out. A tip
//! state is what the pen leaves at one point: its width and height along
//! the stroke. [`TipEvaluator`] is a pure fold over
//! [`StrokePoint`]s (never raw samples), so the live tail, a receiver of
//! wet ink, the committed stroke and a thumbnail all get the same widths
//! from the same stored points.

use crate::stroke::{StrokePoint, Tool};

use super::input::{InputParams, distance};

/// How a tool turns input points into ink. Constants live here so both
/// platforms agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushParams {
    /// Full ink width in canvas units.
    pub size: f32,
    /// How much force and speed narrow the ink: 0 (constant width) ..= 1.
    pub thinning: f32,
    /// Streamline factor: 0 follows the pen exactly, 1 never moves. The
    /// smoothed point covers `1 - streamline` of the distance to each raw
    /// sample.
    pub streamline: f32,
    /// Speed (canvas units per ms) at which speed thinning saturates.
    pub speed_ref: f32,
    /// Ink never gets narrower than this fraction of `size`.
    pub min_width: f32,
    /// Length of the lead-in taper in canvas units (0 = none).
    pub taper_start: f32,
    /// Length of the tail taper in canvas units (0 = none).
    pub taper_end: f32,
    /// Pressure below this counts as this, so a light touch still inks.
    pub min_force: f32,
    /// Smoothed samples closer than this to the previous emitted point are
    /// dropped: a slow pen at 240 Hz would otherwise emit near-duplicates.
    pub min_distance: f32,
}

/// Fraction of the width speed thinning can remove at full `thinning`.
const SPEED_THINNING: f32 = 0.5;
/// EMA factor for the speed estimate: share of each new measurement.
const SPEED_SMOOTHING: f32 = 0.5;

impl BrushParams {
    /// The tuned parameters for `tool` at `size` canvas units wide.
    pub fn for_tool(tool: Tool, size: f32) -> Self {
        let size = size.max(0.0);
        let base = Self {
            size,
            thinning: 0.0,
            streamline: 0.3,
            speed_ref: 1.5,
            min_width: 0.25,
            taper_start: 0.0,
            taper_end: 0.0,
            min_force: 0.15,
            min_distance: 0.25,
        };
        match tool {
            Tool::Pen => Self {
                thinning: 0.5,
                streamline: 0.5,
                taper_end: size * 1.5,
                ..base
            },
            Tool::Marker => Self {
                streamline: 0.35,
                min_distance: 0.5,
                ..base
            },
            Tool::Monoline => Self {
                streamline: 0.2,
                ..base
            },
            // The nib's width comes from its orientation, not from force.
            Tool::Brush => base,
        }
    }

    /// The stage-1 share of these parameters.
    pub fn input(&self) -> InputParams {
        InputParams {
            streamline: self.streamline,
            min_distance: self.min_distance,
        }
    }

    /// Rendered width at `force`, moving at `speed` (canvas units per ms),
    /// `arc` canvas units into the stroke.
    fn width(&self, force: f32, speed: f32, arc: f32) -> f32 {
        let force = force.clamp(self.min_force, 1.0);
        let pressure = 1.0 + (force - 1.0) * self.thinning;
        let saturation = if self.speed_ref > 0.0 {
            (speed / self.speed_ref).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let velocity = 1.0 - self.thinning * SPEED_THINNING * saturation;
        let lead_in = taper_factor(arc, self.taper_start);
        (self.size * pressure * velocity * lead_in).max(self.size * self.min_width)
    }

    fn floor(&self) -> f32 {
        self.size * self.min_width
    }
}

/// 0 at the taper's start, 1 once `distance` reaches `length`; 1 when
/// there is no taper.
fn taper_factor(distance: f32, length: f32) -> f32 {
    if length > 0.0 {
        (distance / length).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// What the tip leaves at one input point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TipState {
    /// Ink width across the stroke, canvas units.
    pub w: f32,
    /// Ink height along the stroke, canvas units.
    pub h: f32,
    /// Arc length of the input path at this point.
    pub arc: f32,
}

/// Where the fold is along the stroke; small so a predicted tail can copy
/// it per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Track {
    prev: [f32; 2],
    t_ms: u32,
    /// Smoothed speed, canvas units per ms.
    speed: f32,
    arc: f32,
}

/// Folds input points into [`TipState`]s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TipEvaluator {
    params: BrushParams,
    track: Option<Track>,
}

impl TipEvaluator {
    pub fn new(params: BrushParams) -> Self {
        Self {
            params,
            track: None,
        }
    }

    pub fn params(&self) -> &BrushParams {
        &self.params
    }

    /// The tip at `p`, given every point pushed before it. A point that
    /// carries its own `size` keeps it: that is a stroke authored by
    /// PencilKit or an earlier build, whose widths are already final.
    pub fn push(&mut self, p: &StrokePoint) -> TipState {
        let at = [p.x, p.y];
        let track = match self.track {
            None => Track {
                prev: at,
                t_ms: p.t_ms,
                speed: 0.0,
                arc: 0.0,
            },
            Some(prev) => {
                let moved = distance(prev.prev, at);
                let dt = p.t_ms.saturating_sub(prev.t_ms).max(1);
                // u32 -> f32 is lossless up to 2^24 ms, or 4.6 hours of one
                // stroke; beyond that the speed only loses precision.
                let speed = moved / dt as f32; // ast-grep-ignore: no-as-cast
                Track {
                    prev: at,
                    t_ms: p.t_ms,
                    speed: prev.speed + (speed - prev.speed) * SPEED_SMOOTHING,
                    arc: prev.arc + moved,
                }
            }
        };
        self.track = Some(track);
        let (w, h) = match p.size {
            Some(size) => (size.w, size.h),
            None => {
                let w = self.params.width(p.force, track.speed, track.arc);
                (w, w)
            }
        };
        TipState {
            w,
            h,
            arc: track.arc,
        }
    }

    /// Every tip state of `points`, folded from the start.
    pub fn evaluate(params: BrushParams, points: &[StrokePoint]) -> Vec<TipState> {
        let mut tip = Self::new(params);
        points.iter().map(|p| tip.push(p)).collect()
    }

    /// Narrow the tail over the last `taper_end` canvas units, or half the
    /// stroke when it is shorter than that, so a dot or a short tick keeps
    /// its width instead of vanishing into the floor. For a finished
    /// stroke only: the live tail is still growing.
    pub fn taper_end(params: &BrushParams, states: &mut [TipState]) {
        let Some(total) = states.last().map(|s| s.arc) else {
            return;
        };
        let length = params.taper_end.min(total / 2.0);
        if length <= 0.0 {
            return;
        }
        let floor = params.floor();
        for s in states.iter_mut() {
            let factor = taper_factor(total - s.arc, length);
            s.w = (s.w * factor).max(floor);
            s.h = (s.h * factor).max(floor);
        }
    }
}
