//! punchcutter-core — the machinery under the engine's Slang modules.
//!
//! The text engine is three Slang modules and the CPU code that fills their
//! buffers. What the two halves share is mechanism, and that is all this crate
//! is: the slangc driver (compile, reflect, validate), the byte views that
//! upload plain structs to the GPU, the declare-once ABI renderer that keeps a
//! Rust struct and its Slang mirror from drifting, and a few wgpu helpers.
//! Nothing here decides anything.
//!
//! # The modules
//!
//! - [`slang`] — the slangc driver: compile, reflect, validate. Used by build
//!   scripts, by the layout tests, and by a consumer that compiles its own
//!   shader against the engine's modules.
//! - [`plain`] — the `Plain` trait and its three byte views, the only unsafe
//!   code on the upload path.
//! - [`decl`] — **declared once**: the two `macro_rules!` an ABI struct or
//!   constant is written through, and the renderer that turns their tables into
//!   a checked-in Slang module.
//! - `gpu` — the wgpu-typed helpers, behind the `gpu` feature, so a build
//!   script can depend on this crate without pulling in wgpu.
//!
//! And one function, [`slang_dir`]: where the engine's Slang modules are, for
//! a consumer that compiles a shader importing them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod decl;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod plain;
pub mod slang;

/// The directory holding the engine's Slang modules: `text.slang`,
/// `shape.slang`, `ot.slang` and `shape_abi.slang`, plus the two demos.
///
/// A consumer whose shader says `import text;` or `import shape;` passes this
/// directory to slangc as an include path (the `includes` argument of
/// [`slang::compile`] and its siblings, or `-I` on a command line of its own).
/// It works the same from a build script, where this crate is a
/// build-dependency, and from a process that compiles at run time.
///
/// The path is the repository's own `slang/`, beside this crate's manifest,
/// fixed when this crate was compiled. A path or git dependency builds from a
/// checkout that holds the whole repository, so the directory is there on the
/// machine that built the binary; a binary moved to a machine without that
/// checkout has to bring its own copy and name it instead. Canonicalised once,
/// so a caller can hash or compare it; when it cannot be (the checkout is
/// gone), the uncanonicalised path comes back and the compile that uses it
/// reports the missing module.
pub fn slang_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../slang"));
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
    })
}

#[cfg(test)]
mod tests;
