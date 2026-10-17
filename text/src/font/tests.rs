use super::*;

/// Every test here needs a real font, and a machine without one must still
/// be `cargo test` green.
fn sans() -> Option<Font> {
    let path = default_sans()?;
    match Font::load(&path, 0) {
        Ok(font) => Some(font),
        Err(err) => {
            eprintln!("skipping: the default sans would not load ({err:#})");
            None
        }
    }
}

#[test]
fn the_default_sans_becomes_curves() {
    let Some(font) = sans() else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    assert!(
        font.glyph_count > 100,
        "a system sans with {} glyphs?",
        font.glyph_count
    );
    assert_eq!(font.curves.glyphs.len(), font.glyph_count as usize);
    assert_eq!(font.curves.index.len(), font.glyph_count as usize);
    assert!(font.curves.points.len() > font.glyph_count as usize);
    // Nothing with ink in it should have defeated the outliner.
    assert_eq!(
        font.refused_glyphs, 0,
        "{} glyphs claim a bounding box and produced no contours",
        font.refused_glyphs
    );
}

/// The geometry must land inside the em square the coverage walk assumes:
/// ink within a couple of ems of the origin, never font units.
#[test]
fn the_curves_are_in_em_units() {
    let Some(font) = sans() else { return };
    for point in &font.curves.points {
        assert!(
            point.p[0].abs() < 4.0 && point.p[1].abs() < 4.0,
            "a control point at {:?} is not in em units",
            point.p
        );
    }
}

/// `g` at a text size is the glyph most likely to look wrong (two contours
/// and a descender), so it gets its own
/// assertions: two contours' worth of ink, an advance, bounds that straddle
/// the baseline (a descender), and a band grid that spans them.
#[test]
fn the_lowercase_g_has_ink_a_descender_and_bands() {
    let Some(font) = sans() else { return };
    let path = default_sans().expect("sans() succeeded");
    let data = std::fs::read(&path).expect("the file read once already");
    let face = ttf_parser::Face::parse(&data, 0).expect("parsed once already");
    let gid = face.glyph_index('g').expect("a sans-serif without a g");

    let glyph = font.curves.glyphs[gid.0 as usize];
    assert!(
        glyph.hband_count > 0 && glyph.vband_count > 0,
        "g has no bands"
    );
    assert!(glyph.advance > 0.2, "g's advance is {}", glyph.advance);
    assert!(
        glyph.bounds[1] < 0.0,
        "g's bounds do not descend: {:?}",
        glyph.bounds
    );
    assert!(
        glyph.bounds[3] > 0.3,
        "g has no x-height: {:?}",
        glyph.bounds
    );

    let index = font.curves.index[gid.0 as usize];
    assert!(index.band_curves.1 > 0);
    // Every curve the bands name must be a real entry of the curve buffer,
    // and `slot + 1` must be in range too, since the walk reads both.
    for slot in &font.curves.band_curves {
        assert!((slot.0 as usize) + 1 < font.curves.points.len());
    }
}

/// The cap height is what a consumer snaps its pixel sizes by, so it has to be an
/// em fraction and not font units, and it has to be there for every face.
#[test]
fn the_cap_height_is_an_em_fraction() {
    let Some(font) = sans() else { return };
    assert!(
        font.cap_height > 0.4 && font.cap_height < 1.0,
        "a cap height of {} em is not a cap height",
        font.cap_height
    );
}

/// A space has an advance and no ink: the one case the walk short-circuits.
#[test]
fn a_space_is_a_blank_glyph_with_an_advance() {
    let Some(font) = sans() else { return };
    let path = default_sans().expect("sans() succeeded");
    let data = std::fs::read(&path).expect("the file read once already");
    let face = ttf_parser::Face::parse(&data, 0).expect("parsed once already");
    let gid = face.glyph_index(' ').expect("a sans-serif without a space");
    let glyph = font.curves.glyphs[gid.0 as usize];
    assert_eq!(glyph.hband_count, 0);
    assert!(glyph.advance > 0.0);
    assert!(font.blank_glyphs > 0);
}

