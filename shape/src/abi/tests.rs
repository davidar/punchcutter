use super::*;
use core::mem::offset_of;

/// Every offset and size the shader assumes, on this side. The Slang half is
/// generated from the same declaration (`the_checked_in_slang_is_the_rendering`
/// below) and its *reflected* offsets are compared against these in
/// `tests/layout.rs`.
#[test]
fn the_sizes_and_offsets_are_what_the_shader_assumes() {
    assert_eq!(size_of::<OtHeader>(), 16);
    assert_eq!(offset_of!(OtHeader, version), 0);
    assert_eq!(offset_of!(OtHeader, face_count), 4);
    assert_eq!(offset_of!(OtHeader, arena_bytes), 8);

    assert_eq!(size_of::<OtFace>(), 64);
    assert_eq!(offset_of!(OtFace, glyph_base), 0);
    assert_eq!(offset_of!(OtFace, glyph_count), 4);
    assert_eq!(offset_of!(OtFace, upem), 8);
    assert_eq!(offset_of!(OtFace, num_h_metrics), 12);
    assert_eq!(offset_of!(OtFace, cmap_off), 16);
    assert_eq!(offset_of!(OtFace, cmap_len), 20);
    assert_eq!(offset_of!(OtFace, hmtx_off), 24);
    assert_eq!(offset_of!(OtFace, hmtx_len), 28);
    assert_eq!(offset_of!(OtFace, gdef_off), 32);
    assert_eq!(offset_of!(OtFace, gdef_len), 36);
    assert_eq!(offset_of!(OtFace, gsub_off), 40);
    assert_eq!(offset_of!(OtFace, gsub_len), 44);
    assert_eq!(offset_of!(OtFace, gpos_off), 48);
    assert_eq!(offset_of!(OtFace, gpos_len), 52);
    assert_eq!(offset_of!(OtFace, filled_off), 56);
    assert_eq!(offset_of!(OtFace, flags), 60);

    // The front block is the header and a fixed directory, and every table
    // offset in the arena is at or past it.
    assert_eq!(OT_TABLES_START, 16 + 16 * 64);
    assert_eq!(
        OT_TABLES_START as usize,
        size_of::<OtHeader>() + 16 * size_of::<OtFace>()
    );

    assert_eq!(size_of::<ShapeFont>(), 16);
    assert_eq!(offset_of!(ShapeFont, upem), 0);
    assert_eq!(offset_of!(ShapeFont, glyph_count), 4);
    assert_eq!(offset_of!(ShapeFont, ascender), 8);
    assert_eq!(offset_of!(ShapeFont, descender), 12);

    // 32 bytes of scalars, then the feature pool. The pool is 16-byte
    // aligned where it lands, which is what a `ConstantBuffer` needs of it.
    assert_eq!(size_of::<ShapeParams>(), 96);
    assert_eq!(offset_of!(ShapeParams, font), 0);
    assert_eq!(offset_of!(ShapeParams, request_count), 16);
    assert_eq!(offset_of!(ShapeParams, glyph_capacity), 20);
    assert_eq!(offset_of!(ShapeParams, version), 24);
    assert_eq!(offset_of!(ShapeParams, deadline_ticks), 28);
    assert_eq!(offset_of!(ShapeParams, features), 32);
    assert_eq!(offset_of!(ShapeParams, features) % 16, 0);
    assert_eq!(MAX_SHAPE_FEATURES as usize, 4 * 4);

    assert_eq!(size_of::<ShapeRequest>(), 32);
    assert_eq!(offset_of!(ShapeRequest, byte_start), 0);
    assert_eq!(offset_of!(ShapeRequest, byte_len), 4);
    assert_eq!(offset_of!(ShapeRequest, px_size), 8);
    assert_eq!(offset_of!(ShapeRequest, out_start), 12);
    assert_eq!(offset_of!(ShapeRequest, out_cap), 16);
    // The feature range's two words, then one of padding.
    assert_eq!(offset_of!(ShapeRequest, feature_start), 20);
    assert_eq!(offset_of!(ShapeRequest, feature_count), 24);

    assert_eq!(size_of::<ShapedGlyph>(), 32);
    assert_eq!(offset_of!(ShapedGlyph, gid), 0);
    assert_eq!(offset_of!(ShapedGlyph, x), 4);
    assert_eq!(offset_of!(ShapedGlyph, y), 8);
    assert_eq!(offset_of!(ShapedGlyph, cluster), 12);
    assert_eq!(offset_of!(ShapedGlyph, flags), 16);

    assert_eq!(size_of::<ShapeRun>(), 16);
    assert_eq!(offset_of!(ShapeRun, glyph_count), 0);
    assert_eq!(offset_of!(ShapeRun, advance), 4);
    assert_eq!(offset_of!(ShapeRun, flags), 8);
    assert_eq!(offset_of!(ShapeRun, depth), 12);
}

/// The flag words are disjoint, because the harness reads them as a set.
#[test]
fn the_flag_bits_do_not_collide() {
    let run = [
        SHAPE_RUN_NOTDEF,
        SHAPE_RUN_TRUNCATED,
        SHAPE_RUN_BAD_UTF8,
        SHAPE_RUN_SUBSTITUTED,
        SHAPE_RUN_POSITIONED,
        SHAPE_RUN_OUT_OF_OPS,
        SHAPE_RUN_MULTI_FACE,
        SHAPE_RUN_UNSUPPORTED,
    ];
    assert_eq!(run.iter().fold(0, |a, b| a | b).count_ones(), 8);
    let glyph = [
        SHAPE_GLYPH_NOTDEF,
        SHAPE_GLYPH_SUBSTITUTED,
        SHAPE_GLYPH_POSITIONED,
        SHAPE_GLYPH_IGNORABLE,
        SHAPE_GLYPH_LIGATURE,
        SHAPE_GLYPH_MARK,
        SHAPE_GLYPH_UNFILLED,
    ];
    assert_eq!(glyph.iter().fold(0, |a, b| a | b).count_ones(), 7);
}

/// A feature word is a packed tag and one bit, and the bit is free because an
/// OpenType tag is four ASCII bytes.
#[test]
fn a_feature_word_has_room_for_its_on_bit() {
    for tag in [b"test", b"kern", b"liga", b"ZZZZ", b"    "] {
        let packed = u32::from_be_bytes(*tag);
        assert_eq!(
            packed & SHAPE_FEATURE_ON,
            0,
            "{}",
            String::from_utf8_lossy(tag)
        );
        assert_ne!(packed, 0, "no valid tag is an empty slot");
    }
}

/// The checked-in Slang is the rendering of the declarations above, byte for
/// byte — the reason `ot.slang` and
/// `shape.slang` keep no hand-mirrored structs.
///
/// It is checked in rather than generated at build time because slangc resolves
/// an import from the importing file's own directory before any `-I`:
/// a consumer and the harness hand it the checked-out `slang/`, and a
/// file that only existed in `OUT_DIR` would not be there.
#[test]
fn the_checked_in_slang_is_the_rendering() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../slang/shape_abi.slang");
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
    let rendered = slang_module();
    if on_disk == rendered {
        return;
    }
    let line = on_disk
        .lines()
        .zip(rendered.lines())
        .position(|(a, b)| a != b)
        .map_or_else(
            || "the end".to_string(),
            |index| format!("line {}", index + 1),
        );
    panic!(
        "{} is not what the declarations render ({line} differs). \
         It is generated: run `{GEN_COMMAND}`.",
        path.display()
    );
}
