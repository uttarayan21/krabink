//! The ink-stroke-modeler input model (`ism` feature): Google's
//! spring-mass smoother as ported by `ink-stroke-modeler-rs`. A weight on
//! a spring is dragged along the raw path, so the modelled point rounds
//! corners and never overshoots, and the output is upsampled to at least
//! 180 Hz. This is the P4 trial next to [`EmaModel`](super::EmaModel):
//! same [`InputParams`] for the distance floor, tilt carried from the raw
//! samples, prediction by the EMA rule from the last modelled point.

use ink_stroke_modeler_rs::{ModelerInput, ModelerInputEventType, ModelerParams, StrokeModeler};

use super::input::{EmaModel, InputParams, RawSample, distance};
use crate::stroke::{StrokePoint, Tilt};
use crate::{Error, Result};

/// The unit the modeler's suggested speed thresholds assume is
/// centimetres; canvas units are points, and this many make a centimetre
/// on an iPad (163 ppi).
pub const UNITS_PER_CM: f32 = 64.0;

/// Tuning of the spring-mass model. The physical constants are
/// unit-free; only the wobble thresholds and the stopping distance carry
/// a length unit, scaled by `units_per_cm`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IsmParams {
    /// Canvas units per centimetre, for the wobble speed window.
    pub units_per_cm: f32,
    /// Mass over spring constant: larger follows the pen more loosely.
    pub spring_mass: f64,
    /// Velocity fraction shed per second.
    pub drag: f64,
    /// Modelled points per second at least.
    pub min_output_hz: f64,
}

impl Default for IsmParams {
    fn default() -> Self {
        let suggested = ModelerParams::suggested();
        Self {
            units_per_cm: UNITS_PER_CM,
            spring_mass: suggested.position_modeler_spring_mass_constant,
            drag: suggested.position_modeler_drag_constant,
            min_output_hz: suggested.sampling_min_output_rate,
        }
    }
}

impl IsmParams {
    fn modeler_params(&self) -> ModelerParams {
        let suggested = ModelerParams::suggested();
        let scale = f64::from(self.units_per_cm);
        ModelerParams {
            wobble_smoother_speed_floor: suggested.wobble_smoother_speed_floor * scale,
            wobble_smoother_speed_ceiling: suggested.wobble_smoother_speed_ceiling * scale,
            position_modeler_spring_mass_constant: self.spring_mass,
            position_modeler_drag_constant: self.drag,
            sampling_min_output_rate: self.min_output_hz,
            sampling_end_of_stroke_stopping_distance: suggested
                .sampling_end_of_stroke_stopping_distance
                * scale,
            ..suggested
        }
    }
}

/// Spring-mass input model. See the module docs.
pub struct IsmModel {
    params: InputParams,
    ism: IsmParams,
    engine: StrokeModeler,
    /// Longest gap one `update` accepts, seconds; longer gaps are bridged
    /// with the pen held still.
    max_gap_s: f64,
    origin_ms: f64,
    /// The last input the engine took, in engine units (seconds).
    last_input: Option<ModelerInput>,
    last_tilt: Option<Tilt>,
    last_raw: Option<RawSample>,
    /// Last emitted position, for the distance floor.
    last_emitted: Option<[f32; 2]>,
    finished: bool,
}