/// One codepoint out of an arbitrary face, which is what a fallback chain
/// asks: the curves in that face's own em units, the advance beside them, and `None` — never an error — for a face that has no glyph.
#[test]
fn a_face_answers_for_one_codepoint_or_says_it_has_none() {
    let Some(path) = default_sans() else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    let data = std::fs::read(&path).expect("the file fontconfig named");

    let a = codepoint_curves(&data, 0, 'A')
        .expect("a face that parses")
        .expect("a sans-serif without an A");
    assert!(a.has_ink(), "A has no contours");
    assert!(
        a.advance > 0.2 && a.advance < 2.0,
        "A advances {} em",
        a.advance
    );
    assert!(a.units_per_em >= 16.0, "unitsPerEm {}", a.units_per_em);
    for contour in &a.contours {
        for quad in &contour.quads {
            assert!(
                quad.p0[0].abs() < 4.0 && quad.p0[1].abs() < 4.0,
                "not em units"
            );
        }
    }

    // The space has a glyph and no ink — the case a chain walk must not
    // confuse with "this face cannot draw it".
    let space = codepoint_curves(&data, 0, ' ')
        .expect("a face that parses")
        .expect("a sans-serif without a space");
    assert!(!space.has_ink());

    // A private-use codepoint no text font claims.
    assert!(
        codepoint_curves(&data, 0, '\u{f8ff}')
            .expect("a face that parses")
            .is_none()
    );

    // The named-instance bits fontconfig packs into `%{index}` are not a face
    // index, and are masked off rather than refused.
    let instance = codepoint_curves(&data, 3 << 16, 'A').expect("the same face 0");
    assert_eq!(instance.map(|glyph| glyph.gid), Some(a.gid));

    // And the glyph this produces is the one the whole-font pass produced.
    let font = Font::parse(&data, 0).expect("the file parsed once already");
    let mut builder = Builder::new();
    builder.push(&a.contours, a.advance, false);
    let one = builder.finish();
    assert_eq!(
        one.glyphs[0].bounds,
        font.curves.glyphs[a.gid as usize].bounds
    );
    assert_eq!(
        one.glyphs[0].advance,
        font.curves.glyphs[a.gid as usize].advance
    );
}

/// Bytes that are not a font are an error, not a panic and not a `None`: a
/// chain walk must be able to tell "this face has no glyph for it" from "this
/// file is not a face", and only the first is worth walking past.
#[test]
fn bytes_that_are_not_a_face_are_an_error() {
    assert!(codepoint_curves(b"not a font at all", 0, 'A').is_err());
}

/// The by-id reader is the second half of the by-codepoint one: the same
/// glyph whichever door it came through, and `None` past the face's last id
/// rather than an error.
#[test]
fn a_glyph_by_id_is_the_codepoints_glyph_and_nothing_past_the_end() {
    let Some(path) = default_sans() else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    let data = std::fs::read(&path).expect("the file fontconfig named");
    let Some(by_char) = codepoint_curves(&data, 0, 'g').expect("a face") else {
        eprintln!("skipping: the default sans has no g");
        return;
    };
    let by_id = glyph_curves(&data, 0, by_char.gid)
        .expect("a face")
        .expect("an id it has");
    assert_eq!(by_id.gid, by_char.gid);
    assert_eq!(by_id.advance, by_char.advance);
    assert_eq!(by_id.contours.len(), by_char.contours.len());
    assert!(by_id.has_ink());

    let count = ttf_parser::Face::parse(&data, 0)
        .expect("parsed")
        .number_of_glyphs();
    assert!(
        glyph_curves(&data, 0, count).expect("a face").is_none(),
        "one past the last id"
    );
    assert!(glyph_curves(b"not a font", 0, 1).is_err());
}
