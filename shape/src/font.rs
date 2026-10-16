//! The table slicer: the CPU's whole half of the job, and it is a directory.
//!
//! Five tables come out of a font file by name — `cmap`, `hmtx`, `GDEF`,
//! `GSUB`, `GPOS` — and go into the arena **verbatim**, behind a record saying
//! where each one landed. Four scalars come with them: the face's `unitsPerEm`,
//! its glyph count, its `numberOfHMetrics`, and the merged glyph id its own
//! id 0 sits at. Nothing here reads a lookup, a coverage table or a subtable
//! format; `slang/shape.slang` walks OpenType's own bytes where they lie.
//!
//! Nothing here prepares a table, because any such step would be a second
//! understanding of a lookup type that the shader already has to have. A
//! subtable format the shader will not walk is a flag the *shader* raises
//! (`SHAPE_RUN_UNSUPPORTED`).
//!
//! # Why this is still I/O
//!
//! Locating a table in an sfnt is reading the file's own directory: a tag, an
//! offset and a length, sixteen bytes a table, in the header every font begins
//! with. The engine leaves the CPU I/O, and this is I/O with nothing added.
//!
//! # A variable font is read at its default instance
//!
//! No axis is pinned, so `hmtx` is the default instance's and `HVAR` is not
//! consulted — which is what a shaper does for the default coordinates too.
//! Variable-font instances are not supported yet.

use std::path::Path;

use ttf_parser::{RawFace, Tag};

use crate::abi::{
    MAX_OT_FACES, OT_FACE_ALL_FILLED, OT_TABLES_START, OtFace, OtHeader, SHAPE_ABI_VERSION,
    ShapeFont,
};

/// The five tables, in the order [`OtFace`] names them.
const TABLES: [&[u8; 4]; 5] = [b"cmap", b"hmtx", b"GDEF", b"GSUB", b"GPOS"];

/// Why a font could not be turned into an arena.
#[derive(Debug)]
pub enum FontError {
    /// The file could not be read.
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// ttf-parser refused the sfnt header.
    Parse(ttf_parser::FaceParsingError),
    /// A face with no glyphs, a `unitsPerEm` of zero, or no `head`/`maxp`:
    /// nothing to shape with.
    Unusable(&'static str),
    /// The directory is full ([`MAX_OT_FACES`]).
    Full,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontError::Io { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            FontError::Parse(err) => write!(f, "the font could not be parsed: {err}"),
            FontError::Unusable(why) => write!(f, "the font is unusable for shaping: {why}"),
            FontError::Full => write!(f, "the arena already holds {MAX_OT_FACES} faces"),
        }
    }
}

impl std::error::Error for FontError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FontError::Io { source, .. } => Some(source),
            FontError::Parse(err) => Some(err),
            _ => None,
        }
    }
}

/// One face's directory entry, as this side remembers it. The arena holds the
/// same numbers as an [`OtFace`]; this is what a caller reads them back from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FaceInfo {
    pub glyph_base: u32,
    pub glyph_count: u32,
    pub upem: u32,
    pub num_h_metrics: u32,
    /// `(offset, length)` for each of `TABLES`, in that order.
    pub tables: [(u32, u32); 5],
    /// Where the fill bitmap starts, or 0 when [`Self::all_filled`].
    pub filled_off: u32,
    pub all_filled: bool,
}

impl FaceInfo {
    /// Bytes this face contributed: its five tables and its fill bitmap.
    pub fn bytes(&self) -> u32 {
        self.tables.iter().map(|(_, len)| pad4(*len)).sum::<u32>() + self.bitmap_bytes()
    }

    /// The fill bitmap's length in bytes, zero when the face needs none.
    pub fn bitmap_bytes(&self) -> u32 {
        if self.all_filled {
            0
        } else {
            self.glyph_count.div_ceil(32) * 4
        }
    }

