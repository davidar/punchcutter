//! Icons the same way: an SVG's *filled paths* into the same curve and band
//! buffers a glyph lands in, so an icon is a glyph with a name.
//!
//! Finding the file (an icon theme's lookup, say) is the caller's, since
//! walking `/usr/share/icons` is I/O, and this module takes the bytes it found.
//! What comes back is an index into the same buffers the font fills, which is
//! what lets a shader draw a volume icon and a digit with one function.
//!
//! # What is ignored, and loudly
//!
//! Adwaita's symbolic icons are plain filled paths, which is exactly what the
//! coverage walk fills. Everything an SVG can *also* be is counted in
//! [`Report`] rather than silently dropped:
//!
//! - **strokes** — a stroked path needs expanding to a fill first, which this crate does not do yet. A path with a stroke and no fill contributes nothing.
//! - **gradients and patterns** — the walk returns coverage, and the colour is
//!   the caller's; a gradient fill is drawn as a flat fill. Counted.
//! - **filters, masks and clip paths** — not applied. Each counted.
//! - **raster images and text** — not outlines at all. Each counted. Text in
//!   an icon would need a text layout pass, which this loader does not have.
//!
//! # The em square
//!
//! An icon's em is the longer side of its viewBox, and its origin is the
//! bottom-left corner, so the icon occupies `[0, w/s] x [0, h/s]` with y up —
//! the same convention as a glyph sitting on a baseline. `advance` is the width,
//! which makes an icon placeable in a run of text by the same arithmetic.

use anyhow::{Context, Result};

use crate::bands::Builder;
use crate::outline::{Builder as OutlineBuilder, Contour};

/// What one SVG needed ignored, and what it actually contained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub name: String,
    /// Filled paths taken.
    pub filled_paths: u32,
    /// Paths with a stroke (whether or not they also had a fill).
    pub stroked_paths: u32,
    /// Paths with no fill at all, which therefore contributed nothing.
    pub unfilled_paths: u32,
    /// Fills that were a gradient or a pattern rather than a colour.
    pub non_solid_fills: u32,
    /// Paths whose fill rule disagreed with the icon's majority, which the one
    /// per-glyph flag cannot express.
    pub mixed_fill_rules: u32,
    pub filters: u32,
    pub masks: u32,
    pub clip_paths: u32,
    pub images: u32,
    pub text_nodes: u32,
    /// Contours, after flattening.
    pub contours: u32,
    /// Quadratics, after flattening — the cubics in the file, split.
    pub curves: u32,
}

impl Report {
    /// Whether anything at all had to be ignored; the render example prints a
    /// line for each icon that did, and a caller may report the same.
    pub fn clean(&self) -> bool {
        self.stroked_paths == 0
            && self.unfilled_paths == 0
            && self.non_solid_fills == 0
            && self.mixed_fill_rules == 0
            && self.filters == 0
            && self.masks == 0
            && self.clip_paths == 0
            && self.images == 0
            && self.text_nodes == 0
    }
}

/// A named icon's slot in the buffers, and what loading it cost.
#[derive(Clone, Debug)]
pub struct Icon {
    pub name: String,
    pub glyph: u32,
    pub report: Report,
}

