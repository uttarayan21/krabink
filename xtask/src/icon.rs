//! `gen icon ios`: the App Store icon (1024×1024, opaque RGB PNG) for the
//! iPad app, mirroring `LogoMark` in ios/Krabink/Sources/Theme.swift: the
//! indigo accent gradient with a white pencil tip. Committed at
//! ios/Krabink/Assets.xcassets/AppIcon.appiconset/AppIcon.png; only needs
//! regenerating when the mark changes. iOS masks the corners itself and App
//! Store Connect rejects icons with an alpha channel, so the gradient runs
//! edge to edge with no transparency.
//!
//! `gen icon mac`: every Mac size scaled from that PNG into
//! macos/Assets.xcassets/AppIcon.appiconset, with the asset catalog JSON.

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use crate::apple::create_dir;
use crate::error::{Error, Result, ResultExt};
use crate::repo::{IOS_APP_DIR, MACOS_APP_DIR, Repo};

const IOS_ICON: &str = "Assets.xcassets/AppIcon.appiconset/AppIcon.png";

type Rgb = [f64; 3];
type Pt = (f64, f64);

/// Theme.accent
const ACCENT: Rgb = [124.0, 140.0, 255.0];
/// Theme.bg
const BG: Rgb = [17.0, 17.0, 23.0];
const WHITE: Rgb = [255.0, 255.0, 255.0];

#[derive(Debug, Clone, clap::Args)]
pub struct AppIconArgs {
    /// Output PNG (default: the iPad asset catalog icon).
    pub out: Option<PathBuf>,
    #[arg(long, default_value_t = 1024)]
    pub size: u32,
}

pub fn app_icon(repo: &Repo, args: &AppIconArgs) -> Result<()> {
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| repo.path(IOS_APP_DIR).join(IOS_ICON));
    if let Some(parent) = out.parent() {
        create_dir(parent)?;
    }
    write_png(&out, args.size, &render(args.size))?;
    tracing::info!("wrote {} ({size}x{size})", out.display(), size = args.size);
    Ok(())
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

/// Signed distance to a convex polygon given as CCW vertices (y down).
struct ConvexSdf {
    /// (x0, y0, nx, ny): a point on each edge and its outward normal.
    edges: Vec<(f64, f64, f64, f64)>,
}

impl ConvexSdf {
    fn new(polygon: &[Pt]) -> Self {
        let n = polygon.len();
        let edges = (0..n)
            .map(|i| {
                let (x0, y0) = polygon[i];
                let (x1, y1) = polygon[(i + 1) % n];
                let (ex, ey) = (x1 - x0, y1 - y0);
                let length = ex.hypot(ey);
                // Outward normal for a CCW polygon in a y-down frame.
                (x0, y0, ey / length, -ex / length)
            })
            .collect();
        Self { edges }
    }

    fn at(&self, px: f64, py: f64) -> f64 {
        self.edges
            .iter()
            .map(|(x0, y0, nx, ny)| (px - x0) * nx + (py - y0) * ny)
            .fold(f64::NEG_INFINITY, f64::max)
    }
}

/// Silhouette and lead polygons of a pencil pointing to the bottom-left.
/// Body and tip are one convex pentagon: a union of two polygons would
/// leave a half-covered seam along their shared edge.
fn pencil(size: f64) -> (Vec<Pt>, Vec<Pt>) {
    let c = size / 2.0;
    let s = size / 1024.0;
    let (ux, uy) = (1.0 / 2f64.sqrt(), -1.0 / 2f64.sqrt()); // along the pencil, up-right
    let (vx, vy) = (1.0 / 2f64.sqrt(), 1.0 / 2f64.sqrt()); // across
    let (length, width, tip) = (560.0 * s, 150.0 * s, 170.0 * s);
    let lead = 0.42;

    let at = |a: f64, b: f64| (c + ux * a + vx * b, c + uy * a + vy * b);
    let half = width / 2.0;
    let (back, front) = (length / 2.0, -length / 2.0);
    let shoulder = front + tip;
    let point = at(front, 0.0);
    let outline = vec![
        point,
        at(shoulder, -half),
        at(back, -half),
        at(back, half),
        at(shoulder, half),
    ];
    let lead_poly = vec![
        point,
        at(front + tip * lead, -half * lead),
        at(front + tip * lead, half * lead),
    ];
    (ccw(outline), ccw(lead_poly))
}

/// Ensure CCW in the y-down frame (signed area > 0).
fn ccw(poly: Vec<Pt>) -> Vec<Pt> {
    let n = poly.len();
    let area: f64 = (0..n)
        .map(|i| {
            let (x0, y0) = poly[i];
            let (x1, y1) = poly[(i + 1) % n];
            x0 * y1 - x1 * y0
        })
        .sum();
    if area > 0.0 {
        poly
    } else {
        poly.into_iter().rev().collect()
    }
}

/// One-pixel anti-aliased edge.
fn coverage(d: f64) -> f64 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Round to the nearest channel value. A float has no `TryFrom<u8>`; the
/// clamp makes the `as` exact for every input.
fn channel(v: f64) -> u8 {
    v.round_ties_even().clamp(0.0, 255.0) as u8
}

