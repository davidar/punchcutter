//! The curve buffers on disk, so 198 ms of preprocessing is paid once per font
//! file and not once per process start.
//!
//! [`crate::bands::Builder`]'s band-count search is where nearly all of that
//! time goes (the note beside `MAX_BANDS`), and none of it depends on anything
//! but the font file: the same bytes always produce the same buffers. So the
//! buffers are **state**, not config — a derived file under
//! `$XDG_CACHE_HOME`, keyed by the font's content, which a caller may delete at
//! any time for nothing worse than one slow start.
//!
//! The format is the four GPU buffers and the CPU-side index, behind a header
//! that names the layout: [`CACHE_MAGIC`] and [`CACHE_VERSION`]. A change to
//! any `#[repr(C)]` type in [`crate::layout`] must bump the version, because
//! the words after the header are those structs' fields and nothing in the file
//! would otherwise notice. A header that does not match is a **miss**, never an
//! error — the caller rebuilds.
//!
//! Nothing here hashes the font: the *key* is the caller's business
//! (`punchcutter-fonts` uses a content hash of the font file as the file
//! name), because only the caller knows what else its cache directory holds.

use crate::bands::{Builder, CurveSet};
use crate::layout::{Band, CurveRef, Glyph, Index, Point};

/// The first eight bytes of a cache file.
pub const CACHE_MAGIC: [u8; 8] = *b"PNCHCRVS";

/// The layout version. **Bump this whenever a type in [`crate::layout`]
/// changes**, including a reserved word taking on a meaning.
pub const CACHE_VERSION: u32 = 1;

/// Words of header before the payload: the magic (2), the version, the units
/// per em, the face's glyph count, its cap height, and the four buffer
/// lengths.
const HEADER_WORDS: usize = 10;

/// Words one [`Glyph`] occupies on disk — the struct is sixteen words and the
/// three reserved ones are written as zero rather than copied.
const GLYPH_WORDS: usize = 16;

/// Words one [`Index`] entry occupies. `Index` is tuples and an array, so it is
/// written field by field rather than as memory.
const INDEX_WORDS: usize = 11;

/// Everything a cache file holds: the buffers, and the two scalars a caller
/// cannot recover from them.
#[derive(Debug)]
pub struct Cached {
    pub curves: CurveSet,
    /// `head.unitsPerEm`, as [`crate::Font::units_per_em`] reported it.
    pub units_per_em: f32,
    /// Glyphs in the face — which is *not* `curves.glyphs.len()` once other
    /// glyphs have been appended, so it is written down.
    pub glyph_count: u32,
    /// [`crate::Font::cap_height`], em units: what a caller snaps its pixel
    /// sizes by, and not recoverable from the buffers.
    pub cap_height: f32,
}

impl Cached {
    /// The file's bytes. Little-endian throughout, because the only reader is
    /// the machine that wrote it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let c = &self.curves;
        let mut words: Vec<u32> = Vec::with_capacity(
            HEADER_WORDS
                + c.points.len() * 4
                + c.bands.len() * 2
                + c.band_curves.len()
                + c.glyphs.len() * GLYPH_WORDS
                + c.index.len() * INDEX_WORDS,
        );
        for half in CACHE_MAGIC.chunks_exact(4) {
            words.push(u32::from_le_bytes([half[0], half[1], half[2], half[3]]));
        }
        words.push(CACHE_VERSION);
        words.push(self.units_per_em.to_bits());
        words.push(self.glyph_count);
        words.push(self.cap_height.to_bits());
        words.push(c.points.len() as u32);
        words.push(c.bands.len() as u32);
        words.push(c.band_curves.len() as u32);
        words.push(c.glyphs.len() as u32);

        for p in &c.points {
            words.extend([
                p.p[0].to_bits(),
                p.p[1].to_bits(),
                p.c[0].to_bits(),
                p.c[1].to_bits(),
            ]);
        }
        for b in &c.bands {
            words.extend([b.count, b.first]);
        }
        words.extend(c.band_curves.iter().map(|r| r.0));
        for g in &c.glyphs {
            words.extend([g.band_base, g.hband_count, g.vband_count, g.flags]);
            words.extend(g.band_transform.iter().map(|v| v.to_bits()));
            words.extend(g.bounds.iter().map(|v| v.to_bits()));
            words.extend([g.advance.to_bits(), 0, 0, 0]);
        }
        for i in &c.index {
            words.extend([
                i.points.0,
                i.points.1,
                i.bands.0,
                i.bands.1,
                i.band_curves.0,
                i.band_curves.1,
                i.advance.to_bits(),
            ]);
            words.extend(i.bounds.iter().map(|v| v.to_bits()));
        }

