use super::*;
use core::mem::offset_of;

#[test]
fn placement_layout() {
    assert_eq!(size_of::<Placement>(), 16);
    assert_eq!(offset_of!(Placement, glyph), 0);
    assert_eq!(offset_of!(Placement, px_per_em), 4);
    assert_eq!(offset_of!(Placement, origin), 8);
}

#[test]
fn params_layout() {
    assert_eq!(size_of::<Params>(), 16);
    assert_eq!(offset_of!(Params, width), 0);
    assert_eq!(offset_of!(Params, height), 4);
    assert_eq!(offset_of!(Params, count), 8);
    assert_eq!(offset_of!(Params, _rsvd0), 12);
}
