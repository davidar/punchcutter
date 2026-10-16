//! The four buffers, as `#[repr(C)]` types `slang/text.slang` mirrors.
//!
//! Layout rules are the engine's (docs/arena.md, *Layout rules*): std430,
//! every struct 16-byte aligned where it carries a vector, every reserved tail
//! a run of scalar words rather than a `uint3` — std430 aligns a 3-vector to
//! 16 B and slangc's reflection puts one at the wrong offset. `tests/layout.rs` compares
//! slangc's own per-field byte offsets against `offset_of!` here.
//!
//! # What the reference's textures became
//!
//! Slug's reference shaders read two 4096-wide textures: a `RGBA16F` *curve*
//! texture holding control points two-to-a-texel, and a `RG16UI` *band* texture
//! holding both the per-band `(count, offset)` records and the curve-location
//! lists, with an offset that wraps at the texture width (`CalcBandLoc`). We
//! are on Vulkan through wgpu's passthrough and a storage buffer indexes
//! flatly, so:
//!
//! - the curve texture is [`Point`]`[]` — one entry per texel, still two points
//!   wide, so a contour's curves still *share* endpoints between neighbouring
//!   entries and a curve is still named by one index;
//! - the band texture splits in two — [`Band`]`[]` for the records and a plain
//!   `u32[]` for the curve-location lists — which deletes `CalcBandLoc`, the
//!   wrap arithmetic, and the 16-bit ceiling on a glyph's data size;
//! - the per-glyph constants that the reference packs into two vertex
//!   attributes (`tex.zw`, `bnd`) are [`Glyph`]`[]`, indexed by glyph id, so a
//!   glyph is an index and nothing per-glyph travels with a draw.

/// One entry of the curve buffer: an on-curve point and the control point that
/// follows it, in em units with y up and the origin at the glyph's pen.
///
/// Quadratic curve `i` is `{points[i].p, points[i].c, points[i + 1].p}` — the
/// reference's packing, kept exactly, because it is what makes a contour's
/// `n` curves cost `n + 1` entries instead of `3n`. A contour is therefore
/// always a *contiguous* run, and the run's last entry carries only a point.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// The on-curve point this curve starts at.
    pub p: [f32; 2],
    /// The off-curve control point. Unused in a contour run's last entry.
    pub c: [f32; 2],
}

/// One band record: how many curves cross this band, and where their indices
/// start in the curve-location buffer.
///
/// A glyph's horizontal records come first, `hband_count` of them, then its
/// vertical records. Within a band the curves are sorted in *descending* order
/// of their maximum x (horizontal bands) or y (vertical bands), which is the
/// property the coverage walk's early `break` rests on.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Band {
    pub count: u32,
    pub first: u32,
}

/// One entry of the curve-location buffer: the index of a curve some band lists.
///
/// A newtype over `u32` and nothing more. The Slang side declares this buffer as
/// a plain `StructuredBuffer<uint>`, which has the same four-byte stride; the
/// wrapper exists only because [`punchcutter_core::plain::Plain`] is `punchcutter-core`'s trait
/// and `u32` is not this crate's type, so the orphan rule forbids the obvious
/// implementation. Keeping it means every one of the four buffers uploads through
/// the same byte view as every other buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CurveRef(pub u32);

/// Everything the coverage walk needs to know about one glyph.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Glyph {
    /// Index into the band buffer of this glyph's first *horizontal* record.
    pub band_base: u32,
    /// Horizontal bands (rows, split along y). Zero means "no ink".
    pub hband_count: u32,
    /// Vertical bands (columns, split along x). Zero means "no ink".
    pub vband_count: u32,
    /// [`FILL_EVEN_ODD`], and room for more.
    pub flags: u32,
    /// `(1/band width, 1/band height, -min x/band width, -min y/band height)`
    /// — the reference's `bnd` attribute, which turns a sample position into a
    /// band index with one multiply-add. Zero scale for a glyph with no ink.
    pub band_transform: [f32; 4],
    /// Ink bounds over the control points, em units: `(min x, min y, max x,
    /// max y)`. The convex hull of a quadratic's control points contains the
    /// curve, so every curve lies inside this and the band grid spans it.
    pub bounds: [f32; 4],
    /// Horizontal advance, em units: `hmtx` for a font's glyph, whatever the
    /// caller passed to [`crate::Builder::push`] for its own outline.
    pub advance: f32,
    pub _rsvd0: u32,
    pub _rsvd1: u32,
    pub _rsvd2: u32,
}

/// [`Glyph::flags`]: fill with the even-odd rule rather than non-zero.
pub const FILL_EVEN_ODD: u32 = 1;

/// The CPU-side index: `gid -> (curve range, band range, curve-location range, advance, bbox)`. The GPU never reads it — [`Glyph`] carries the band range, advance and bounds the walk needs — but it is what answers "how big is this
/// glyph" without a second pass over the buffers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Index {
    /// Curve-buffer range, as entries (one more than the curve count for each
    /// contour — see [`Point`]).
    pub points: (u32, u32),
    /// Band-record range: `hband_count + vband_count` records from
    /// [`Glyph::band_base`].
    pub bands: (u32, u32),
    /// Curve-location range in the band-curves buffer.
    pub band_curves: (u32, u32),
    pub advance: f32,
    pub bounds: [f32; 4],
}

// SAFETY for all four: `#[repr(C)]` aggregates of `f32`/`u32` with no padding
// beyond what the explicit `_rsvd*` words and `align(16)` account for, which is
// exactly what `Plain` asks for. `tests/layout.rs` pins every offset against
// slangc's reflection of the Slang mirror.
unsafe impl punchcutter_core::plain::Plain for Point {}
unsafe impl punchcutter_core::plain::Plain for CurveRef {}
unsafe impl punchcutter_core::plain::Plain for Band {}
unsafe impl punchcutter_core::plain::Plain for Glyph {}

#[cfg(test)]
mod tests;
