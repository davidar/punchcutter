//! The shaping ABI: the arena the CPU fills and `slang/shape.slang` walks,
//! as `#[repr(C)]` types. docs/arena.md is the same contract as prose.
//!
//! The layout rules are the engine's (docs/arena.md, *Layout rules*): std430,
//! padding as scalar words and never a 3-vector.
//!
//! **Declared once**: every struct and constant below
//! is written through [`abi_struct!`](punchcutter_core::abi_struct) and
//! [`abi_consts!`](punchcutter_core::abi_consts), which emit the Rust item *and* a
//! table beside it, and `slang/shape_abi.slang` is that table rendered by
//! `cargo run -p punchcutter-shape --bin gen-slang`. `ot.slang` and `shape.slang`
//! import it. There is no hand-kept Slang half to drift; the macros live in
//! `punchcutter-core` so that a consumer can declare its own buffers the same
//! way.
//!
//! # The tables go over verbatim
//!
//! The alternative is to flatten bespoke tables out of a font — a `cmap`
//! sorted by codepoint, an advance array, the PairPos subtables, the ligature
//! list, the single-input chain rules — for `slang/shape.slang` to read.
//! Every lookup type the shaper gained that way would be written twice, once
//! as a Rust flattener that had to *understand* the lookup and once as a Slang
//! interpreter of the flattener's output.
//!
//! So this sends **OpenType's own bytes**. [`Arena`](crate::Arena) slices `cmap`,
//! `hmtx`, `GDEF`, `GSUB` and `GPOS` out of the file and concatenates them
//! behind a directory; the shader walks the spec's binary format where it
//! lies. The CPU's whole contribution is the table directory in this file —
//! offsets and lengths out of the sfnt header, which is I/O, the one thing the
//! CPU keeps — and nothing on this side understands a lookup.
//!
//! # The arena
//!
//! One `StructuredBuffer<uint>`, laid out as
//!
//! ```text
//! byte 0     OtHeader          version, face count, bytes in use
//! byte 16    OtFace * MAX_OT_FACES    the directory, fixed size so it never moves
//! byte 1040  the table bytes, 4-aligned, appended face by face
//! ```
//!
//! The directory has a **fixed capacity** because the arena is append-only: a
//! face opened later adds its tables at the end and rewrites the 1040-byte
//! front block, so a GPU copy is brought up to date by two writes (and a fill
//! bitmap per face that changed) and never by an upload of the whole thing.
//!
//! # Glyph ids are `glyph_base + face gid`
//!
//! One merged glyph space across every face — and a face that is opened
//! reserves its **whole** glyph range rather
//! than one id per covered codepoint, so its `GSUB` and `GPOS` index the merged
//! space by one addition and a fallback face's own kerning and substitutions
//! work. The curve records inside a reserved range are empty until a cover
//! request fills them, and [`OtFace::filled_off`] is the bitmap that says
//! which — a glyph whose bit is clear shapes to its id and raises
//! [`SHAPE_RUN_NOTDEF`], which is what sends the string back for cover.

use punchcutter_core::decl::{AbiStruct, ConstGroup};
use punchcutter_core::plain::Plain;
use punchcutter_core::{abi_consts, abi_struct};

