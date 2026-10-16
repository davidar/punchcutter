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
//! And the engine's Slang modules themselves, in [`modules`]: compiled into
//! this crate, and written out by [`slang_dir`] for a consumer that compiles a
//! shader importing them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod decl;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod plain;
pub mod slang;

/// The engine's Slang modules, compiled into this crate.
///
/// A consumer's shader says `import text;` or `import shape;`; `shape`
/// imports `ot` and `shape_abi`. The four travel inside any binary built on
/// this crate, so the engine a program was built against is the engine it
/// runs with, and no checkout has to be where it was when the program was
/// built. [`write_to`](modules::write_to) puts them where slangc can find
/// them; [`slang_dir`] does that once into a cache directory.
pub mod modules {
    use std::io;
    use std::path::Path;

    /// Glyph and icon coverage.
    pub const TEXT: &str = include_str!("../slang/text.slang");
    /// The shaper.
    pub const SHAPE: &str = include_str!("../slang/shape.slang");
    /// OpenType's binary format, walked in place.
    pub const OT: &str = include_str!("../slang/ot.slang");
    /// The arena's header, rendered from `punchcutter-shape`'s declarations.
    pub const SHAPE_ABI: &str = include_str!("../slang/shape_abi.slang");

    /// Every module with the file name an `import` resolves to.
    pub const ALL: [(&str, &str); 4] = [
        ("text.slang", TEXT),
        ("shape.slang", SHAPE),
        ("ot.slang", OT),
        ("shape_abi.slang", SHAPE_ABI),
    ];

    /// One number for the four, FNV-1a over their names and texts: what
    /// [`slang_dir`](super::slang_dir) names its directory by, so two
    /// versions of the engine on one machine never read each other's files.
    pub fn hash() -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for (name, text) in ALL {
            for byte in name.bytes().chain([0]).chain(text.bytes()).chain([0]) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    /// Writes the four modules into `dir`, creating it, so that `dir` can go
    /// on slangc's include path (the `includes` argument of
    /// [`slang::compile_with`](super::slang::compile_with) and its siblings,
    /// or `-I` on a command line). A build script writes them under
    /// `OUT_DIR`; a process that compiles at run time can call
    /// [`slang_dir`](super::slang_dir) instead.
    pub fn write_to(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, text) in ALL {
            std::fs::write(dir.join(name), text)?;
        }
        Ok(())
    }

    /// Whether `dir` already holds the four, byte for byte.
    pub(super) fn is_written(dir: &Path) -> bool {
        ALL.iter().all(|(name, text)| {
            std::fs::read(dir.join(name)).is_ok_and(|on_disk| on_disk == text.as_bytes())
        })
    }
}

/// A directory holding the engine's Slang modules ([`modules`]), for a
/// process that compiles a shader importing them at run time: pass it to
/// slangc as an include path.
///
/// The modules are written from the copies compiled into this crate, so the
/// directory exists wherever the binary runs. It is
/// `$XDG_CACHE_HOME/punchcutter/slang/<hash>/` (`~/.cache` when that is
/// unset), named by [`modules::hash`] so each version of the engine has its
/// own; the system temporary directory stands in when the cache is not
/// writable. Written once, atomically (into a sibling, then renamed), and
/// checked rather than rewritten when already there. Resolved once per
/// process, so a caller can hash or compare it.
///
/// When no directory can be written the last one tried comes back as it is,
/// and the compile that uses it reports the missing module, with the reason
/// on stderr here.
pub fn slang_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let name = format!("{:016x}", modules::hash());
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
        let roots = cache.into_iter().chain([std::env::temp_dir()]);
        let mut last = PathBuf::from(&name);
        for root in roots {
            let dir = root.join("punchcutter").join("slang").join(&name);
            if modules::is_written(&dir) {
                return dir;
            }
            match materialise(&dir) {
                Ok(()) => return dir,
                Err(err) => eprintln!("punchcutter: cannot write {}: {err}", dir.display()),
            }
            last = dir;
        }
        last
    })
}

/// Writes the modules into a sibling of `dir` and renames it into place, so
/// no reader ever sees a half-written directory. Two processes doing this at
/// once both succeed: the rename that loses finds the winner's copy in place.
fn materialise(dir: &Path) -> std::io::Result<()> {
    let parent = dir
        .parent()
        .expect("a directory under a cache root has a parent");
    let staging = parent.join(format!(
        "{}.tmp-{}",
        dir.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("slang"),
        std::process::id()
    ));
    modules::write_to(&staging)?;
    match std::fs::rename(&staging, dir) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            if modules::is_written(dir) {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests;
