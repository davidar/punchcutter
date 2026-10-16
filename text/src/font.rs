//! A whole font as curves: read the file, outline every glyph, hand back the
//! buffers.
//!
//! This is the part the engine leaves on the CPU: reading a file and
//! flattening what is in it is I/O, and nothing here decides anything. What it
//! deliberately does *not* do is shape: there is no `cmap` lookup, no kerning
//! and no ligature substitution in this module, because those are table walks
//! over data a buffer holds as happily as the outlines do, and they are the
//! shaper's (`shape/`). The interface between the two is a glyph id, which is what this
//! module indexes by.

use std::path::Path;

use anyhow::{Context, Result};

use crate::bands::{Builder, CurveSet};
use crate::outline::{Builder as OutlineBuilder, Contour, FontOutline};

/// One font's curves, and what it took to make them.
#[derive(Debug)]
pub struct Font {
    pub curves: CurveSet,
    /// `head.unitsPerEm`, the divisor that turned font units into em units.
    pub units_per_em: f32,
    /// Glyphs in the face, which is also `curves.glyphs.len()`.
    pub glyph_count: u32,
    /// Glyphs whose outline came back empty — spaces, and `glyf` entries with
    /// no contours. They are still glyphs, with an advance and no ink.
    pub blank_glyphs: u32,
    /// Glyphs that claim a bounding box in the font but produced no contours —
    /// an outline this crate failed to read, as distinct from a glyph that has
    /// none. Each one is a character that will draw nothing, so the count is the
    /// one worth reporting; `blank_glyphs` beside it is merely the spaces.
    pub refused_glyphs: u32,
    /// The variation axes the face declares and the default each was taken at.
    /// A variable font is read at its default instance: no `set_variation` call
    /// is made, so `gvar` deltas are not applied.
    pub axes: Vec<(String, f32)>,
    /// `OS/2.sCapHeight` in **em units**, or the height of `H` when the face
    /// declares none.
    ///
    /// The one number Slug's lack of hinting makes a caller want: the paper's
    /// remedy for unhinted text is to pick a pixel size at which the cap height
    /// lands on a whole pixel, and that cannot be done without knowing it.
    /// A consumer snaps its text sizes by it.
    pub cap_height: f32,
}

impl Font {
    /// Read a TrueType or OpenType file and build every glyph's curves.
    ///
    /// `index` selects a face in a collection; 0 for a plain `.ttf`.
    pub fn load(path: &Path, index: u32) -> Result<Font> {
        let data = std::fs::read(path)
            .with_context(|| format!("could not read the font at {}", path.display()))?;
        Font::parse(&data, index)
            .with_context(|| format!("could not build curves for {}", path.display()))
    }

    /// [`Self::load`] for bytes already in hand.
    pub fn parse(data: &[u8], index: u32) -> Result<Font> {
        let face = ttf_parser::Face::parse(data, index).context("the face would not parse")?;
        let units_per_em = f32::from(face.units_per_em());
        anyhow::ensure!(units_per_em > 0.0, "the face declares unitsPerEm 0");

        let axes: Vec<(String, f32)> = face
            .variation_axes()
            .into_iter()
            .map(|axis| (axis.tag.to_string(), axis.def_value))
            .collect();

        let glyph_count = u32::from(face.number_of_glyphs());
        let mut builder = Builder::new();
        let mut blank_glyphs = 0;
        let mut refused_glyphs = 0;

        for gid in 0..glyph_count {
            let id = ttf_parser::GlyphId(gid as u16);
            let advance = face
                .glyph_hor_advance(id)
                .map(|units| f32::from(units) / units_per_em)
                .unwrap_or(0.0);
            let contours = outline(&face, id, units_per_em);
            if contours.is_empty() {
                blank_glyphs += 1;
                // A glyph with no contours is a space — unless the font says it
                // has ink, in which case something here could not read it.
                let claims_ink = face
                    .glyph_bounding_box(id)
                    .is_some_and(|rect| rect.x_max > rect.x_min && rect.y_max > rect.y_min);
                if claims_ink {
                    refused_glyphs += 1;
                }
            }
            // Glyphs are pushed in gid order with no gaps, so a glyph index in
            // the buffers *is* a glyph id — which is the whole interface the
            // shaper needs.
            let slot = builder.push(&contours, advance, false);
            debug_assert_eq!(slot, gid);
        }

        let cap_height = face
            .capital_height()
            .map(|units| f32::from(units) / units_per_em)
            .filter(|em| *em > 0.0)
            .or_else(|| {
                let h = face.glyph_index('H')?;
                let rect = face.glyph_bounding_box(h)?;
                Some(f32::from(rect.y_max) / units_per_em)
            })
            .unwrap_or(0.7);

        Ok(Font {
            curves: builder.finish(),
            units_per_em,
            glyph_count,
            blank_glyphs,
            refused_glyphs,
            axes,
            cap_height,
        })
    }
}

