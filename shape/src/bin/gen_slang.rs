//! Writes `slang/modules/shape_abi.slang` from the declarations in
//! `shape/src/abi.rs` (docs/arena.md, *Declared once*).
//!
//! The file is checked in — it travels inside `punchcutter-slang` with the
//! module that imports it — so this is run by hand after a declaration
//! changes. A test in the crate fails when the
//! file on disk differs from the rendering, and names this command.

use std::path::{Path, PathBuf};

fn main() {
    let path = slang_dir().join("shape_abi.slang");
    let source = punchcutter_shape::abi::slang_module();
    let changed = std::fs::read_to_string(&path)
        .map(|old| old != source)
        .unwrap_or(true);
    std::fs::write(&path, &source)
        .unwrap_or_else(|err| panic!("could not write {}: {err}", path.display()));
    println!(
        "{} {}",
        if changed { "wrote  " } else { "current" },
        path.display()
    );
}

/// The workspace's `slang/modules/`, resolved against this crate so the command does
/// not care about the working directory.
fn slang_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../slang/modules")
}