abi_consts! {
    VERSIONS = "The arena's layout version, and the capacities it is sized by";

    /// The arena layout version: a label. Both halves are compiled from one
    /// tree and the generated Slang is the real guard.
    pub const SHAPE_ABI_VERSION: u32 = 4;

    /// Faces the directory holds. The primary, then one per fallback face a
    /// cover has opened; a consumer that has drawn sixteen scripts has a font
    /// problem rather than a directory problem, and the seventeenth is
    /// refused.
    pub const MAX_OT_FACES: u32 = 16;

    /// Bytes of the fixed front block: `OtHeader` and the whole directory.
    /// Every table offset in the arena is at or past this.
    pub const OT_TABLES_START: u32 = 16 + MAX_OT_FACES * 64;

    /// Glyph slots one string may occupy. A run that needs more is truncated
    /// with `SHAPE_RUN_TRUNCATED` — never a stall, never a partial write past
    /// the caller's slice.
    pub const MAX_RUN_GLYPHS: u32 = 256;

    /// Nested lookup levels a contextual rule may reach. HarfBuzz's own limit
    /// is 64; no common desktop font goes past 3, and a font that would is a
    /// flagged run and a stopped walk rather than a hang.
    pub const MAX_NESTING: u32 = 8;

    /// Input positions one contextual rule may name — the width of the
    /// `match_positions` array at each nesting level. HarfBuzz's is 64.
    pub const MAX_CONTEXT: u32 = 32;

    /// Subtable steps one string may cost before the shaper gives up,
    /// HarfBuzz's `max_ops` with the buffer length taken out of it: a
    /// pathological font cannot make a consumer's frame late.
    pub const MAX_OPS: u32 = 4096;

    /// Feature tags one dispatch may force on or off. A default-features
    /// caller sends none; the conformance harness (shape/examples/conform.rs)
    /// sends one, `test`, which Adobe's OpenType test fonts use; and a
    /// language's `locl` and a complex script's features are what the seam is
    /// for.
    pub const MAX_SHAPE_FEATURES: u32 = 16;

    /// `Digest`'s three masks, as 32-bit halves: HarfBuzz's `hb_set_digest_t`
    /// is three 64-bit words and Slang has no 64-bit integer here.
    pub const SHAPE_DIGEST_WORDS: u32 = 6;

    /// How often the budget helper re-reads the device clock: every 256 ops.
    /// A clock read is an instruction, not a memory access, but it is not free
    /// and a run of a dozen glyphs spends a few hundred ops in all.
    pub const SHAPE_CLOCK_EVERY: u32 = 256;
}

abi_consts! {
    RUN_FLAGS = "What one run did: `ShapeRun::flags`";

    /// Some codepoint in the string had no glyph, or had one whose curves have
    /// not been covered yet; either way the host opens a face that covers it.
    pub const SHAPE_RUN_NOTDEF: u32 = 1 << 0;
    /// The run needed more glyph slots than it was given; the tail is missing.
    ///
    /// With `Batch::push`'s allocation, `min(bytes * 4 + 8, MAX_RUN_GLYPHS)`
    /// slots, this is a **bug rather than a size** for any string short of the
    /// cap: a Latin label cannot multiply past it.
    pub const SHAPE_RUN_TRUNCATED: u32 = 1 << 1;
    /// The UTF-8 decoder hit a byte sequence it could not read.
    pub const SHAPE_RUN_BAD_UTF8: u32 = 1 << 2;
    /// At least one GSUB lookup changed the run.
    pub const SHAPE_RUN_SUBSTITUTED: u32 = 1 << 3;
    /// At least one GPOS lookup moved something.
    pub const SHAPE_RUN_POSITIONED: u32 = 1 << 4;
    /// The operation budget (`MAX_OPS`) ran out and the rest of the lookups
    /// were not applied. The sibling of `SHAPE_RUN_TRUNCATED`, for time rather
    /// than room.
    pub const SHAPE_RUN_OUT_OF_OPS: u32 = 1 << 5;
    /// The string used more than one face, so its segments were shaped apart.
    pub const SHAPE_RUN_MULTI_FACE: u32 = 1 << 6;
    /// The shader met a subtable format, an anchor format or a table version
    /// it does not walk. The run is drawn anyway.
    pub const SHAPE_RUN_UNSUPPORTED: u32 = 1 << 7;
    /// The run passed `ShapeParams::deadline_ticks` of **device wall clock**
    /// and stopped where it stood.
    ///
    /// The guardrail that holds when the operation accounting is wrong, which
    /// is the one thing counting cannot promise: `VK_KHR_shader_clock` gives
    /// the kernel a clock it can read, and the budget helper reads it every
    /// `SHAPE_CLOCK_EVERY` ops. A driver without the extension leaves
    /// `deadline_ticks` at zero and this never fires — and the caller logs that rather than dropping it quietly.
    pub const SHAPE_RUN_DEADLINE: u32 = 1 << 8;
}

