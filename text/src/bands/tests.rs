use super::*;

/// A unit square as one contour of four lines.
fn square() -> Contour {
    let corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let mut quads = Vec::new();
    for i in 0..4 {
        let a = corners[i];
        let b = corners[(i + 1) % 4];
        quads.push(Quad {
            p0: a,
            p1: b,
            p2: b,
        });
    }
    Contour { quads }
}

#[test]
fn a_contour_of_n_curves_costs_n_plus_one_points() {
    let mut builder = Builder::new();
    builder.push(&[square()], 1.0, false);
    let set = builder.finish();
    assert_eq!(set.points.len(), 5);
    // And the run is a ring: the tail repeats the head's point.
    assert_eq!(set.points[4].p, set.points[0].p);
    // Every curve's endpoint is the next entry's start.
    for i in 0..4 {
        assert_eq!(set.points[i].c, set.points[i + 1].p);
    }
}

#[test]
fn an_empty_glyph_has_no_bands() {
    let mut builder = Builder::new();
    let slot = builder.push(&[], 0.25, false);
    let set = builder.finish();
    assert_eq!(slot, 0);
    assert_eq!(set.glyphs[0].hband_count, 0);
    assert_eq!(set.glyphs[0].vband_count, 0);
    assert_eq!(set.glyphs[0].advance, 0.25);
    assert!(set.bands.is_empty());
}

/// The square's top and bottom edges are horizontal and its sides vertical,
/// so each direction may only ever see the two curves it can cross.
#[test]
fn axis_parallel_curves_are_excluded_from_their_own_axis() {
    let mut builder = Builder::new();
    builder.push(&[square()], 1.0, false);
    let set = builder.finish();
    let glyph = set.glyphs[0];
    for band in 0..glyph.hband_count {
        let record = set.bands[(glyph.band_base + band) as usize];
        assert!(
            record.count <= 2,
            "a horizontal band of the square listed {} curves",
            record.count
        );
    }
    for band in 0..glyph.vband_count {
        let record = set.bands[(glyph.band_base + glyph.hband_count + band) as usize];
        assert!(record.count <= 2);
    }
}

#[test]
fn band_lists_are_sorted_descending_by_their_key() {
    // A zigzag of quadratics with assorted extents, so the sort has work.
    let mut quads = Vec::new();
    let mut cursor = [0.0f32, 0.0];
    for i in 0..12 {
        let t = i as f32 / 12.0;
        let next = [0.3 + 0.7 * (t * 7.0).sin().abs(), t];
        quads.push(Quad {
            p0: cursor,
            p1: [
                (cursor[0] + next[0]) * 0.5 + 0.1,
                (cursor[1] + next[1]) * 0.5,
            ],
            p2: next,
        });
        cursor = next;
    }
    let contour = Contour { quads };
    let mut builder = Builder::new();
    builder.push(std::slice::from_ref(&contour), 1.0, false);
    let set = builder.finish();
    let glyph = set.glyphs[0];
    let by_slot: std::collections::HashMap<u32, Quad> = {
        // Rebuild the slot -> quad map the same way `push` laid it out.
        let mut map = std::collections::HashMap::new();
        for (j, quad) in contour.quads.iter().enumerate() {
            map.insert(j as u32, *quad);
        }
        map
    };
    for band in 0..glyph.hband_count {
        let record = set.bands[(glyph.band_base + band) as usize];
        let slots = &set.band_curves[record.first as usize..(record.first + record.count) as usize];
        let keys: Vec<f32> = slots.iter().map(|s| by_slot[&s.0].max_x()).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] >= pair[1], "horizontal band not sorted: {keys:?}");
        }
    }
    for band in 0..glyph.vband_count {
        let record = set.bands[(glyph.band_base + glyph.hband_count + band) as usize];
        let slots = &set.band_curves[record.first as usize..(record.first + record.count) as usize];
        let keys: Vec<f32> = slots.iter().map(|s| by_slot[&s.0].max_y()).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] >= pair[1], "vertical band not sorted: {keys:?}");
        }
    }
}

/// Every curve with extent on an axis must appear in at least one band of
/// that axis — a curve that falls through the grid is a missing stem.
#[test]
fn every_crossable_curve_lands_in_a_band() {
    let mut quads = Vec::new();
    let pts = [[0.1, 0.2], [0.9, 0.1], [0.8, 0.95], [0.2, 0.7]];
    for i in 0..4 {
        let a = pts[i];
        let b = pts[(i + 1) % 4];
        quads.push(Quad {
            p0: a,
            p1: [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5 + 0.05],
            p2: b,
        });
    }
    let mut builder = Builder::new();
    builder.push(&[Contour { quads }], 1.0, false);
    let set = builder.finish();
    let glyph = set.glyphs[0];
    let mut seen_h: std::collections::HashSet<CurveRef> = std::collections::HashSet::new();
    for band in 0..glyph.hband_count {
        let r = set.bands[(glyph.band_base + band) as usize];
        seen_h.extend(&set.band_curves[r.first as usize..(r.first + r.count) as usize]);
    }
    assert_eq!(seen_h.len(), 4, "a curve is in no horizontal band");
}

