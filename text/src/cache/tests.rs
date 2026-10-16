use super::*;
use crate::outline::{Contour, Quad};

/// A square as four degenerate quadratics — the control point is the
/// midpoint, so nothing bulges and the bands are easy to reason about.
fn square() -> Contour {
    let corners = [[0.1, 0.1], [0.6, 0.1], [0.6, 0.7], [0.1, 0.7]];
    let mut quads = Vec::new();
    for i in 0..4 {
        let a = corners[i];
        let b = corners[(i + 1) % 4];
        quads.push(Quad {
            p0: a,
            p1: [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5],
            p2: b,
        });
    }
    Contour { quads }
}

fn sample() -> Cached {
    let mut builder = Builder::new();
    builder.push(&[square()], 0.7, false);
    builder.push(&[], 0.25, false);
    builder.push(&[square()], 0.7, true);
    Cached {
        curves: builder.finish(),
        units_per_em: 1000.0,
        glyph_count: 3,
        cap_height: 0.714,
    }
}

#[test]
fn a_cache_file_round_trips_every_buffer() {
    let before = sample();
    let bytes = before.to_bytes();
    let after = Cached::from_bytes(&bytes).expect("what we just wrote");
    assert_eq!(after.units_per_em, before.units_per_em);
    assert_eq!(after.glyph_count, before.glyph_count);
    assert_eq!(after.cap_height, before.cap_height);
    assert_eq!(after.curves.points, before.curves.points);
    assert_eq!(after.curves.bands, before.curves.bands);
    assert_eq!(after.curves.band_curves, before.curves.band_curves);
    assert_eq!(after.curves.glyphs, before.curves.glyphs);
    assert_eq!(after.curves.index, before.curves.index);
}

/// Every way a file can be wrong has the same answer: rebuild.
#[test]
fn a_file_this_build_does_not_recognise_is_a_miss() {
    let bytes = sample().to_bytes();
    assert!(Cached::from_bytes(&[]).is_none(), "empty");
    assert!(
        Cached::from_bytes(&bytes[..bytes.len() - 4]).is_none(),
        "truncated"
    );
    assert!(
        Cached::from_bytes(&bytes[..bytes.len() - 1]).is_none(),
        "not whole words"
    );

    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 0xff;
    assert!(Cached::from_bytes(&wrong_magic).is_none(), "magic");

    let mut wrong_version = bytes.clone();
    wrong_version[8..12].copy_from_slice(&(CACHE_VERSION + 1).to_le_bytes());
    assert!(Cached::from_bytes(&wrong_version).is_none(), "version");

    let mut wrong_counts = bytes.clone();
    wrong_counts[24..28].copy_from_slice(&9999u32.to_le_bytes());
    assert!(Cached::from_bytes(&wrong_counts).is_none(), "lengths");
}

/// The point of the file: a font out of the cache can still take more glyphs.
#[test]
fn a_resumed_builder_appends_after_the_cached_glyphs() {
    let cached = sample();
    let glyphs_before = cached.curves.glyphs.len() as u32;
    let points_before = cached.curves.points.len();
    let bands_before = cached.curves.bands.len() as u32;
    let mut builder = cached.into_builder();
    assert_eq!(builder.len(), glyphs_before);
    let added = builder.push(&[square()], 1.0, true);
    assert_eq!(
        added, glyphs_before,
        "an appended glyph is the next glyph id"
    );
    let set = builder.finish();
    assert!(set.points.len() > points_before);
    // And the appended glyph's records point *past* the cached ones, which
    // is the thing a naive concatenation would get wrong.
    let glyph = set.glyphs[added as usize];
    assert!(
        glyph.band_base >= bands_before,
        "bands were not rebased: {} < {bands_before}",
        glyph.band_base
    );
    assert!(glyph.hband_count > 0 && glyph.vband_count > 0);
}