impl core::fmt::Debug for IsmModel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IsmModel")
            .field("params", &self.params)
            .field("ism", &self.ism)
            .field("last_input", &self.last_input)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl IsmModel {
    /// Fails only when `ism` describes a model the engine rejects
    /// (non-positive constants).
    pub fn new(params: InputParams, ism: IsmParams) -> Result<Self> {
        let modeler_params = ism.modeler_params();
        let engine = StrokeModeler::new(modeler_params).map_err(Error::Schema)?;
        let per_call = u32::try_from(modeler_params.sampling_max_outputs_per_call)
            .map_err(|_| Error::Schema("sampling_max_outputs_per_call overflow".into()))?;
        Ok(Self {
            params,
            ism,
            engine,
            max_gap_s: f64::from(per_call.saturating_sub(1))
                / modeler_params.sampling_min_output_rate,
            origin_ms: 0.0,
            last_input: None,
            last_tilt: None,
            last_raw: None,
            last_emitted: None,
            finished: false,
        })
    }

    pub fn params(&self) -> &InputParams {
        &self.params
    }

    pub fn ism_params(&self) -> &IsmParams {
        &self.ism
    }

    /// Feed one raw sample; the modelled points it produced (several when
    /// the pen was slower than the output rate, none when they all fell
    /// under the distance floor).
    pub fn push(&mut self, raw: RawSample) -> Vec<StrokePoint> {
        if self.finished {
            return Vec::new();
        }
        let Some(last) = self.last_input.clone() else {
            self.origin_ms = raw.t_ms;
            self.last_raw = Some(raw);
            let input = self.input(ModelerInputEventType::Down, raw, 0.0);
            return self.feed(input, raw.tilt, raw.tilt);
        };
        let t = ((raw.t_ms - self.origin_ms) / 1000.0).max(last.time);
        // Bridge a pause longer than the engine takes in one call with the
        // pen held where it was.
        let mut points = Vec::new();
        while t - self.last_time() > self.max_gap_s {
            let held = self.input(
                ModelerInputEventType::Move,
                self.last_raw.unwrap_or(raw),
                self.last_time() + self.max_gap_s,
            );
            points.extend(self.feed(held, self.last_tilt, self.last_tilt));
        }
        let input = self.input(ModelerInputEventType::Move, raw, t);
        self.last_raw = Some(raw);
        if input == last {
            return points;
        }
        let from = self.last_tilt;
        points.extend(self.feed(input, from, raw.tilt));
        points
    }

    /// The points `raw` would produce if pushed now, by the EMA rule from
    /// the last modelled point: the spring model cannot be run ahead
    /// without committing to it.
    pub fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint> {
        let Some(last) = self.last_raw else {
            return Vec::new();
        };
        let mut ema = EmaModel::new(self.params);
        let seed = match self.last_emitted {
            Some([x, y]) => RawSample { x, y, ..last },
            None => last,
        };
        ema.push(seed);
        ema.predict(raw)
    }

    /// Lift the pen: the spring settles onto the last raw sample and the
    /// points it passes on the way are the stroke's tail. Once.
    pub fn finish(&mut self) -> Vec<StrokePoint> {
        let (Some(last), Some(raw)) = (self.last_input.clone(), self.last_raw) else {
            return Vec::new();
        };
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let input = self.input(
            ModelerInputEventType::Up,
            raw,
            last.time + 1.0 / self.ism.min_output_hz,
        );
        let mut tail = self.feed(input, raw.tilt, raw.tilt);
        // The spring gets a bounded number of settling steps; a fast pen
        // can lift before it arrives, so land the stroke where the pen was:
        // move the last settling point there when it got close, add one
        // otherwise.
        let lifted = [raw.x, raw.y];
        let close = self
            .last_emitted
            .is_some_and(|at| distance(at, lifted) < self.params.min_distance);
        match tail.last_mut() {
            Some(p) if close => {
                p.x = raw.x;
                p.y = raw.y;
            }
            _ => tail.push(StrokePoint {
                x: raw.x,
                y: raw.y,
                force: raw.force.clamp(0.0, 1.0),
                t_ms: tail.last().map_or_else(|| ms(last.time), |p| p.t_ms),
                tilt: raw.tilt,
                size: None,
            }),
        }
        self.last_emitted = Some(lifted);
        tail
    }

    fn last_time(&self) -> f64 {
        self.last_input.as_ref().map_or(0.0, |i| i.time)
    }

    fn input(&self, event_type: ModelerInputEventType, raw: RawSample, time: f64) -> ModelerInput {
        ModelerInput {
            event_type,
            pos: (f64::from(raw.x), f64::from(raw.y)),
            time,
            pressure: f64::from(raw.force.clamp(0.0, 1.0)),
        }
    }

    /// Run the engine on `input`; tilt is carried by interpolating between
    /// the previous raw sample's and this one's over the modelled times.
    fn feed(
        &mut self,
        input: ModelerInput,
        from: Option<Tilt>,
        to: Option<Tilt>,
    ) -> Vec<StrokePoint> {
        let t0 = self.last_time();
        let t1 = input.time;
        let results = match self.engine.update(input.clone()) {
            Ok(results) => results,
            Err(err) => {
                tracing::warn!(?err, "ism update rejected a sample");
                return Vec::new();
            }
        };
        self.last_input = Some(input);
        self.last_tilt = to.or(from);
        let mut out = Vec::with_capacity(results.len());
        for r in results {
            let at = [r.pos.0 as f32, r.pos.1 as f32]; // ast-grep-ignore: no-as-cast
            if self
                .last_emitted
                .is_some_and(|prev| distance(prev, at) < self.params.min_distance)
            {
                continue;
            }
            self.last_emitted = Some(at);
            let frac = if t1 > t0 {
                ((r.time - t0) / (t1 - t0)).clamp(0.0, 1.0) as f32 // ast-grep-ignore: no-as-cast
            } else {
                1.0
            };
            out.push(StrokePoint {
                x: at[0],
                y: at[1],
                force: (r.pressure as f32).clamp(0.0, 1.0), // ast-grep-ignore: no-as-cast
                t_ms: ms(r.time),
                tilt: lerp_tilt(from, to, frac),
                size: None,
            });
        }
        out
    }
}

