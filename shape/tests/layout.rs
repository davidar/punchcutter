//! Slang's own per-field byte offsets for the shaping ABI, against
//! `offset_of!` on this side — the same check `text/tests/layout.rs` makes on
//! the curve buffers.
//!
//! The compile is `punchcutter_slang`'s, not a copy of it: the same settings
//! a host compiles the module with.
//!
//! A hand-kept Slang half would need a written-out list of every field and its
//! offset here — the drift check a generator exists to make unnecessary.
//! `slang/modules/shape_abi.slang` is generated
//! (`shape/src/abi.rs`, `the_checked_in_slang_is_the_rendering`), so what is
//! left here is the one thing a generator cannot promise: that **Slang lays
//! the rendered struct out the way Rust laid the declared one out**. The list
//! is walked out of the same table the renderer used, so adding a field to the
//! declaration adds it here.
//!
//! The table bytes behind the directory are OpenType's and need no declaration
//! on either side: the shader walks OpenType's own format.

use std::path::{Path, PathBuf};

use punchcutter_slang::Options;

fn slang_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../slang/modules")
}

#[test]
fn the_slang_layout_matches_this_one() {
    let source = slang_dir().join("shape_demo.slang");
    let reflected = punchcutter_slang::reflect(
        &source,
        punchcutter_shape::STAGE_ENTRIES[0],
        &Options::default(),
    )
    .unwrap_or_else(|err| panic!("reflection failed:\n{err}"))
    .structs;

    let mut checked = 0usize;
    for entry in punchcutter_shape::abi::structs() {
        let name = entry.decl.name;
        if !entry.bound {
            // Slang reflects the types a binding's layout needs and nothing
            // else, and the arena is bound as a word buffer — so `OtHeader` and
            // `OtFace` have no reflected layout to compare against. `ot.slang`
            // reads them word by word, which is what pins their order.
            assert!(
                !reflected.contains_key(name),
                "`{name}` is reflected after all; mark it bound and check it"
            );
            continue;
        }
        let Some(slang_side) = reflected.get(name) else {
            panic!("Slang did not reflect `{name}`; reflected: {reflected:?}");
        };
        for (field, offset) in (entry.offsets)() {
            assert_eq!(
                slang_side.get(field).copied(),
                Some(offset),
                "`{name}.{field}` is at {offset} here and at {:?} in Slang",
                slang_side.get(field)
            );
        }
        // A Slang field this side does not list is a word the shader reads and
        // nothing fills, which is the failure this test exists for.
        assert_eq!(
            slang_side.len(),
            entry.decl.fields.len(),
            "`{name}` has {} fields in Slang and {} here: {:?}",
            slang_side.len(),
            entry.decl.fields.len(),
            slang_side.keys().collect::<Vec<_>>()
        );
        checked += 1;
    }
    assert_eq!(checked, 7, "the seven structs a binding's layout reaches");
}

/// The two Slang files that are *not* generated do not declare the ABI's names
/// a second time: `shape_abi.slang` is the one declaration and both import it.
///
/// There is no second declaration to compare the constants with; what can
/// go wrong is someone writing a second copy beside the import, and a duplicate
/// `public static const` is a compile error in every importer at once.
#[test]
fn the_hand_written_modules_declare_none_of_the_abi() {
    let declared: Vec<&str> = punchcutter_shape::abi::structs()
        .iter()
        .map(|entry| entry.decl.name)
        .chain([
            "SHAPE_ABI_VERSION",
            "MAX_OT_FACES",
            "OT_TABLES_START",
            "OT_FACE_ALL_FILLED",
            "MAX_RUN_GLYPHS",
            "MAX_NESTING",
            "MAX_CONTEXT",
            "MAX_OPS",
            "MAX_SHAPE_FEATURES",
            "SHAPE_FEATURE_ON",
        ])
        .collect();
    for file in ["shape.slang", "ot.slang"] {
        let text = std::fs::read_to_string(slang_dir().join(file))
            .unwrap_or_else(|err| panic!("could not read {file}: {err}"));
        for name in &declared {
            for prefix in [
                format!("public static const uint {name} "),
                format!("public struct {name}"),
            ] {
                assert!(
                    !text.contains(&prefix),
                    "{file} declares `{name}` again; it is `shape_abi.slang`'s"
                );
            }
        }
        assert!(
            text.contains("__exported import shape_abi;"),
            "{file} must re-export the generated ABI, or its importers lose these names"
        );
    }
}
