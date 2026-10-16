//! punchcutter-shape — OpenType Layout as a Slang compute module.
//!
//! The rule this crate exists to obey is the engine's: the CPU keeps what is
//! **I/O**, and anything that is computation over data the GPU can hold
//! belongs in a shader. Shaping a string is computation over a font, and a
//! font is data.
//!
//! Flattening the font on the CPU would mean a second implementation of every lookup type. This crate takes the
//! other fork:
//!
//! * **this crate slices the font** — [`Arena`] finds `cmap`, `hmtx`, `GDEF`,
//!   `GSUB` and `GPOS` in the sfnt directory and copies them into one word
//!   buffer, verbatim, behind a record saying where each landed. That is a
//!   table directory and nothing else: no lookup, no coverage table, no
//!   subtable format is read on this side.
//! * **`slang/shape.slang` shapes** — walking OpenType's own binary format
//!   where it lies (`slang/ot.slang` is the reader), with the glyph
//!   iteration, feature selection, GSUB 1–8 and GPOS 1–9 that a default-shaper
//!   run needs. One thread per string.
//!
//! There is deliberately **no CPU shaper here**, not even a reference one: a
//! second implementation is a second set of answers, and the one thing this
//! crate must not ship is a quiet CPU path for a consumer to fall back into.
//! The reference is harfrust, in `examples/conform.rs`, which is a harness and
//! not a dependency of the library.
//!
//! # Many faces, one glyph space
//!
//! A face added to the arena reserves its **whole** glyph range in the merged
//! space at [`FaceInfo::glyph_base`], so its own `GSUB` and `GPOS` index the
//! merged ids by one addition and a fallback face kerns and substitutes with
//! itself. The curve records inside a reserved range are empty until a cover
//! fills them; [`Arena::mark_filled`] is the bitmap that says which, and a
//! glyph whose bit is clear raises [`SHAPE_RUN_NOTDEF`] so one goes out.
//!
//! # The interface to the rest of the text engine
//!
//! Glyph ids and positions, in the merged glyph index: the shaper's output is
//! that and nothing else, and it is the whole interface. `punchcutter-text` turns a
//! gid into curves and `slang/text.slang` draws them. Nothing here knows
//! what a glyph looks like, and nothing here draws.
//!
//! # Using it
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use punchcutter_shape::{Arena, Batch};
//!
//! let arena = Arena::load("/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf")?;
//! let mut batch = Batch::new();
//! batch.push("22:15", 16.0);
//! batch.push("Close", 16.0);
//! let params = batch.params(&arena);
//! // Upload `params`, `batch.requests()`, `batch.utf8_words()` and
//! // `arena.bytes()` (`slang/shape_demo.slang` binds the four in order);
//! // dispatch `batch.workgroups()` groups of `shape_batch`.
//! # Ok(()) }
//! ```

pub mod abi;
pub mod batch;
pub mod font;

pub use abi::{
    MAX_CONTEXT, MAX_NESTING, MAX_OPS, MAX_OT_FACES, MAX_RUN_GLYPHS, MAX_SHAPE_FEATURES,
    OT_FACE_ALL_FILLED, OT_TABLES_START, OtFace, OtHeader, SHAPE_ABI_VERSION, SHAPE_CLOCK_EVERY,
    SHAPE_FEATURE_ON, SHAPE_GLYPH_IGNORABLE, SHAPE_GLYPH_LIGATURE, SHAPE_GLYPH_MARK,
    SHAPE_GLYPH_NOTDEF, SHAPE_GLYPH_POSITIONED, SHAPE_GLYPH_SUBSTITUTED, SHAPE_GLYPH_UNFILLED,
    SHAPE_RUN_BAD_UTF8, SHAPE_RUN_DEADLINE, SHAPE_RUN_MULTI_FACE, SHAPE_RUN_NOTDEF,
    SHAPE_RUN_OUT_OF_OPS, SHAPE_RUN_POSITIONED, SHAPE_RUN_SUBSTITUTED, SHAPE_RUN_TRUNCATED,
    SHAPE_RUN_UNSUPPORTED, SHAPE_WORKGROUP, ShapeFont, ShapeParams, ShapeRequest, ShapeRun,
    ShapedGlyph,
};
pub use batch::{Batch, Feature};
pub use font::{Arena, FaceInfo, FontError};

/// The Slang modules this crate's arena is written for, as file names in
/// [`punchcutter_core::slang_dir`]: `slang_dir().join(SLANG_SHAPE)`. A shader
/// imports the second, which imports the first.
pub const SLANG_OT: &str = "ot.slang";
pub const SLANG_SHAPE: &str = "shape.slang";

/// The entry point in `slang/shape_demo.slang` that the harness dispatches.
pub const DEMO_ENTRY: &str = "shape_batch";