/// Modelled seconds since the stroke started, in the point's whole
/// milliseconds. `as` saturates, the clamp we want (see
/// `input::ms_since`).
fn ms(seconds: f64) -> u32 {
    (seconds * 1000.0).round() as u32 // ast-grep-ignore: no-as-cast
}

/// Tilt between two samples, `frac` of the way from `from` to `to`;
/// azimuth turns the short way round. A side without tilt yields the
/// other.
fn lerp_tilt(from: Option<Tilt>, to: Option<Tilt>, frac: f32) -> Option<Tilt> {
    match (from, to) {
        (Some(a), Some(b)) => {
            let tau = core::f32::consts::TAU;
            let mut d = (b.azimuth - a.azimuth) % tau;
            if d > tau / 2.0 {
                d -= tau;
            } else if d < -tau / 2.0 {
                d += tau;
            }
            Some(Tilt {
                azimuth: (a.azimuth + d * frac).rem_euclid(tau),
                altitude: a.altitude + (b.altitude - a.altitude) * frac,
                roll: a.roll + (b.roll - a.roll) * frac,
            })
        }
        (a, b) => b.or(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush::BrushSpec;
    use crate::stroke::Tool;

    fn raw(x: f32, y: f32, t_ms: f64) -> RawSample {
        RawSample {
            x,
            y,
            force: 0.5,
            t_ms,
            tilt: None,
            estimate: None,
        }
    }

    fn model() -> IsmModel {
        IsmModel::new(BrushSpec::preset(Tool::Pen).input, IsmParams::default()).expect("params")
    }

    #[test]
    fn first_sample_is_emitted_as_is() {
        let mut m = model();
        let p = m.push(raw(10.0, 20.0, 500.0));
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].x, p[0].y, p[0].t_ms), (10.0, 20.0, 0));
    }

    #[test]
    fn finish_lands_near_the_last_sample_and_only_once() {
        let mut m = model();
        for i in 0..30 {
            m.push(raw(i as f32 * 4.0, 0.0, 1000.0 + i as f64 * 8.0));
        }
        let tail = m.finish();
        let last = tail.last().expect("tail");
        assert_eq!((last.x, last.y), (116.0, 0.0));
        assert!(m.finish().is_empty());
        assert!(m.push(raw(200.0, 0.0, 2000.0)).is_empty());
    }

    #[test]
    fn a_long_pause_is_bridged_not_rejected() {
        let mut m = model();
        m.push(raw(0.0, 0.0, 0.0));
        m.push(raw(4.0, 0.0, 8.0));
        // Two seconds without an event, then the pen moves on.
        let after = m.push(raw(8.0, 0.0, 2008.0));
        assert!(!after.is_empty());
        assert!(!m.finish().is_empty());
    }

    #[test]
    fn a_backwards_clock_is_clamped() {
        let mut m = model();
        m.push(raw(0.0, 0.0, 100.0));
        m.push(raw(4.0, 0.0, 108.0));
        let p = m.push(raw(8.0, 0.0, 90.0));
        assert!(p.iter().all(|p| p.t_ms == 8));
    }

    #[test]
    fn tilt_is_carried_between_samples() {
        let mut m = model();
        let tilt = |az: f32| {
            Some(Tilt {
                azimuth: az,
                altitude: 1.0,
                roll: 0.0,
            })
        };
        m.push(RawSample {
            tilt: tilt(6.2),
            ..raw(0.0, 0.0, 0.0)
        });
        // 60 ms apart: the engine upsamples to ~11 points between.
        let pts = m.push(RawSample {
            tilt: tilt(0.2),
            ..raw(40.0, 0.0, 60.0)
        });
        assert!(pts.len() > 3);
        for p in &pts {
            let az = p.tilt.expect("tilt").azimuth;
            // The short way round from 6.2 to 0.2 passes through 0, never 3.
            assert!(!(1.0..5.5).contains(&az), "azimuth went the long way: {az}");
        }
    }

    #[test]
    fn predict_runs_ahead_from_the_last_point() {
        let mut m = model();
        for i in 0..10 {
            m.push(raw(i as f32 * 4.0, 0.0, i as f64 * 8.0));
        }
        let tail = m.predict(&[raw(60.0, 0.0, 80.0), raw(80.0, 0.0, 88.0)]);
        assert_eq!(tail.len(), 2);
        assert!(tail[0].x < tail[1].x);
    }
}
