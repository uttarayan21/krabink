//! `krabink brush-lab`: replay recorded strokes (the iPad's
//! `-recordStrokes 1` files, `crates/krabink-core/tests/corpus/brush/`)
//! through every input model and preset asked for, and write one SVG grid
//! per recording (columns: presets, rows: models, the raw pen path in grey
//! under each cell) plus `metrics.json` with the corpus numbers
//! (`krabink_core::corpus::StrokeMetrics`). Same core code as the app and
//! the iPad lab, so a tuning judged here is what ships.

use std::path::{Path, PathBuf};

use krabink_core::{
    BrushSpec, CustomBrush, Element, InputModelKind, PointKind, Rgba, Stroke, StrokeId,
    StrokePoint, Tool, corpus, elements_to_svg,
};

use crate::errors::{Error, Result, ResultExt};

pub struct LabArgs {
    pub corpus: PathBuf,
    /// Tool names, bundled brush ids, or `self` for the recording's tool.
    pub presets: Vec<String>,
    pub models: Vec<String>,
    pub out: PathBuf,
    pub svg: bool,
    pub metrics: bool,
}

/// What one column draws with.
struct Preset {
    name: String,
    /// `None` uses the recording's own tool.
    brush: Option<Brush>,
}

struct Brush {
    tool: Tool,
    spec: BrushSpec,
    custom: Option<CustomBrush>,
}

const PAD: f32 = 24.0;

pub fn run(args: LabArgs) -> Result<()> {
    let files = corpus_files(&args.corpus)?;
    let models = args
        .models
        .iter()
        .map(|name| {
            let kind = InputModelKind::parse(name)
                .ok_or(Error)
                .attach_with(|| format!("unknown input model {name:?}; ema or ism"))?;
            if !InputModelKind::available().contains(&kind) {
                return Err(Error).attach_with(|| {
                    format!("{name} is not built in; rebuild with `--features ism`")
                });
            }
            Ok(kind)
        })
        .collect::<Result<Vec<_>>>()?;
    let presets = args
        .presets
        .iter()
        .map(|name| preset(name))
        .collect::<Result<Vec<_>>>()?;
    std::fs::create_dir_all(&args.out)
        .change_context(Error)
        .attach_with(|| format!("creating {}", args.out.display()))?;

    // ast-grep-ignore: no-println (CLI output)
    println!(
        "{:<24} {:<8} {:<4} {:>6} {:>7} {:>6} {:>6} {:>6}",
        "file", "preset", "model", "points", "jitter", "lag", "dev", "µs"
    );
    let mut rows = Vec::new();
    for path in &files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("stroke")
            .to_owned();
        let text = std::fs::read_to_string(path)
            .change_context(Error)
            .attach_with(|| format!("reading {}", path.display()))?;
        let rec = corpus::parse(&text)
            .change_context(Error)
            .attach_with(|| format!("parsing {}", path.display()))?;
        let (min, max) = bounds(rec.samples.iter().map(|s| [s.x, s.y]));
        let cell = [max[0] - min[0] + 2.0 * PAD, max[1] - min[1] + 2.0 * PAD];
        let mut elements = Vec::new();
        for (row, &model) in models.iter().enumerate() {
            for (col, preset) in presets.iter().enumerate() {
                let (tool, spec, custom) = match &preset.brush {
                    Some(b) => (b.tool, b.spec.clone(), b.custom.clone()),
                    None => (rec.tool, BrushSpec::preset(rec.tool), None),
                };
                let (metrics, points) =
                    corpus::StrokeMetrics::measure_with(&rec, &spec, rec.size, model)
                        .change_context(Error)
                        .attach_with(|| format!("modelling {stem} with {}", model.name()))?;
                // ast-grep-ignore: no-println (CLI output)
                println!(
                    "{stem:<24} {:<8} {:<4} {:>6} {:>7.3} {:>6.2} {:>6.2} {:>6}",
                    preset.name,
                    model.name(),
                    metrics.points,
                    metrics.jitter,
                    metrics.lag,
                    metrics.deviation,
                    metrics.micros
                );
                rows.push(metrics.json(&format!("{stem}:{}", preset.name), model));
                if !args.svg {
                    continue;
                }
                let shift = [
                    grid_index(col) * cell[0] + PAD - min[0],
                    grid_index(row) * cell[1] + PAD - min[1],
                ];
                elements.push(raw_path(&rec, shift, elements.len()));
                elements.push(Element::Stroke(Stroke {
                    id: lab_id(elements.len()),
                    tool,
                    brush: custom,
                    color: rec.color.unwrap_or(Rgba::BLACK),
                    base_width: rec.size,
                    kind: PointKind::PolylineSample,
                    points: shifted(&points, shift),
                    created_ms: 0,
                }));
            }
        }
        if args.svg {
            let out = args.out.join(format!("{stem}.svg"));
            std::fs::write(&out, elements_to_svg(&elements))
                .change_context(Error)
                .attach_with(|| format!("writing {}", out.display()))?;
        }
    }
    if args.metrics {
        let out = args.out.join("metrics.json");
        std::fs::write(&out, format!("[\n{}\n]\n", rows.join(",\n")))
            .change_context(Error)
            .attach_with(|| format!("writing {}", out.display()))?;
        // ast-grep-ignore: no-println (CLI output)
        println!("wrote {}", out.display());
    }
    Ok(())
}