#[test]
fn the_band_transform_maps_a_sample_to_its_band() {
    let mut builder = Builder::new();
    builder.push(&[square()], 1.0, false);
    let set = builder.finish();
    let glyph = set.glyphs[0];
    let [sx, sy, ox, oy] = glyph.band_transform;
    // A sample at the bottom-left corner is band 0 in both directions; one
    // at the top-right is the last band in both.
    assert_eq!((0.0 * sx + ox) as i32, 0);
    assert_eq!((0.0 * sy + oy) as i32, 0);
    let top_x = ((1.0 * sx + ox) as i32).min(glyph.vband_count as i32 - 1);
    let top_y = ((1.0 * sy + oy) as i32).min(glyph.hband_count as i32 - 1);
    assert_eq!(top_x, glyph.vband_count as i32 - 1);
    assert_eq!(top_y, glyph.hband_count as i32 - 1);
}

#[test]
fn the_even_odd_flag_rides_on_the_glyph() {
    let mut builder = Builder::new();
    builder.push(&[square()], 1.0, true);
    let set = builder.finish();
    assert_eq!(set.glyphs[0].flags & FILL_EVEN_ODD, FILL_EVEN_ODD);
}

/// A curved contour, so the band search has something to sort and a rebase has
/// something to get wrong.
fn blob() -> Contour {
    Contour {
        quads: vec![
            Quad {
                p0: [0.1, 0.0],
                p1: [0.4, 0.2],
                p2: [0.7, 0.0],
            },
            Quad {
                p0: [0.7, 0.0],
                p1: [0.5, 0.5],
                p2: [0.4, 0.8],
            },
            Quad {
                p0: [0.4, 0.8],
                p1: [0.2, 0.4],
                p2: [0.1, 0.0],
            },
        ],
    }
}

/// A glyph appended from another set is byte-for-byte the glyph `push` would
/// have built — the property the fallback chain's per-face cache rests on, since what a cache
/// file holds is exactly these records and what it skips is the band search
/// that made them.
#[test]
fn a_prebuilt_glyph_is_rebased_onto_the_buffers_it_joins() {
    // The source: the glyph at slot 1 of a set of two, so its own bases are
    // non-zero and a rebase that forgets to subtract them cannot pass.
    let mut source = Builder::new();
    source.push(&[square()], 0.5, false);
    let slot = source.push(&[blob()], 0.7, true);
    let source = source.finish();

    // Two glyphs in the destination as well, for the same reason.
    let mut expected = Builder::new();
    expected.push(&[square()], 0.5, false);
    expected.push(&[blob()], 0.4, false);
    let mut appended = Builder::new();
    appended.push(&[square()], 0.5, false);
    appended.push(&[blob()], 0.4, false);

    let by_push = expected.push(&[blob()], 0.7, true);
    let by_copy = appended.push_from(&source, slot);
    let expected = expected.finish();
    let appended = appended.finish();

    assert_eq!(by_copy, by_push, "the same slot in the same buffers");
    assert_eq!(appended.glyphs, expected.glyphs, "the glyph records");
    assert_eq!(appended.points, expected.points, "the curve buffer");
    assert_eq!(appended.bands, expected.bands, "the band records");
    assert_eq!(appended.band_curves, expected.band_curves, "the locations");
    assert_eq!(appended.index, expected.index, "the CPU-side index");

    // And a slot the source does not have is a blank glyph, not a panic: a
    // cache file is derived data, but it is still a file.
    let mut more = Builder::resume(appended);
    let blank = more.push_from(&source, 99);
    let more = more.finish();
    assert_eq!(more.glyphs[blank as usize].hband_count, 0);
    assert_eq!(more.glyphs[blank as usize].advance, 0.0);
}

/// A record filled in place is reported once, sorted; appends never are, and
/// the list survives a `finish`/`resume` round trip, because a fallback merge
/// resumes a builder per fill.
#[test]
fn a_record_filled_in_place_is_reported_once_and_an_append_is_not() {
    let mut source = Builder::new();
    source.push(&[square()], 0.5, false);
    let slot = source.push(&[blob()], 0.7, true);
    let source = source.finish();

    let mut builder = Builder::new();
    assert_eq!(builder.reserve(10), 0);
    assert!(builder.fill_from(7, &source, slot));
    // A round trip between the two fills, as `punchcutter-fonts` makes one.
    let mut builder = Builder::resume(builder.finish());
    assert!(builder.fill_from(3, &source, slot));
    assert!(builder.fill_from(7, &source, 0));
    assert!(!builder.fill_from(10, &source, slot), "no slot 10 to fill");
    builder.push_from(&source, slot);
    builder.push(&[square()], 0.5, false);
    builder.reserve(2);

    assert_eq!(builder.take_rewritten(), vec![3, 7]);
    assert_eq!(builder.take_rewritten(), Vec::<u32>::new());

    // The same list, asked of the set instead of the builder.
    builder.fill_from(5, &source, slot);
    let mut set = builder.finish();
    assert_eq!(set.take_rewritten(), vec![5]);
    assert!(set.take_rewritten().is_empty());
}