/// Parse one SVG and push its filled paths into `builder` as a glyph.
///
/// `name` is the icon's name — a theme's `audio-volume-high-symbolic`, say — and
/// is carried through for the caller's own map; nothing in the buffers knows it.
pub fn load(builder: &mut Builder, name: &str, data: &[u8]) -> Result<Icon> {
    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &options)
        .with_context(|| format!("the icon {name} would not parse as SVG"))?;

    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    anyhow::ensure!(
        w > 0.0 && h > 0.0,
        "the icon {name} has a zero-sized viewBox"
    );
    let em = w.max(h);

    let mut report = Report {
        name: name.to_string(),
        ..Report::default()
    };
    // Collected per path so the fill rule can be decided by majority before the
    // contours are pushed — the flag is one bit per glyph, not one per path.
    let mut parts: Vec<(bool, Vec<Contour>)> = Vec::new();
    walk(tree.root(), em, h, &mut report, &mut parts);

    let even_odd_paths: u32 = parts.iter().filter(|(eo, _)| *eo).count() as u32;
    let even_odd = even_odd_paths * 2 > parts.len() as u32;
    report.mixed_fill_rules = if even_odd {
        parts.len() as u32 - even_odd_paths
    } else {
        even_odd_paths
    };

    let mut contours: Vec<Contour> = Vec::new();
    for (_, part) in parts {
        contours.extend(part);
    }
    report.contours = contours.len() as u32;
    report.curves = contours.iter().map(|c| c.quads.len() as u32).sum();

    let glyph = builder.push(&contours, w / em, even_odd);
    Ok(Icon {
        name: name.to_string(),
        glyph,
        report,
    })
}

/// Read one icon from a path.
pub fn load_file(builder: &mut Builder, path: &std::path::Path) -> Result<Icon> {
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let data = std::fs::read(path)
        .with_context(|| format!("could not read the icon at {}", path.display()))?;
    load(builder, &name, &data)
}

/// Depth-first over the resolved tree, taking fills and counting the rest.
fn walk(
    group: &usvg::Group,
    em: f32,
    height: f32,
    report: &mut Report,
    parts: &mut Vec<(bool, Vec<Contour>)>,
) {
    if !group.filters().is_empty() {
        report.filters += group.filters().len() as u32;
    }
    if group.mask().is_some() {
        report.masks += 1;
    }
    if group.clip_path().is_some() {
        report.clip_paths += 1;
    }

    for node in group.children() {
        match node {
            usvg::Node::Group(child) => walk(child, em, height, report, parts),
            usvg::Node::Path(path) => {
                if path.stroke().is_some() {
                    report.stroked_paths += 1;
                }
                let Some(fill) = path.fill() else {
                    report.unfilled_paths += 1;
                    continue;
                };
                if !matches!(fill.paint(), usvg::Paint::Color(_)) {
                    report.non_solid_fills += 1;
                }
                report.filled_paths += 1;
                let even_odd = fill.rule() == usvg::FillRule::EvenOdd;
                parts.push((even_odd, contours_of(path, em, height)));
            }
            usvg::Node::Image(_) => report.images += 1,
            usvg::Node::Text(_) => report.text_nodes += 1,
        }
    }
}

/// One path's contours in em units, y up.
///
/// `abs_transform` already folds in every ancestor transform and the root's
/// viewBox mapping, so the only thing left is the flip and the divide.
fn contours_of(path: &usvg::Path, em: f32, height: f32) -> Vec<Contour> {
    let transform = path.abs_transform();
    let mut builder = OutlineBuilder::new(move |x, y| {
        let (tx, ty) = apply(&transform, x, y);
        [tx / em, (height - ty) / em]
    });
    for segment in path.data().segments() {
        match segment {
            usvg::tiny_skia_path::PathSegment::MoveTo(p) => builder.move_to(p.x, p.y),
            usvg::tiny_skia_path::PathSegment::LineTo(p) => builder.line_to(p.x, p.y),
            usvg::tiny_skia_path::PathSegment::QuadTo(c, p) => builder.quad_to(c.x, c.y, p.x, p.y),
            usvg::tiny_skia_path::PathSegment::CubicTo(c1, c2, p) => {
                builder.cubic_to(c1.x, c1.y, c2.x, c2.y, p.x, p.y)
            }
            usvg::tiny_skia_path::PathSegment::Close => builder.close(),
        }
    }
    builder.finish()
}

fn apply(transform: &usvg::Transform, x: f32, y: f32) -> (f32, f32) {
    (
        transform.sx * x + transform.kx * y + transform.tx,
        transform.ky * x + transform.sy * y + transform.ty,
    )
}

#[cfg(test)]
mod tests;