        let mut bytes = Vec::with_capacity(words.len() * 4);
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// The inverse, or `None` for anything this build does not recognise —
    /// wrong magic, wrong version, a truncated file, lengths that do not add
    /// up.
    ///
    /// Deliberately not a `Result`: every one of those cases has the same
    /// remedy, which is to build the buffers again.
    pub fn from_bytes(bytes: &[u8]) -> Option<Cached> {
        if bytes.len() % 4 != 0 {
            return None;
        }
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if words.len() < HEADER_WORDS {
            return None;
        }
        let mut magic = [0u8; 8];
        magic[..4].copy_from_slice(&words[0].to_le_bytes());
        magic[4..].copy_from_slice(&words[1].to_le_bytes());
        if magic != CACHE_MAGIC || words[2] != CACHE_VERSION {
            return None;
        }
        let units_per_em = f32::from_bits(words[3]);
        let glyph_count = words[4];
        let cap_height = f32::from_bits(words[5]);
        let n_points = words[6] as usize;
        let n_bands = words[7] as usize;
        let n_curves = words[8] as usize;
        let n_glyphs = words[9] as usize;

        let want = HEADER_WORDS
            + n_points * 4
            + n_bands * 2
            + n_curves
            + n_glyphs * (GLYPH_WORDS + INDEX_WORDS);
        if words.len() != want {
            return None;
        }

        let mut at = HEADER_WORDS;
        let mut take = |n: usize| -> &[u32] {
            let slice = &words[at..at + n];
            at += n;
            slice
        };

        let points: Vec<Point> = take(n_points * 4)
            .chunks_exact(4)
            .map(|w| Point {
                p: [f32::from_bits(w[0]), f32::from_bits(w[1])],
                c: [f32::from_bits(w[2]), f32::from_bits(w[3])],
            })
            .collect();
        let bands: Vec<Band> = take(n_bands * 2)
            .chunks_exact(2)
            .map(|w| Band {
                count: w[0],
                first: w[1],
            })
            .collect();
        let band_curves: Vec<CurveRef> = take(n_curves).iter().map(|w| CurveRef(*w)).collect();
        let glyphs: Vec<Glyph> = take(n_glyphs * GLYPH_WORDS)
            .chunks_exact(GLYPH_WORDS)
            .map(|w| Glyph {
                band_base: w[0],
                hband_count: w[1],
                vband_count: w[2],
                flags: w[3],
                band_transform: [
                    f32::from_bits(w[4]),
                    f32::from_bits(w[5]),
                    f32::from_bits(w[6]),
                    f32::from_bits(w[7]),
                ],
                bounds: [
                    f32::from_bits(w[8]),
                    f32::from_bits(w[9]),
                    f32::from_bits(w[10]),
                    f32::from_bits(w[11]),
                ],
                advance: f32::from_bits(w[12]),
                _rsvd0: 0,
                _rsvd1: 0,
                _rsvd2: 0,
            })
            .collect();
        let index: Vec<Index> = take(n_glyphs * INDEX_WORDS)
            .chunks_exact(INDEX_WORDS)
            .map(|w| Index {
                points: (w[0], w[1]),
                bands: (w[2], w[3]),
                band_curves: (w[4], w[5]),
                advance: f32::from_bits(w[6]),
                bounds: [
                    f32::from_bits(w[7]),
                    f32::from_bits(w[8]),
                    f32::from_bits(w[9]),
                    f32::from_bits(w[10]),
                ],
            })
            .collect();

        Some(Cached {
            curves: CurveSet {
                points,
                bands,
                band_curves,
                glyphs,
                index,
                rewritten: Vec::new(),
            },
            units_per_em,
            glyph_count,
            cap_height,
        })
    }

    /// A [`Builder`] that will append to these buffers — which is how a
    /// fallback face's glyphs, or a caller's own outlines, join a font that came
    /// out of a cache file rather than out of a `.ttf`.
    pub fn into_builder(self) -> Builder {
        Builder::resume(self.curves)
    }
}

#[cfg(test)]
mod tests;
