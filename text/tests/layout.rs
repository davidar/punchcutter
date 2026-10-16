//! slangc's own per-field byte offsets against `offset_of!` on this side, so the
//! Rust and Slang halves of the curve buffers cannot drift apart silently.
//!
//! `shape/tests/layout.rs` makes the same check on the arena, down to the
//! courtesy it extends a machine with no slangc: a toolchain that will not
//! spawn skips the check rather than failing the suite. Everything else — a
//! renamed field, a reordered one, a `uint3` where three `uint`s were meant —
//! fails here, in a second, instead of as wrong pixels three layers down.

use std::path::PathBuf;

use punchcutter_core::slang::{self, SlangError};
use punchcutter_text::demo;
use punchcutter_text::layout;

fn demo_slang() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/slang/text_demo.slang")
}

/// No slangc, no check, still green.
fn is_missing_toolchain(err: &SlangError) -> Option<String> {
    match err {
        SlangError::Spawn { program, source } => Some(format!("{program}: {source}")),
        _ => None,
    }
}

#[test]
fn the_slang_layout_matches_this_one() {
    use core::mem::offset_of;
    use std::collections::BTreeMap;

    // Reflecting the demo entry point reflects `text.slang`'s whole parameter
    // block too: slangc lays out every struct reachable from the module, not
    // only what one entry point happens to read.
    let json = match slang::reflect(&demo_slang(), "text_demo", &[], &[]) {
        Ok(json) => json,
        Err(err) => match is_missing_toolchain(&err) {
            Some(why) => {
                eprintln!("skipping the Slang layout check: slangc is not runnable ({why})");
                return;
            }
            None => panic!("reflection failed:\n{err}"),
        },
    };

    let reflected = slang::struct_offsets(&json);

    // Field name on the Slang side, byte offset on this one. Both halves use the same identifiers throughout, so each Slang field is looked up by its Rust name.
    let expected: Vec<(&str, Vec<(&str, usize)>)> = vec![
        (
            "TextPoint",
            vec![
                ("p", offset_of!(layout::Point, p)),
                ("c", offset_of!(layout::Point, c)),
            ],
        ),
        (
            "TextBand",
            vec![
                ("count", offset_of!(layout::Band, count)),
                ("first", offset_of!(layout::Band, first)),
            ],
        ),
        (
            "TextGlyph",
            vec![
                ("band_base", offset_of!(layout::Glyph, band_base)),
                ("hband_count", offset_of!(layout::Glyph, hband_count)),
                ("vband_count", offset_of!(layout::Glyph, vband_count)),
                ("flags", offset_of!(layout::Glyph, flags)),
                ("band_transform", offset_of!(layout::Glyph, band_transform)),
                ("bounds", offset_of!(layout::Glyph, bounds)),
                ("advance", offset_of!(layout::Glyph, advance)),
                ("_rsvd0", offset_of!(layout::Glyph, _rsvd0)),
                ("_rsvd1", offset_of!(layout::Glyph, _rsvd1)),
                ("_rsvd2", offset_of!(layout::Glyph, _rsvd2)),
            ],
        ),
        (
            "TextPlacement",
            vec![
                ("glyph", offset_of!(demo::Placement, glyph)),
                ("px_per_em", offset_of!(demo::Placement, px_per_em)),
                ("origin", offset_of!(demo::Placement, origin)),
            ],
        ),
        (
            "TextDemoParams",
            vec![
                ("width", offset_of!(demo::Params, width)),
                ("height", offset_of!(demo::Params, height)),
                ("count", offset_of!(demo::Params, count)),
                ("_rsvd0", offset_of!(demo::Params, _rsvd0)),
            ],
        ),
    ];

    for (name, fields) in expected {
        let got = reflected
            .get(name)
            .unwrap_or_else(|| panic!("slangc reflected no struct named {name}"));
        let want: BTreeMap<String, usize> = fields
            .into_iter()
            .map(|(field, offset)| (field.to_string(), offset))
            .collect();
        assert_eq!(*got, want, "{name} disagrees with its Slang mirror");
    }
}

/// Sizes as well as offsets: a tail of reserved words that slangc pads past is a
/// buffer whose stride is wrong, and a stride is not a field offset.
#[test]
fn the_strides_are_what_the_buffers_assume() {
    assert_eq!(size_of::<layout::Point>(), 16);
    assert_eq!(size_of::<layout::Band>(), 8);
    assert_eq!(size_of::<layout::Glyph>(), 64);
    assert_eq!(size_of::<demo::Placement>(), 16);
    assert_eq!(size_of::<demo::Params>(), 16);
}
