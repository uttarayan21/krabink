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

use crate::brush::RawSample;
use crate::stroke::{Rgba, Tilt, Tool};
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