abi_consts! {
    GLYPH_FLAGS = "What happened to one glyph: `ShapedGlyph::flags`";

    /// This glyph is `.notdef` standing in for a codepoint no face has.
    pub const SHAPE_GLYPH_NOTDEF: u32 = 1 << 0;
    /// A GSUB lookup put this glyph here.
    pub const SHAPE_GLYPH_SUBSTITUTED: u32 = 1 << 1;
    /// A GPOS lookup moved this glyph.
    pub const SHAPE_GLYPH_POSITIONED: u32 = 1 << 2;
    /// This glyph stands for a default-ignorable codepoint — a zero-width
    /// space, a soft hyphen, a bidi control, a variation selector. It is the
    /// face's space glyph with no advance, which is what a full shaper hides
    /// them as.
    pub const SHAPE_GLYPH_IGNORABLE: u32 = 1 << 3;
    /// This glyph is a ligature: two or more input glyphs became it.
    pub const SHAPE_GLYPH_LIGATURE: u32 = 1 << 4;
    /// GDEF calls this glyph a mark, so it is attached rather than advanced.
    pub const SHAPE_GLYPH_MARK: u32 = 1 << 5;
    /// The glyph id is inside a reserved range whose curves have not arrived
    /// yet (`OtFace::filled_off`). It draws blank until a cover fills it, and
    /// the run carries `SHAPE_RUN_NOTDEF` so one goes out.
    pub const SHAPE_GLYPH_UNFILLED: u32 = 1 << 6;
}

abi_consts! {
    FACE_FLAGS = "`OtFace::flags`, and the one word a feature request carries";

    /// Every glyph of this face has its curves, so the shader need not read
    /// the fill bitmap. True of the primary from the first frame.
    pub const OT_FACE_ALL_FILLED: u32 = 1 << 0;

    /// A feature word's high bit: set means **force on**, clear means force
    /// off. An OpenType tag is four ASCII bytes, so bit 31 of the packed tag is
    /// always free — and a word of zero is an empty slot, because no tag is 0.
    pub const SHAPE_FEATURE_ON: u32 = 0x8000_0000;
}

/// What a **host with no clock probe** puts in [`ShapeParams::deadline_ticks`].
///
/// The realtime clock's rate is a vendor constant no Vulkan limit reports, and
/// two common adapters are an order of magnitude apart: RADV on
/// an integrated Radeon measures 102 360 ticks a millisecond and lavapipe
/// 1 004 746 (`shape/examples/garbage.rs` measures it). This number is 250 ms at the
/// slower of the two and 25 ms at the faster, and either is far above any
/// dispatch a real font produces and far below the seconds a driver waits
/// before it resets a ring.
///
/// The harness measures instead, because it can dispatch a probe. A consumer
/// can take this, because measuring would cost it an entry point for a
/// guardrail whose whole job is to be loose.
pub const SHAPE_DEADLINE_TICKS: u32 = 25_000_000;

/// Threads per workgroup in the shaping kernel: one thread shapes one string.
///
/// Not in the generated module: every entry point that shapes (`shape_demo.slang`,
/// a consumer's) spells its own `[numthreads]`, which cannot take a constant
/// anyway.
pub const SHAPE_WORKGROUP: u32 = 64;

abi_struct! {
    /// The arena's front block: what the directory behind it holds.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct OtHeader {
        /// `SHAPE_ABI_VERSION`, so a buffer read back in a debugger names its layout.
        pub version: u32,
        /// Faces in the directory, at most `MAX_OT_FACES`.
        pub face_count: u32,
        /// Bytes of the arena in use, the front block included. What the
        /// shader bounds every offset against.
        pub arena_bytes: u32,
        pub _rsvd0: u32,
    }
}

// SAFETY: repr(C), four 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for OtHeader {}

abi_struct! {
    /// One face: where its glyphs sit in the merged space, and where its five
    /// tables sit in the arena.
    ///
    /// Every `*_off` is a **byte offset from the arena's start**, 4-aligned,
    /// and zero when the face lacks the table; every `*_len` is that table's
    /// length. The shader adds the offset to the subtable-relative offsets
    /// OpenType states and bounds the result against `OtHeader::arena_bytes`.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct OtFace {
        /// The first merged glyph id this face owns. Face 0 (the primary) is 0.
        pub glyph_base: u32,
        /// `maxp`'s glyph count: the whole range the face reserved.
        pub glyph_count: u32,
        /// `head.unitsPerEm`, this face's own.
        pub upem: u32,
        /// `hhea.numberOfHMetrics`, the bound the `hmtx` walk clamps to.
        pub num_h_metrics: u32,
        pub cmap_off: u32,
        pub cmap_len: u32,
        pub hmtx_off: u32,
        pub hmtx_len: u32,
        pub gdef_off: u32,
        pub gdef_len: u32,
        pub gsub_off: u32,
        pub gsub_len: u32,
        pub gpos_off: u32,
        pub gpos_len: u32,
        /// A bitmap of `ceil(glyph_count / 32)` words, one bit per face glyph,
        /// set when that glyph's curves are in the curve buffers. Meaningless —
        /// and not read — when `OT_FACE_ALL_FILLED` is set.
        pub filled_off: u32,
        /// `OT_FACE_*`.
        pub flags: u32,
    }
}