    /// `cmap hmtx GDEF GSUB GPOS` with a size each, for a log line.
    pub fn table_sizes(&self) -> String {
        TABLES
            .iter()
            .zip(self.tables.iter())
            .filter(|(_, (_, len))| *len > 0)
            .map(|(tag, (_, len))| {
                format!(
                    "{} {} KiB",
                    std::str::from_utf8(*tag).unwrap_or("????"),
                    len.div_ceil(1024)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The arena: one word buffer, a fixed-capacity directory at the front, the
/// faces' table bytes behind it.
///
/// Append-only. A face opened later puts its tables at the end and rewrites the
/// front block, so what a GPU copy needs is two writes — the front block and
/// the tail, plus any fill bitmap that changed — and never the whole arena
/// again.
#[derive(Clone, Debug)]
pub struct Arena {
    bytes: Vec<u8>,
    faces: Vec<FaceInfo>,
    /// The merged glyph watermark: the next `glyph_base` a face would take, and
    /// what [`ShapeFont::glyph_count`] follows.
    watermark: u32,
    font: ShapeFont,
}

impl Default for Arena {
    fn default() -> Arena {
        Arena::new()
    }
}

impl Arena {
    /// An arena with the front block written and no faces.
    pub fn new() -> Arena {
        let mut arena = Arena {
            bytes: vec![0u8; OT_TABLES_START as usize],
            faces: Vec::new(),
            watermark: 0,
            font: ShapeFont::default(),
        };
        arena.write_front();
        arena
    }

    /// Read a font file and make an arena of its first face.
    pub fn load(path: impl AsRef<Path>) -> Result<Arena, FontError> {
        let path = path.as_ref();
        let data = std::fs::read(path).map_err(|source| FontError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Arena::of(&data, 0)
    }

    /// An arena of one face already in memory — the primary, with every glyph
    /// counted as filled.
    pub fn of(data: &[u8], index: u32) -> Result<Arena, FontError> {
        let mut arena = Arena::new();
        arena.add_face(data, index, true)?;
        Ok(arena)
    }

    /// Slice one face's five tables into the arena and give it the next range
    /// of the merged glyph space.
    ///
    /// `all_filled` is true for a face whose curves are all in the curve
    /// buffers already (the primary); false for one opened by a cover, whose
    /// glyphs are filled a few at a time and whose bitmap says which.
    ///
    /// Returns the face's index in the directory.
    pub fn add_face(
        &mut self,
        data: &[u8],
        index: u32,
        all_filled: bool,
    ) -> Result<usize, FontError> {
        if self.faces.len() >= MAX_OT_FACES as usize {
            return Err(FontError::Full);
        }
        let raw = RawFace::parse(data, index).map_err(FontError::Parse)?;

        let head = raw
            .table(Tag::from_bytes(b"head"))
            .ok_or(FontError::Unusable("no head table"))?;
        let upem = u32::from(be16(head, 18).ok_or(FontError::Unusable("head is too short"))?);
        if upem == 0 {
            return Err(FontError::Unusable("unitsPerEm is zero"));
        }
        let maxp = raw
            .table(Tag::from_bytes(b"maxp"))
            .ok_or(FontError::Unusable("no maxp table"))?;
        let glyph_count = u32::from(be16(maxp, 4).ok_or(FontError::Unusable("maxp is too short"))?);
        if glyph_count == 0 {
            return Err(FontError::Unusable("the face has no glyphs"));
        }
        let hhea = raw.table(Tag::from_bytes(b"hhea"));
        let num_h_metrics = hhea
            .and_then(|table| be16(table, 34))
            .map(u32::from)
            .unwrap_or(0);

        let mut info = FaceInfo {
            glyph_base: self.watermark,
            glyph_count,
            upem,
            num_h_metrics,
            all_filled,
            ..FaceInfo::default()
        };
        for (slot, tag) in TABLES.iter().enumerate() {
            let Some(table) = raw.table(Tag::from_bytes(tag)) else {
                continue;
            };
            if table.is_empty() {
                continue;
            }
            info.tables[slot] = (self.append(table), table.len() as u32);
        }
        if !all_filled {
            let words = glyph_count.div_ceil(32) as usize;
            info.filled_off = self.append(&vec![0u8; words * 4]);
        }

        // The face's own numbers into the ShapeFont when it is the primary; the
        // merged glyph count follows the watermark whatever face moved it.
        if self.faces.is_empty() {
            self.font.upem = upem;
            let (ascender, descender) = hhea
                .map(|table| {
                    (
                        be16(table, 4).map(i32::from).unwrap_or(0),
                        be16(table, 6)
                            .map(|value| i32::from(value as i16))
                            .unwrap_or(0),
                    )
                })
                .unwrap_or((0, 0));
            self.font.ascender = ascender;
            self.font.descender = descender;
        }
        self.watermark += glyph_count;
        self.faces.push(info);
        self.write_front();
        Ok(self.faces.len() - 1)
    }

    /// Reserve merged glyph ids that belong to no face — a caller's own
    /// outlines, which have no codepoint and no tables and sit between the
    /// primary's range and the next fallback face's.
    ///
    /// Called when a caller appends such glyphs to the curve buffers, so that
    /// the next face opened takes a base past them.
    pub fn reserve_ids(&mut self, count: u32) -> u32 {
        let base = self.watermark;
        self.watermark += count;
        self.set_glyph_count(self.watermark);
        base
    }

    /// The merged glyph watermark: one past the highest id any face or
    /// appended glyph has been given.
    pub fn watermark(&self) -> u32 {
        self.watermark
    }

    /// Say that a face's glyph now has its curves. Idempotent; a face that is
    /// [`FaceInfo::all_filled`] needs none of this.
    pub fn mark_filled(&mut self, face: usize, local_gid: u32) {
        let Some(info) = self.faces.get(face).copied() else {
            return;
        };
        if info.all_filled || local_gid >= info.glyph_count {
            return;
        }
        let at = info.filled_off as usize + (local_gid / 32) as usize * 4;
        let Some(slot) = self.bytes.get_mut(at..at + 4) else {
            return;
        };
        let mut word = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
        word |= 1 << (local_gid % 32);
        slot.copy_from_slice(&word.to_le_bytes());
    }

    /// Whether a face's glyph has been filled.
    pub fn is_filled(&self, face: usize, local_gid: u32) -> bool {
        let Some(info) = self.faces.get(face) else {
            return false;
        };
        if info.all_filled {
            return true;
        }
        if local_gid >= info.glyph_count {
            return false;
        }
        let at = info.filled_off as usize + (local_gid / 32) as usize * 4;
        self.bytes
            .get(at..at + 4)
            .map(|slot| {
                let word = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
                word & (1 << (local_gid % 32)) != 0
            })
            .unwrap_or(false)
    }

    /// The byte range of one face's fill bitmap, for the delta a cover sends.
    pub fn bitmap_range(&self, face: usize) -> Option<(usize, usize)> {
        let info = self.faces.get(face)?;
        if info.all_filled {
            return None;
        }
        let len = info.bitmap_bytes() as usize;
        Some((info.filled_off as usize, len))
    }

    /// The faces, in directory order.
    pub fn faces(&self) -> &[FaceInfo] {
        &self.faces
    }

    /// The whole arena as the GPU holds it.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The front block — header and directory — which changes whenever a face
    /// is added and is the only part of the arena that is ever rewritten.
    pub fn front(&self) -> &[u8] {
        &self.bytes[..OT_TABLES_START as usize]
    }

    /// The scalars that ride beside the arena in `ShapeParams`.
    pub fn font(&self) -> ShapeFont {
        self.font
    }

    /// Tell the arena how many glyph records the curve buffers hold. The
    /// watermark is the floor; glyphs appended past a reserved range raise it.
    pub fn set_glyph_count(&mut self, glyphs: u32) {
        self.font.glyph_count = glyphs.max(self.watermark);
        if self.watermark < glyphs {
            self.watermark = glyphs;
        }
    }

    /// Append bytes, 4-aligned, and return the offset they landed at.
    fn append(&mut self, bytes: &[u8]) -> u32 {
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        let at = self.bytes.len() as u32;
        self.bytes.extend_from_slice(bytes);
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        at
    }

    /// Rewrite the header and the directory from [`Self::faces`].
    fn write_front(&mut self) {
        let header = OtHeader {
            version: SHAPE_ABI_VERSION,
            face_count: self.faces.len() as u32,
            arena_bytes: self.bytes.len() as u32,
            _rsvd0: 0,
        };
        self.bytes[..size_of::<OtHeader>()]
            .copy_from_slice(punchcutter_core::plain::bytes_of(&header));
        for (index, info) in self.faces.iter().enumerate() {
            let record = OtFace {
                glyph_base: info.glyph_base,
                glyph_count: info.glyph_count,
                upem: info.upem,
                num_h_metrics: info.num_h_metrics,
                cmap_off: info.tables[0].0,
                cmap_len: info.tables[0].1,
                hmtx_off: info.tables[1].0,
                hmtx_len: info.tables[1].1,
                gdef_off: info.tables[2].0,
                gdef_len: info.tables[2].1,
                gsub_off: info.tables[3].0,
                gsub_len: info.tables[3].1,
                gpos_off: info.tables[4].0,
                gpos_len: info.tables[4].1,
                filled_off: info.filled_off,
                flags: if info.all_filled {
                    OT_FACE_ALL_FILLED
                } else {
                    0
                },
            };
            let at = size_of::<OtHeader>() + index * size_of::<OtFace>();
            self.bytes[at..at + size_of::<OtFace>()]
                .copy_from_slice(punchcutter_core::plain::bytes_of(&record));
        }
    }
}

/// Round a length up to the 4-byte alignment the arena appends at.
fn pad4(len: u32) -> u32 {
    len.div_ceil(4) * 4
}

/// A big-endian `u16` out of a table, or `None` past its end.
fn be16(table: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*table.get(at)?, *table.get(at + 1)?]))
}

#[cfg(test)]
mod tests;
