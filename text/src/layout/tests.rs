use super::*;
use core::mem::offset_of;

#[test]
fn point_layout() {
    assert_eq!(size_of::<Point>(), 16);
    assert_eq!(offset_of!(Point, p), 0);
    assert_eq!(offset_of!(Point, c), 8);
}

#[test]
fn curve_ref_is_just_a_word() {
    assert_eq!(size_of::<CurveRef>(), 4);
    assert_eq!(align_of::<CurveRef>(), 4);
}

#[test]
fn band_layout() {
    assert_eq!(size_of::<Band>(), 8);
    assert_eq!(offset_of!(Band, count), 0);
    assert_eq!(offset_of!(Band, first), 4);
}

#[test]
fn glyph_layout() {
    assert_eq!(size_of::<Glyph>(), 64);
    assert_eq!(align_of::<Glyph>(), 16);
    assert_eq!(offset_of!(Glyph, band_base), 0);
    assert_eq!(offset_of!(Glyph, hband_count), 4);
    assert_eq!(offset_of!(Glyph, vband_count), 8);
    assert_eq!(offset_of!(Glyph, flags), 12);
    assert_eq!(offset_of!(Glyph, band_transform), 16);
    assert_eq!(offset_of!(Glyph, bounds), 32);
    assert_eq!(offset_of!(Glyph, advance), 48);
    assert_eq!(offset_of!(Glyph, _rsvd0), 52);
    assert_eq!(offset_of!(Glyph, _rsvd1), 56);
    assert_eq!(offset_of!(Glyph, _rsvd2), 60);
}