/// One glyph's contours in em units, y up.
///
/// `outline_glyph`'s `None` is not an error signal: it means "no outline", which
/// is what a space is. The caller tells a space from a failure by asking the font
/// whether the glyph claims a bounding box.
fn outline(face: &ttf_parser::Face<'_>, id: ttf_parser::GlyphId, upem: f32) -> Vec<Contour> {
    let scale = 1.0 / upem;
    let mut sink = FontOutline {
        builder: OutlineBuilder::new(move |x, y| [x * scale, y * scale]),
    };
    let _ = face.outline_glyph(id, &mut sink);
    sink.builder.finish()
}

/// One glyph of *another* face, ready to be appended to a set that already
/// holds a font.
///
/// The fallback chain's unit of work. Nothing here decides anything: it is a
/// codepoint, a face's bytes, and the curves that face draws for it — in **its
/// own** em units, because `outline` divides by the face's `unitsPerEm` and
/// the merged glyph space is em-normalised throughout. The advance is the one
/// number that is not: it comes back in this face's ems and the caller rescales
/// it to whatever design units the shaper's tables are in.
#[derive(Debug)]
pub struct FaceGlyph {
    /// The glyph id **in that face**, which is what a caller keys a cache by.
    pub gid: u16,
    /// Its horizontal advance, em units of this face.
    pub advance: f32,
    /// `head.unitsPerEm` of this face.
    pub units_per_em: f32,
    /// The contours, em units, y up — empty for a face that names the
    /// codepoint but draws it with something this crate does not read (a
    /// colour table: COLRv1, CBDT, sbix).
    pub contours: Vec<Contour>,
}

impl FaceGlyph {
    /// Whether there is anything to draw. A face that claims a codepoint and
    /// has no outline for it is a colour font, and a caller walking a chain
    /// should keep walking rather than append a blank.
    pub fn has_ink(&self) -> bool {
        !self.contours.is_empty()
    }
}

/// What one face draws for one codepoint, or `None` when it has no glyph for
/// it at all.
///
/// `index` is a face index, as fontconfig reports it for a collection — the low
/// sixteen bits, because fontconfig packs a named-instance number into the high
/// ones (`%{index}` comes back as 196608 for instance 3 of face 0) and a named
/// instance is not a face. A variable font is read at its default instance
/// here, as it is in [`Font::parse`].
///
/// Parsing a face is cheap — a table directory and a few headers, not the
/// outlines — so a caller probing a chain may call this per codepoint rather
/// than hold parsed faces across a borrow.
pub fn codepoint_curves(data: &[u8], index: u32, ch: char) -> Result<Option<FaceGlyph>> {
    let face = ttf_parser::Face::parse(data, index & 0xffff).context("the face would not parse")?;
    let units_per_em = f32::from(face.units_per_em());
    anyhow::ensure!(units_per_em > 0.0, "the face declares unitsPerEm 0");
    let Some(gid) = face.glyph_index(ch) else {
        return Ok(None);
    };
    Ok(Some(face_glyph(&face, gid, units_per_em)))
}

/// What one face draws for one of **its own** glyph ids, or `None` for an id
/// past the face's `maxp.numGlyphs`.
///
/// The second half of [`codepoint_curves`], for a glyph no codepoint maps to:
/// a ligature, a contextual form, an alternate — whatever a face's `GSUB`
/// substitutes to. A cover fills those by id after it has filled the
/// codepoints, because the shaper on the GPU will reach them and a reserved
/// record nothing filled draws nothing. `index` is read as
/// [`codepoint_curves`] reads it.
///
/// A glyph with no contours is still a glyph — a space, a zero-width joiner's
/// glyph, a ligature caret — and comes back with an advance and no ink; unlike
/// a codepoint's glyph, a substitution's output with no outline is not a
/// reason to walk on.
pub fn glyph_curves(data: &[u8], index: u32, gid: u16) -> Result<Option<FaceGlyph>> {
    let face = ttf_parser::Face::parse(data, index & 0xffff).context("the face would not parse")?;
    let units_per_em = f32::from(face.units_per_em());
    anyhow::ensure!(units_per_em > 0.0, "the face declares unitsPerEm 0");
    if gid >= face.number_of_glyphs() {
        return Ok(None);
    }
    Ok(Some(face_glyph(
        &face,
        ttf_parser::GlyphId(gid),
        units_per_em,
    )))
}

/// One glyph of a parsed face as a [`FaceGlyph`]: the part the two readers
/// above share once each has a glyph id.
fn face_glyph(
    face: &ttf_parser::Face<'_>,
    gid: ttf_parser::GlyphId,
    units_per_em: f32,
) -> FaceGlyph {
    let advance = face
        .glyph_hor_advance(gid)
        .map(|units| f32::from(units) / units_per_em)
        .unwrap_or(0.0);
    FaceGlyph {
        gid: gid.0,
        advance,
        units_per_em,
        contours: outline(face, gid, units_per_em),
    }
}

/// The system's default sans-serif, through fontconfig — `fc-match
/// --format=%{file} sans-serif`, used by the harness and the tests.
///
/// I/O and a subprocess, so it belongs on this side of the rule. `None` when
/// fontconfig is not installed or says nothing useful, and the caller names a
/// path itself.
pub fn default_sans() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("fc-match")
        .args(["--format=%{file}", "sans-serif"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(path))
}

#[cfg(test)]
mod tests;