// SAFETY: repr(C), sixteen 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for OtFace {}

abi_struct! {
    /// The scalars that are the *merged font's* rather than any one face's.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct ShapeFont {
        /// The primary's design units per em — the divisor a caller's line box
        /// uses.
        pub upem: u32,
        /// Glyph records in the merged space: the bound on every id the shader
        /// writes, and the length of the curve record table behind it.
        pub glyph_count: u32,
        /// The primary's `hhea` ascender, in its design units. The shader does
        /// not use it; it is here so a caller need not open the font again.
        pub ascender: i32,
        /// The primary's `hhea` descender (negative).
        pub descender: i32,
    }
}

// SAFETY: repr(C), four 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for ShapeFont {}

abi_struct! {
    /// The dispatch's scalars: which font, how many strings, how much room, and
    /// the feature pool a request's range indexes.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct ShapeParams {
        pub font: ShapeFont,
        /// Strings in this dispatch. A thread above this does nothing.
        pub request_count: u32,
        /// Total glyph slots in the output array, for the bounds check.
        pub glyph_capacity: u32,
        /// `SHAPE_ABI_VERSION`, so a buffer read back in a debugger names its layout.
        pub version: u32,
        /// Ticks of `getRealtimeClock` one run may spend before it stops with
        /// `SHAPE_RUN_DEADLINE`. **Zero disables it**, which is what a
        /// device without `VK_KHR_shader_clock` gets — and the caller says so.
        ///
        /// The realtime clock is a wall clock at a vendor-fixed rate that no
        /// Vulkan limit reports, so a caller either measures it with one probe dispatch against a known wall-clock interval (the harness) or takes `SHAPE_DEADLINE_TICKS`.
        pub deadline_ticks: u32,
        /// The dispatch's feature pool: `MAX_SHAPE_FEATURES` words, each
        /// a packed OpenType tag with `SHAPE_FEATURE_ON` set to force the
        /// feature on and clear to force it off. A zero word is an empty slot.
        ///
        /// Four-wide because this is a `ConstantBuffer`: std140 gives an array
        /// of scalars a stride of sixteen bytes, and a `uint4` is the one
        /// spelling whose Slang offsets are the Rust ones.
        pub features: [[u32; 4]; 4],
    }
}

// SAFETY: repr(C); a `Plain` struct, four 4-byte leaves and sixteen more.
unsafe impl Plain for ShapeParams {}

abi_struct! {
    /// One string to shape.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct ShapeRequest {
        /// Byte offset of the string in the UTF-8 buffer.
        pub byte_start: u32,
        /// Its length in bytes.
        pub byte_len: u32,
        /// Device pixels per em. The positions come back in 26.6 fixed point at
        /// this size; nothing else in the ABI is scale-dependent.
        pub px_size: f32,
        /// First output glyph slot for this run.
        pub out_start: u32,
        /// Slots this run may use, at most `MAX_RUN_GLYPHS`.
        pub out_cap: u32,
        /// First word of `ShapeParams::features` this run forces, and how
        /// many. Two words, then padding to 32 B.
        pub feature_start: u32,
        pub feature_count: u32,
        pub _rsvd0: u32,
    }
}

// SAFETY: repr(C), eight 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for ShapeRequest {}

abi_struct! {
    /// One positioned glyph: the whole interface to the curve renderer
    /// (`punchcutter-text`), plus the cluster a bidi pass would need.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct ShapedGlyph {
        /// The glyph id in the **merged** space: its face's `glyph_base` plus
        /// the id that face's own tables named.
        pub gid: u32,
        /// Pen position in 26.6 fixed point (64ths of a device pixel), x offset
        /// from GPOS included.
        pub x: i32,
        /// The same on y — a mark's attachment, or zero.
        pub y: i32,
        /// The byte offset in the request's string of the first codepoint this
        /// glyph came from — HarfBuzz's cluster, merged the way
        /// `merge_clusters` merges one at `cluster_level` 0: a
        /// ligature, a contextual match and a multiple substitution take the
        /// **minimum** cluster over their span, in both directions.
        pub cluster: u32,
        /// `SHAPE_GLYPH_*`.
        pub flags: u32,
        pub _rsvd0: u32,
        pub _rsvd1: u32,
        pub _rsvd2: u32,
    }
}

