use super::*;
use crate::abi::{OT_FACE_ALL_FILLED, OtFace, OtHeader};

/// The two fonts the harness uses, reached the same way every test in this
/// crate reaches them: a missing file is a skip, never a failure.
const ADWAITA_SANS: &str = "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf";
const NOTO_SANS: &str = "/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf";

fn arena_of(path: &str) -> Option<Arena> {
    if !Path::new(path).exists() {
        eprintln!("skipping: {path} is not installed");
        return None;
    }
    Some(Arena::load(path).unwrap_or_else(|err| panic!("{path}: {err}")))
}

fn header(arena: &Arena) -> OtHeader {
    let mut header = OtHeader::default();
    assert!(punchcutter_core::plain::write_from_bytes(
        &mut header,
        &arena.bytes()[..size_of::<OtHeader>()]
    ));
    header
}

fn record(arena: &Arena, index: usize) -> OtFace {
    let at = size_of::<OtHeader>() + index * size_of::<OtFace>();
    let mut face = OtFace::default();
    assert!(punchcutter_core::plain::write_from_bytes(
        &mut face,
        &arena.bytes()[at..at + size_of::<OtFace>()]
    ));
    face
}

#[test]
fn an_empty_arena_is_the_front_block_and_nothing_else() {
    let arena = Arena::new();
    assert_eq!(arena.bytes().len(), OT_TABLES_START as usize);
    let header = header(&arena);
    assert_eq!(header.version, SHAPE_ABI_VERSION);
    assert_eq!(header.face_count, 0);
    assert_eq!(header.arena_bytes, OT_TABLES_START as u32);
    assert!(arena.faces().is_empty());
}

#[test]
fn the_primary_lands_behind_the_directory_with_its_tables_verbatim() {
    let Some(arena) = arena_of(ADWAITA_SANS) else {
        return;
    };
    let data = std::fs::read(ADWAITA_SANS).expect("read the font again");
    let raw = RawFace::parse(&data, 0).expect("a font this machine ships");

    assert_eq!(arena.faces().len(), 1);
    let info = arena.faces()[0];
    assert_eq!(info.glyph_base, 0);
    assert!(info.glyph_count > 0);
    assert!(info.upem > 0);
    assert!(info.all_filled);
    assert_eq!(info.filled_off, 0);
    assert_eq!(header(&arena).face_count, 1);
    assert_eq!(record(&arena, 0).flags, OT_FACE_ALL_FILLED);

    // Every table this side sliced is byte-for-byte what the file holds, at a
    // 4-aligned offset past the front block. That is the whole contract.
    for (slot, tag) in TABLES.iter().enumerate() {
        let (offset, len) = info.tables[slot];
        let Some(table) = raw.table(Tag::from_bytes(tag)) else {
            assert_eq!(len, 0, "{:?} is absent but sliced", tag);
            continue;
        };
        assert_eq!(len as usize, table.len());
        assert!(offset as usize >= OT_TABLES_START as usize);
        assert_eq!(offset % 4, 0);
        assert_eq!(
            &arena.bytes()[offset as usize..offset as usize + table.len()],
            table
        );
    }
    eprintln!(
        "AdwaitaSans-Regular: {} glyphs at {} upem, {} — {} KiB of arena",
        info.glyph_count,
        info.upem,
        info.table_sizes(),
        arena.bytes().len().div_ceil(1024),
    );
}

#[test]
fn a_second_face_takes_the_range_after_the_first() {
    let (Some(mut arena), true) = (arena_of(ADWAITA_SANS), Path::new(NOTO_SANS).exists()) else {
        return;
    };
    let first = arena.faces()[0];
    let noto = std::fs::read(NOTO_SANS).expect("read the fallback");
    let index = arena.add_face(&noto, 0, false).expect("a second face fits");

    assert_eq!(index, 1);
    let second = arena.faces()[1];
    assert_eq!(second.glyph_base, first.glyph_count);
    assert_eq!(arena.watermark(), first.glyph_count + second.glyph_count);
    assert!(!second.all_filled);

    // The fill bitmap: one bit a glyph, all clear until a cover sets one.
    assert_eq!(second.bitmap_bytes(), second.glyph_count.div_ceil(32) * 4);
    assert!(!arena.is_filled(1, 7));
    arena.mark_filled(1, 7);
    assert!(arena.is_filled(1, 7));
    assert!(!arena.is_filled(1, 8));
    // The primary answers without reading anything.
    assert!(arena.is_filled(0, 12345));
    // A glyph past the face's range is neither filled nor a panic.
    assert!(!arena.is_filled(1, second.glyph_count + 1));
    arena.mark_filled(1, second.glyph_count + 1);
}

#[test]
fn reserved_ids_push_the_watermark_past_the_faces() {
    let Some(mut arena) = arena_of(ADWAITA_SANS) else {
        return;
    };
    let glyphs = arena.faces()[0].glyph_count;
    let base = arena.reserve_ids(24);
    assert_eq!(base, glyphs);
    assert_eq!(arena.watermark(), glyphs + 24);
    assert_eq!(arena.font().glyph_count, glyphs + 24);
}

#[test]
fn the_directory_fills_up_rather_than_overflowing() {
    let Some(mut arena) = arena_of(ADWAITA_SANS) else {
        return;
    };
    let data = std::fs::read(ADWAITA_SANS).expect("read the font again");
    while arena.faces().len() < MAX_OT_FACES as usize {
        arena
            .add_face(&data, 0, false)
            .expect("room in the directory");
    }
    assert!(matches!(
        arena.add_face(&data, 0, false),
        Err(FontError::Full)
    ));
    assert_eq!(header(&arena).face_count as usize, MAX_OT_FACES as usize);
}

#[test]
fn a_file_that_is_not_a_font_is_an_error_and_not_a_panic() {
    let mut arena = Arena::new();
    assert!(arena.add_face(b"not a font at all", 0, true).is_err());
    assert!(arena.faces().is_empty());
}
