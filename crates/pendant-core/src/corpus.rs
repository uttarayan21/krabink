//! Recorded pen strokes, as the iPad writes them with `-recordStrokes 1`
//! and the test corpora under `tests/corpus/` keep them. One stroke per
//! file:
//!
//! ```text
//! # pendant-stroke v2
//! # expect: rect|ellipse|line|arrow|none
//! # tool: fountain size: 14.0 color: 157efbff brush: preset
//! # device: iPad ios: 26.6 force: half-average zoom: 1.00
//! # columns: x y force t_ms azimuth altitude roll est
//! 573.00 998.50 0.035 24168840.8 0.272 0.754 0.268 0
//! …
//! ```
//!
//! Version 1 files have no `# pendant-stroke` line and four columns per
//! row. Tilt columns hold `nan` for touches without a pencil. `est` is 1
//! when the row's force or tilt was still an estimate at commit. A
//! `# kind: modeled` header marks points that already went through the
//! input model (a stroke dumped at hold time). Every header the parser
//! does not know is ignored, so recorders may add more.

use crate::brush::{BrushModeler, BrushSpec, InputModelKind, RawSample, StrokeEnd, TipEvaluator};
use crate::geom::{DEFAULT_TOLERANCE, Ink};
use crate::stroke::{Rgba, StrokePoint, Tilt, Tool};
use crate::{Error, Result};

/// One recorded stroke.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// Format version; 1 when the file has no version line.
    pub version: u8,
    /// What the shape recogniser should make of it, if the file says.
    pub expect: Option<String>,
    pub tool: Tool,
    /// Base width, canvas units.
    pub size: f32,
    pub color: Option<Rgba>,
    /// Rows are input-model output, not raw samples.
    pub modeled: bool,
    pub samples: Vec<RawSample>,
    /// Parallel to `samples`: the row was still an estimate at commit.
    pub estimated: Vec<bool>,
}

impl Recording {
    /// Does every sample carry tilt?
    pub fn has_tilt(&self) -> bool {
        !self.samples.is_empty() && self.samples.iter().all(|s| s.tilt.is_some())
    }
}