/// RGB8 pixels, row-major.
pub fn render(size: u32) -> Vec<u8> {
    let sizef = f64::from(size);
    let (outline, lead) = pencil(sizef);
    let (outline_sdf, lead_sdf) = (ConvexSdf::new(&outline), ConvexSdf::new(&lead));
    let bound = |pick: fn(&Pt) -> f64, lo: bool| {
        let v = outline.iter().map(pick).fold(
            if lo { f64::INFINITY } else { f64::NEG_INFINITY },
            if lo { f64::min } else { f64::max },
        );
        if lo { v.floor() - 2.0 } else { v.floor() + 3.0 }
    };
    let (x_lo, x_hi) = (bound(|p| p.0, true), bound(|p| p.0, false));
    let (y_lo, y_hi) = (bound(|p| p.1, true), bound(|p| p.1, false));
    // LogoMark: accent at the top-left fading to accent at 60% opacity over
    // the dark page at the bottom-right.
    let grad_end = mix(ACCENT, BG, 0.4);
    // A subtle drop shadow under the pencil, offset like LogoMark's.
    let (shadow_dx, shadow_dy, shadow_blur) = (0.0, 10.0 * sizef / 1024.0, 22.0 * sizef / 1024.0);
    let shadow_color = mix(BG, ACCENT, 0.15);

    let mut pixels = Vec::with_capacity(usize::try_from(size).unwrap_or(0).pow(2) * 3);
    for y in 0..size {
        for x in 0..size {
            let (xf, yf) = (f64::from(x), f64::from(y));
            let t = (xf + yf) / (2.0 * (sizef - 1.0));
            let mut rgb = mix(ACCENT, grad_end, t);
            if xf >= x_lo && xf <= x_hi + shadow_blur && yf >= y_lo && yf <= y_hi + shadow_blur {
                let (px, py) = (xf + 0.5, yf + 0.5);
                let shadow_d = outline_sdf.at(px - shadow_dx, py - shadow_dy);
                let shadow = 0.35 * (1.0 - shadow_d / shadow_blur).clamp(0.0, 1.0);
                rgb = mix(rgb, shadow_color, shadow);
                rgb = mix(rgb, WHITE, coverage(outline_sdf.at(px, py)));
                rgb = mix(rgb, BG, coverage(lead_sdf.at(px, py)));
            }
            pixels.extend(rgb.map(channel));
        }
    }
    pixels
}

fn write_png(path: &Path, size: u32, rgb: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path).change_context_lazy(|| Error::Io(path.to_owned()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), size, size);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder
        .write_header()
        .change_context(Error::Icon)
        .attach_with(|| path.display().to_string())?;
    writer
        .write_image_data(rgb)
        .change_context(Error::Icon)
        .attach_with(|| path.display().to_string())?;
    writer.finish().change_context(Error::Icon)
}

/// Mac icon set from the iPad icon. The PNGs are committed under
/// macos/Assets.xcassets/AppIcon.appiconset; rerun after regenerating the
/// iPad icon.
pub fn mac_icons(repo: &Repo) -> Result<()> {
    let src = repo.path(IOS_APP_DIR).join(IOS_ICON);
    let catalog = repo.path(MACOS_APP_DIR).join("Assets.xcassets");
    let out = catalog.join("AppIcon.appiconset");
    create_dir(&out)?;

    let info = serde_json::json!({ "author": "xcode", "version": 1 });
    write_json(
        &catalog.join("Contents.json"),
        &serde_json::json!({ "info": info }),
    )?;

    let source = image::open(&src)
        .change_context(Error::Icon)
        .attach_with(|| format!("reading {}", src.display()))?;
    let mut images = Vec::new();
    for size in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let px = size * scale;
            let name = format!("icon_{size}x{size}@{scale}x.png");
            source
                .resize_exact(px, px, image::imageops::FilterType::Lanczos3)
                .save(out.join(&name))
                .change_context(Error::Icon)
                .attach_with(|| name.clone())?;
            images.push(serde_json::json!({
                "filename": name,
                "idiom": "mac",
                "scale": format!("{scale}x"),
                "size": format!("{size}x{size}"),
            }));
        }
    }
    write_json(
        &out.join("Contents.json"),
        &serde_json::json!({ "images": images, "info": info }),
    )?;
    tracing::info!("mac icon set: {}", out.display());
    Ok(())
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value).change_context(Error::Icon)?;
    text.push('\n');
    std::fs::write(path, text).change_context_lazy(|| Error::Io(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_mark() {
        let size = 64;
        let px = render(size);
        assert_eq!(px.len(), 64 * 64 * 3);
        // Top-left corner is pure accent, bottom-right the darkened end.
        assert_eq!(&px[..3], &[124, 140, 255]);
        let last = &px[px.len() - 3..];
        assert!(last[2] < 255 && last[0] < 124);
        // The pencil leaves white pixels in the middle.
        let centre = ((size / 2) * size + size / 2) as usize * 3;
        assert_eq!(&px[centre..centre + 3], &[255, 255, 255]);
    }
}