/// `corpus` itself when it is a file, else every `*.txt` in it, sorted.
fn corpus_files(corpus: &Path) -> Result<Vec<PathBuf>> {
    if corpus.is_file() {
        return Ok(vec![corpus.to_path_buf()]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus)
        .change_context(Error)
        .attach_with(|| format!("listing {}", corpus.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(Error).attach_with(|| format!("no recordings in {}", corpus.display()));
    }
    Ok(files)
}

fn preset(name: &str) -> Result<Preset> {
    let brush = match name {
        "self" => None,
        "pen" => Some(tool_brush(Tool::Pen)),
        "pencil" => Some(tool_brush(Tool::Pencil)),
        "marker" => Some(tool_brush(Tool::Marker)),
        "monoline" => Some(tool_brush(Tool::Monoline)),
        "fountain" => Some(tool_brush(Tool::Fountain)),
        id => {
            let builtin = BrushSpec::builtins()
                .into_iter()
                .find(|b| b.id.to_string() == id)
                .ok_or(Error)
                .attach_with(|| {
                    format!("unknown preset {id:?}: a tool name, `self` or a bundled brush id")
                })?;
            Some(Brush {
                tool: Tool::Pencil,
                spec: builtin.spec.clone(),
                custom: Some(CustomBrush {
                    id: builtin.id,
                    spec: builtin.spec,
                }),
            })
        }
    };
    Ok(Preset {
        name: name.to_owned(),
        brush,
    })
}

fn tool_brush(tool: Tool) -> Brush {
    Brush {
        tool,
        spec: BrushSpec::preset(tool),
        custom: None,
    }
}

/// The raw samples as a hairline monoline stroke, grey, under the cell.
fn raw_path(rec: &corpus::Recording, shift: [f32; 2], n: usize) -> Element {
    let origin = rec.samples.first().map_or(0.0, |s| s.t_ms);
    let points = rec
        .samples
        .iter()
        .map(|s| StrokePoint {
            x: s.x + shift[0],
            y: s.y + shift[1],
            force: 1.0,
            t_ms: u32::try_from(((s.t_ms - origin).max(0.0)).round() as u64) // ast-grep-ignore: no-as-cast
                .unwrap_or(u32::MAX),
            tilt: None,
            size: None,
        })
        .collect();
    Element::Stroke(Stroke {
        id: lab_id(n),
        tool: Tool::Monoline,
        brush: None,
        color: Rgba([160, 160, 160, 255]),
        base_width: 0.6,
        kind: PointKind::PolylineSample,
        points,
        created_ms: 0,
    })
}

/// A grid row or column number as a float; the grid never has 65k cells.
fn grid_index(n: usize) -> f32 {
    f32::from(u16::try_from(n).unwrap_or(u16::MAX))
}

fn shifted(points: &[StrokePoint], shift: [f32; 2]) -> Vec<StrokePoint> {
    points
        .iter()
        .map(|p| StrokePoint {
            x: p.x + shift[0],
            y: p.y + shift[1],
            ..*p
        })
        .collect()
}

fn bounds(points: impl Iterator<Item = [f32; 2]>) -> ([f32; 2], [f32; 2]) {
    points.fold(
        ([f32::MAX, f32::MAX], [f32::MIN, f32::MIN]),
        |(lo, hi), p| {
            (
                [lo[0].min(p[0]), lo[1].min(p[1])],
                [hi[0].max(p[0]), hi[1].max(p[1])],
            )
        },
    )
}

/// Element ids for a document nobody stores: fresh ULIDs.
fn lab_id(_n: usize) -> StrokeId {
    StrokeId::new()
}