// SAFETY: repr(C), eight 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for ShapedGlyph {}

abi_struct! {
    /// What one request produced.
    { #[repr(C)] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
    pub struct ShapeRun {
        pub glyph_count: u32,
        /// The run's total advance in 26.6 fixed point.
        pub advance: i32,
        /// `SHAPE_RUN_*`.
        pub flags: u32,
        /// The deepest nested-lookup level the run reached, so the conformance harness can report how deep a test corpus actually nests against `MAX_NESTING`.
        pub depth: u32,
    }
}

// SAFETY: repr(C), four 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for ShapeRun {}

// ---------------------------------------------------------------------------
// The tables `slang/shape_abi.slang` is rendered from
// ---------------------------------------------------------------------------

/// How the generated module says where it came from.
pub const GEN_COMMAND: &str = "cargo run -p punchcutter-shape --bin gen-slang";

const NORMATIVE: &[&str] = &[
    "`shape/src/abi.rs` is the one declaration; this file is that declaration",
    "rendered. Edit the Rust and re-run the command above.",
];

const CONST_GROUPS: &[ConstGroup] = &[VERSIONS, RUN_FLAGS, GLYPH_FLAGS, FACE_FLAGS];

/// `bound` is whether the running shader reaches the struct from one of its
/// bindings, which is what the reflection test can compare an offset against.
///
/// The arena's two are **not**: the arena is a `StructuredBuffer<uint>` — it has
/// to be, since the tables behind the directory are OpenType's own bytes — so
/// slangc reflects no layout for `OtHeader` or `OtFace` and there is nothing to
/// compare. What pins their field order on the Slang side is `ot.slang`'s
/// `otFaceAt`, which reads sixteen consecutive words by name.
const STRUCTS: &[AbiStruct] = &[
    AbiStruct {
        decl: OtHeader::DECL,
        bound: false,
        offsets: OtHeader::offsets,
    },
    AbiStruct {
        decl: OtFace::DECL,
        bound: false,
        offsets: OtFace::offsets,
    },
    AbiStruct {
        decl: ShapeFont::DECL,
        bound: true,
        offsets: ShapeFont::offsets,
    },
    AbiStruct {
        decl: ShapeParams::DECL,
        bound: true,
        offsets: ShapeParams::offsets,
    },
    AbiStruct {
        decl: ShapeRequest::DECL,
        bound: true,
        offsets: ShapeRequest::offsets,
    },
    AbiStruct {
        decl: ShapedGlyph::DECL,
        bound: true,
        offsets: ShapedGlyph::offsets,
    },
    AbiStruct {
        decl: ShapeRun::DECL,
        bound: true,
        offsets: ShapeRun::offsets,
    },
];

/// The generated module's own header comment.
const MODULE_DOC: &[&str] = &[
    "The shaping ABI: the arena `punchcutter-shape` fills and `ot.slang` walks, and",
    "the dispatch's own scalars.",
    "",
    "Both halves of this contract are built from one tree, so it has no binary",
    "stability to keep across builds — `SHAPE_ABI_VERSION` is a courtesy for",
    "whoever reads a buffer dump. What it does have is two *readers*:",
    "`ot.slang` reads the front block, `shape.slang` reads the request and",
    "writes the glyph, and both import this module rather than keep a",
    "hand-written copy of these structs.",
    "",
    "The table bytes behind the directory are OpenType's own and are declared",
    "nowhere: the CPU sends the font's tables verbatim and the shader walks",
    "OpenType's format, so a new lookup type is a shader edit and never a",
    "second format to keep in step.",
];

/// `slang/shape_abi.slang`, rendered from the tables above.
pub fn slang_module() -> String {
    punchcutter_core::decl::render(
        GEN_COMMAND,
        NORMATIVE,
        "shape_abi",
        "shape/src/abi.rs",
        MODULE_DOC,
        &[],
        CONST_GROUPS,
        STRUCTS,
    )
}

/// The declared structs, for the reflection test in `shape/tests/layout.rs`:
/// slangc's own per-field offsets against `offset_of!`, walked from the table
/// rather than from a hand-written list of it.
pub fn structs() -> &'static [AbiStruct] {
    STRUCTS
}

#[cfg(test)]
mod tests;