/// Parse one recording. Malformed rows and unknown tools are errors;
/// unknown headers are skipped.
pub fn parse(text: &str) -> Result<Recording> {
    let mut rec = Recording {
        version: 1,
        expect: None,
        tool: Tool::Pen,
        size: 4.0,
        color: None,
        modeled: false,
        samples: Vec::new(),
        estimated: Vec::new(),
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let bad = |what: &str| Error::Schema(format!("corpus line {}: {what}", n + 1));
        if let Some(rest) = line.strip_prefix("# pendant-stroke v") {
            rec.version = rest.trim().parse().map_err(|_| bad("version"))?;
        } else if let Some(rest) = line.strip_prefix("# expect:") {
            rec.expect = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("# kind:") {
            rec.modeled = rest.trim() == "modeled";
        } else if let Some(rest) = line.strip_prefix("# tool:") {
            let mut words = rest.split_whitespace();
            rec.tool = words
                .next()
                .ok_or_else(|| bad("tool"))?
                .parse()
                .map_err(|_| bad("tool"))?;
            while let (Some(key), Some(value)) = (words.next(), words.next()) {
                match key {
                    "size:" => rec.size = value.parse().map_err(|_| bad("size"))?,
                    "color:" => {
                        let packed = u32::from_str_radix(value, 16).map_err(|_| bad("color"))?;
                        rec.color = Some(Rgba(packed.to_be_bytes()));
                    }
                    _ => {}
                }
            }
        } else if line.starts_with('#') {
            continue;
        } else {
            let f: Vec<f32> = line
                .split_whitespace()
                .map(|v| v.parse().map_err(|_| bad("number")))
                .collect::<Result<_>>()?;
            let (tilt, est) = match f.len() {
                4 => (None, false),
                7 | 8 => {
                    let tilt = (f[4].is_finite() && f[5].is_finite()).then(|| Tilt {
                        azimuth: f[4],
                        altitude: f[5],
                        roll: if f[6].is_finite() { f[6] } else { 0.0 },
                    });
                    (tilt, f.get(7).is_some_and(|e| *e != 0.0))
                }
                _ => return Err(bad("expected 4, 7 or 8 columns")),
            };
            rec.samples.push(RawSample {
                x: f[0],
                y: f[1],
                force: f[2],
                t_ms: f64::from(f[3]),
                tilt,
                estimate: None,
            });
            rec.estimated.push(est);
        }
    }
    Ok(rec)
}

/// How an input model behaved on one recording. Every number is a
/// canvas-unit or a count; bounds live with the callers (the corpus test
/// gates, the labs report).
#[derive(Debug, Clone, PartialEq)]
pub struct StrokeMetrics {
    pub samples: usize,
    pub points: usize,
    /// Mean second difference over mean step of the raw samples: the hand
    /// jitter the model starts from.
    pub raw_jitter: f32,
    /// The same over the modelled points: what is left after smoothing.
    pub jitter: f32,
    /// Mean distance between each modelled point and the raw sample
    /// nearest in time: how far the live stroke trails the pen.
    pub lag: f32,
    /// Mean distance from each modelled point to the raw polyline: how far
    /// the committed shape strays from where the pen went.
    pub deviation: f32,
    /// Distance from the finished stroke's last point to the last raw
    /// sample.
    pub overshoot: f32,
    /// Lowest and highest tip width the preset produced.
    pub width: (f32, f32),
    pub vertices: usize,
    /// Wall time of push + finish, microseconds.
    pub micros: u64,
}

impl StrokeMetrics {
    /// Model `rec` with the recording's own tool preset.
    pub fn measure(rec: &Recording, model: InputModelKind) -> Result<Self> {
        let spec = BrushSpec::preset(rec.tool);
        Self::measure_with(rec, &spec, rec.size, model).map(|(m, _)| m)
    }

    /// Model `rec` with any spec; also the points, for drawing.
    pub fn measure_with(
        rec: &Recording,
        spec: &BrushSpec,
        size: f32,
        model: InputModelKind,
    ) -> Result<(Self, Vec<StrokePoint>)> {
        let started = std::time::Instant::now();
        let points = model_points(rec, spec, size, model)?;
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let origin = rec.samples.first().map_or(0.0, |s| s.t_ms);
        let lag = mean(points.iter().map(|p| {
            let t = origin + f64::from(p.t_ms);
            rec.samples
                .iter()
                .min_by(|a, b| (a.t_ms - t).abs().total_cmp(&(b.t_ms - t).abs()))
                .map_or(0.0, |s| (p.x - s.x).hypot(p.y - s.y))
        }));
        let raw_path: Vec<[f32; 2]> = rec.samples.iter().map(|s| [s.x, s.y]).collect();
        let deviation = mean(
            points
                .iter()
                .map(|p| distance_to_polyline([p.x, p.y], &raw_path)),
        );
        let overshoot = match (points.last(), rec.samples.last()) {
            (Some(p), Some(s)) => (p.x - s.x).hypot(p.y - s.y),
            _ => 0.0,
        };
        let ink = Ink::custom(spec.clone(), rec.color.unwrap_or(Rgba::BLACK), size);
        let tips = TipEvaluator::evaluate(spec, size, &points, StrokeEnd::Complete);
        let width = tips.iter().fold((f32::INFINITY, 0.0_f32), |(lo, hi), t| {
            (lo.min(t.w), hi.max(t.w))
        });
        let mesh = ink.mesh(&points, StrokeEnd::Complete, DEFAULT_TOLERANCE);
        let metrics = Self {
            samples: rec.samples.len(),
            points: points.len(),
            raw_jitter: jitter(rec.samples.iter().map(|s| [s.x, s.y])),
            jitter: jitter(points.iter().map(|p| [p.x, p.y])),
            lag,
            deviation,
            overshoot,
            width,
            vertices: mesh.vertices.len(),
            micros,
        };
        Ok((metrics, points))
    }

    /// One JSON object, `file` and `model` included, no serializer needed.
    pub fn json(&self, file: &str, model: InputModelKind) -> String {
        format!(
            "{{\"file\":\"{file}\",\"model\":\"{}\",\"samples\":{},\"points\":{},\"raw_jitter\":{},\"jitter\":{},\"lag\":{},\"deviation\":{},\"overshoot\":{},\"width_min\":{},\"width_max\":{},\"vertices\":{},\"micros\":{}}}",
            model.name(),
            self.samples,
            self.points,
            self.raw_jitter,
            self.jitter,
            self.lag,
            self.deviation,
            self.overshoot,
            self.width.0,
            self.width.1,
            self.vertices,
            self.micros
        )
    }
}

/// The points `model` makes of `rec` under `spec`: push every sample,
/// finish.
pub fn model_points(
    rec: &Recording,
    spec: &BrushSpec,
    size: f32,
    model: InputModelKind,
) -> Result<Vec<StrokePoint>> {
    let mut modeler = BrushModeler::with_model(rec.tool, spec.clone(), size, model)?;
    for &s in &rec.samples {
        modeler.push(s);
    }
    Ok(modeler.finish())
}

fn mean(values: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = values.fold((0.0_f32, 0_usize), |(s, n), v| (s + v, n + 1));
    sum / n.max(1) as f32 // ast-grep-ignore: no-as-cast (count to float)
}

/// Mean second difference of position over the mean step: jitter that
/// does not depend on how densely the points fall.
pub fn jitter(points: impl Iterator<Item = [f32; 2]>) -> f32 {
    let pts: Vec<[f32; 2]> = points.collect();
    if pts.len() < 3 {
        return 0.0;
    }
    let second: f32 = pts
        .windows(3)
        .map(|w| {
            let ax = w[0][0] - 2.0 * w[1][0] + w[2][0];
            let ay = w[0][1] - 2.0 * w[1][1] + w[2][1];
            ax.hypot(ay)
        })
        .sum();
    let step: f32 = pts
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum();
    let steps = (pts.len() - 1) as f32; // ast-grep-ignore: no-as-cast (count to float)
    (second / (pts.len() - 2) as f32) / (step / steps).max(1e-3) // ast-grep-ignore: no-as-cast (count to float)
}

fn distance_to_polyline(p: [f32; 2], path: &[[f32; 2]]) -> f32 {
    let to_segment = |a: [f32; 2], b: [f32; 2]| {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
    };
    match path {
        [] => 0.0,
        [only] => to_segment(*only, *only),
        _ => path
            .windows(2)
            .map(|w| to_segment(w[0], w[1]))
            .fold(f32::INFINITY, f32::min),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_and_v2_rows() {
        let v1 = "# expect: rect\n# tool: pen size: 4\n1 2 0.5 1000\n3 4 0.6 1008\n";
        let r = parse(v1).unwrap();
        assert_eq!((r.version, r.tool, r.size), (1, Tool::Pen, 4.0));
        assert_eq!(r.expect.as_deref(), Some("rect"));
        assert_eq!(r.samples.len(), 2);
        assert!(!r.has_tilt());
        assert_eq!(r.estimated, vec![false, false]);

        let v2 = "# pendant-stroke v2\n# expect: none\n\
                  # tool: fountain size: 14.0 color: 157efbff brush: preset\n\
                  # device: iPad ios: 26.6 force: half-average zoom: 1.00\n\
                  # columns: x y force t_ms azimuth altitude roll est\n\
                  573.00 998.50 0.035 24168840.8 0.272 0.754 0.268 0\n\
                  574.00 998.00 0.040 24168845.0 nan nan nan 1\n";
        let r = parse(v2).unwrap();
        assert_eq!((r.version, r.tool, r.size), (2, Tool::Fountain, 14.0));
        assert_eq!(r.color, Some(Rgba([0x15, 0x7e, 0xfb, 0xff])));
        assert_eq!(r.samples[0].tilt.map(|t| t.roll), Some(0.268));
        assert_eq!(r.samples[1].tilt, None);
        assert_eq!(r.estimated, vec![false, true]);
        assert!(!r.has_tilt());
    }

    #[test]
    fn rejects_bad_rows_and_tools() {
        assert!(parse("# tool: quill size: 4\n1 2 3 4\n").is_err());
        assert!(parse("1 2 3\n").is_err());
        assert!(parse("1 2 x 4\n").is_err());
        assert!(parse("# pendant-stroke vX\n").is_err());
    }
}
