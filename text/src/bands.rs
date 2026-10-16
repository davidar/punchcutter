//! Slug's preprocessing: a glyph's curves sorted into horizontal and vertical
//! bands, which is the whole reason the per-pixel walk is short.
//!
//! The coverage at a sample is a winding number found by casting two rays, one
//! along +x and one along +y, and counting signed crossings. Testing every curve
//! in the glyph against every pixel would be quadratic; instead the glyph's ink
//! box is cut into horizontal strips and vertical strips, each strip lists the
//! curves that cross it, and a sample consults one strip of each. The lists are
//! sorted so the walk can stop early: once a curve's rightmost control point
//! lies left of the sample, no later curve in a horizontal band can matter.
//!
//! Three rules come from the Slug repository's own notes and all three are
//! obeyed here: the bands overlap by a small epsilon so a curve grazing a
//! boundary lands in both; horizontal curves stay out of horizontal bands and
//! vertical curves out of vertical bands, since a ray parallel to a curve
//! cannot cross it; and every band of one glyph has the same thickness, which is
//! what lets one multiply-add turn a position into a band index.

use crate::layout::{Band, CurveRef, FILL_EVEN_ODD, Glyph, Index, Point};
use crate::outline::{Contour, Quad};

/// Band overlap, em units — the Slug notes' suggested 1/1024 of an em. Without
/// it a curve whose extremum sits exactly on a band boundary can be missing
/// from the band the sample lands in, which shows up as a one-pixel notch.
const BAND_EPSILON: f32 = 1.0 / 1024.0;

/// Most bands in one direction for one glyph. Past this the records cost more
/// than the walk saves; Noto Sans's busiest glyphs settle well below it.
///
/// It is also, measured, where nearly all of the preprocessing time goes:
/// [`choose_bands`] tries every count from 1 to here on both axes, so it runs
/// `2 * (1 + 2 + ... + 16) = 272` assignments per glyph against one for the
/// outline. Noto Sans's 3877 glyphs take **198 ms** with the search and
/// **11 ms** with `MAX_BANDS = 1` — and the search earns it, taking the worst
/// band from 189 curves to 47. There are three ways out if 200 ms of startup
/// matters: preprocess the glyphs a string actually uses, cache the buffers
/// beside the font ([`crate::cache`], the one taken), or cut the search to a
/// few candidate counts. None of them changes the buffers' shape, so none of
/// them is this module's business.
const MAX_BANDS: u32 = 16;

/// Accumulates glyphs into the four buffers.
///
/// One builder holds a whole font, or a font and the fallback faces merged
/// after it, or a font and a caller's own outlines: the buffers are flat and a
/// glyph is an index, so nothing that can be drawn needs to live anywhere
/// else. [`CurveSet`] is what comes out.
#[derive(Debug, Default)]
pub struct Builder {
    points: Vec<Point>,
    bands: Vec<Band>,
    band_curves: Vec<CurveRef>,
    glyphs: Vec<Glyph>,
    index: Vec<Index>,
    /// Records [`Self::fill_from`] rewrote in place since the last
    /// [`Self::take_rewritten`], unsorted and possibly repeated.
    rewritten: Vec<u32>,
}

/// The buffers, plus the CPU-side per-glyph index.
#[derive(Debug, Default)]
pub struct CurveSet {
    pub points: Vec<Point>,
    pub bands: Vec<Band>,
    pub band_curves: Vec<CurveRef>,
    pub glyphs: Vec<Glyph>,
    pub index: Vec<Index>,
    /// Glyph records rewritten **in place** since someone last asked
    /// ([`Self::take_rewritten`]) — carried across [`Builder::resume`] and
    /// [`Builder::finish`], because a merge resumes a builder per fill.
    ///
    /// The other three buffers only ever append, and so does this one apart
    /// from [`Builder::fill_from`]: a copy of the buffers kept by a byte offset
    /// is whole again after its tail plus these records, and is stale without
    /// them.
    pub rewritten: Vec<u32>,
}

impl CurveSet {
    /// The records rewritten in place since the last call, sorted and without
    /// repeats; the list is empty afterwards.
    pub fn take_rewritten(&mut self) -> Vec<u32> {
        sorted(std::mem::take(&mut self.rewritten))
    }

    /// Bytes the four buffers occupy on the GPU.
    pub fn bytes(&self) -> usize {
        size_of_val(self.points.as_slice())
            + size_of_val(self.bands.as_slice())
            + size_of_val(self.band_curves.as_slice())
            + size_of_val(self.glyphs.as_slice())
    }

    /// Bytes each buffer occupies, in the binding order `text.slang` declares:
    /// points, bands, band curves, glyphs.
    pub fn buffer_bytes(&self) -> [usize; 4] {
        [
            size_of_val(self.points.as_slice()),
            size_of_val(self.bands.as_slice()),
            size_of_val(self.band_curves.as_slice()),
            size_of_val(self.glyphs.as_slice()),
        ]
    }

    /// The largest number of curves any single band lists — the walk's worst
    /// case, and the number the band-count search minimises.
    pub fn worst_band(&self) -> u32 {
        self.bands.iter().map(|band| band.count).max().unwrap_or(0)
    }
}

impl Builder {
    pub fn new() -> Self {
        Builder::default()
    }

    /// A builder that will append to buffers that already exist — a font read
    /// back from [`crate::cache`], ready to take more glyphs after its own.
    ///
    /// The inverse of [`Self::finish`], and the reason [`Self::push`] rebases
    /// every record it writes against the buffers' current lengths rather than
    /// against zero: there is nothing in a [`CurveSet`] that remembers where it
    /// came from, so resuming is just owning the vectors again.
    pub fn resume(set: CurveSet) -> Self {
        Builder {
            points: set.points,
            bands: set.bands,
            band_curves: set.band_curves,
            glyphs: set.glyphs,
            index: set.index,
            rewritten: set.rewritten,
        }
    }

    /// How many glyphs have been pushed — the index the next [`Self::push`]
    /// returns.
    pub fn len(&self) -> u32 {
        self.glyphs.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    pub fn finish(self) -> CurveSet {
        CurveSet {
            points: self.points,
            bands: self.bands,
            band_curves: self.band_curves,
            glyphs: self.glyphs,
            index: self.index,
            rewritten: self.rewritten,
        }
    }

    /// The records [`Self::fill_from`] rewrote in place since the last call,
    /// sorted and without repeats. An append ([`Self::push`],
    /// [`Self::push_from`], [`Self::reserve`]) is never reported: it is past
    /// whatever a copy of the buffers already holds.
    pub fn take_rewritten(&mut self) -> Vec<u32> {
        sorted(std::mem::take(&mut self.rewritten))
    }

    /// Append a glyph **already built** — its records copied out of another
    /// set and rebased onto these buffers (the fallback chain's path).
    ///
    /// The band search is what a cache is for: nearly all of the preprocessing
    /// time is [`choose_bands`] (the note beside `MAX_BANDS`), and the records
    /// it produces are position-independent apart from three indices — a
    /// glyph's `band_base` into the band records, a band's `first` into the
    /// curve-location list, and a location into the point buffer. So a cached
    /// glyph is three rebases and four `extend_from_slice`s, and it produces
    /// byte-for-byte what [`Self::push`] would have.
    ///
    /// `slot` is the glyph's index in `set`; a slot `set` does not have is a
    /// blank glyph, the same answer [`Self::push`] gives for empty contours.
    pub fn push_from(&mut self, set: &CurveSet, slot: u32) -> u32 {
        self.place_from(None, set, slot)
    }

    /// Append `count` blank glyph records, and answer the first one's index.
    ///
    /// The shaping arena gives a fallback face its **whole** glyph range in
    /// the merged space the moment it is opened, so that the face's own `GSUB`
    /// and `GPOS` index the merged ids by one addition. The records in that
    /// range are blanks until a cover fills them with [`Self::fill_from`], and
    /// a blank costs one 64-byte `Glyph` (on the GPU too) and one 44-byte `Index`, which is why
    /// opening Droid Sans Fallback's 50k glyphs is affordable at all.
    pub fn reserve(&mut self, count: u32) -> u32 {
        let first = self.glyphs.len() as u32;
        for _ in 0..count {
            self.push(&[], 0.0, false);
        }
        first
    }

    /// [`Self::push_from`] into a slot that already exists — one of
    /// [`Self::reserve`]'s blanks.
    ///
    /// The point, band and curve buffers still *append*: they are never
    /// rewritten, so a GPU copy of them grows by a tail and the record at
    /// `at` is the only thing that moves. `false` for a slot this builder does
    /// not have.
    pub fn fill_from(&mut self, at: u32, set: &CurveSet, slot: u32) -> bool {
        if at as usize >= self.glyphs.len() {
            return false;
        }
        self.place_from(Some(at), set, slot);
        true
    }

    /// The shared body: `at` is `None` to append and `Some(slot)` to overwrite.
    fn place_from(&mut self, at: Option<u32>, set: &CurveSet, slot: u32) -> u32 {
        let glyph_index = at.unwrap_or(self.glyphs.len() as u32);
        let (Some(src), Some(src_index)) =
            (set.glyphs.get(slot as usize), set.index.get(slot as usize))
        else {
            return match at {
                None => self.push(&[], 0.0, false),
                Some(_) => glyph_index,
            };
        };

        let point_first = self.points.len() as u32;
        let band_first = self.bands.len() as u32;
        let curve_first = self.band_curves.len() as u32;
        let (p0, pn) = src_index.points;
        let (b0, bn) = src_index.bands;
        let (c0, cn) = src_index.band_curves;

        // Rebasing is `- source base + our base` in every case, and every
        // source index is at or past its own base by construction — but this
        // takes bytes off a cache file, so it is spelled to be total: an index
        // that underflows lands on our base rather than at 4 billion.
        let rebase = |value: u32, from: u32, to: u32| value.saturating_sub(from).saturating_add(to);

        self.points
            .extend_from_slice(&set.points[range(p0, pn, set.points.len())]);
        for location in &set.band_curves[range(c0, cn, set.band_curves.len())] {
            self.band_curves
                .push(CurveRef(rebase(location.0, p0, point_first)));
        }
        for band in &set.bands[range(b0, bn, set.bands.len())] {
            self.bands.push(Band {
                count: band.count,
                first: rebase(band.first, c0, curve_first),
            });
        }

        let mut glyph = *src;
        glyph.band_base = band_first;
        let index = Index {
            points: (point_first, self.points.len() as u32 - point_first),
            bands: (band_first, self.bands.len() as u32 - band_first),
            band_curves: (curve_first, self.band_curves.len() as u32 - curve_first),
            advance: src_index.advance,
            bounds: src_index.bounds,
        };
        match at {
            None => {
                self.glyphs.push(glyph);
                self.index.push(index);
            }
            Some(slot) => {
                self.glyphs[slot as usize] = glyph;
                self.index[slot as usize] = index;
                self.rewritten.push(slot);
            }
        }
        glyph_index
    }

    /// Add one glyph: its contours, its advance in em units, and which fill rule
    /// its paths were drawn with. Returns the glyph's index.
    ///
    /// Contours with no curves — a space, an empty `glyf` entry, a path with
    /// nothing filled — are pushed as a glyph with zero bands, which the walk
    /// reads as zero coverage without touching a buffer.
    pub fn push(&mut self, contours: &[Contour], advance: f32, even_odd: bool) -> u32 {
        let glyph_index = self.glyphs.len() as u32;
        let point_first = self.points.len() as u32;
        let band_first = self.bands.len() as u32;
        let curve_first = self.band_curves.len() as u32;

        // Lay the contours into the curve buffer. Each contour of `n` curves
        // takes `n + 1` entries because neighbours share an endpoint, and curve
        // `j` of the contour is addressed by the entry it starts at.
        let mut curves: Vec<(u32, Quad)> = Vec::new();
        for contour in contours {
            if contour.quads.is_empty() {
                continue;
            }
            let base = self.points.len() as u32;
            for (j, quad) in contour.quads.iter().enumerate() {
                self.points.push(Point {
                    p: quad.p0,
                    c: quad.p1,
                });
                curves.push((base + j as u32, *quad));
            }
            // The run's tail carries the last curve's endpoint, which is also
            // the contour's first point. Its control field is never read; it is
            // written as the same point so a buffer dump is readable.
            let last = contour.quads.last().expect("checked non-empty");
            self.points.push(Point {
                p: last.p2,
                c: last.p2,
            });
        }

        if curves.is_empty() {
            self.glyphs.push(Glyph {
                advance,
                ..Glyph::default()
            });
            self.index.push(Index {
                points: (point_first, self.points.len() as u32 - point_first),
                bands: (band_first, 0),
                band_curves: (curve_first, 0),
                advance,
                bounds: [0.0; 4],
            });
            return glyph_index;
        }

        let bounds = ink_bounds(&curves);

        // Horizontal bands split along y and are consulted by a sample's y;
        // vertical bands split along x. Each direction picks its own count.
        let (hband_count, h_lists) = choose_bands(&curves, Axis::Horizontal, bounds);
        let (vband_count, v_lists) = choose_bands(&curves, Axis::Vertical, bounds);

        let band_base = self.bands.len() as u32;
        for list in h_lists.iter().chain(v_lists.iter()) {
            let first = self.band_curves.len() as u32;
            self.band_curves.extend_from_slice(list);
            self.bands.push(Band {
                count: list.len() as u32,
                first,
            });
        }

        let (min_x, min_y, max_x, max_y) = (bounds[0], bounds[1], bounds[2], bounds[3]);
        let band_w = (max_x - min_x) / vband_count as f32;
        let band_h = (max_y - min_y) / hband_count as f32;
        let scale_x = if band_w > 0.0 { 1.0 / band_w } else { 0.0 };
        let scale_y = if band_h > 0.0 { 1.0 / band_h } else { 0.0 };

        self.glyphs.push(Glyph {
            band_base,
            hband_count,
            vband_count,
            flags: if even_odd { FILL_EVEN_ODD } else { 0 },
            band_transform: [scale_x, scale_y, -min_x * scale_x, -min_y * scale_y],
            bounds,
            advance,
            _rsvd0: 0,
            _rsvd1: 0,
            _rsvd2: 0,
        });
        self.index.push(Index {
            points: (point_first, self.points.len() as u32 - point_first),
            bands: (band_base, hband_count + vband_count),
            band_curves: (curve_first, self.band_curves.len() as u32 - curve_first),
            advance,
            bounds,
        });
        glyph_index
    }
}

fn sorted(mut indices: Vec<u32>) -> Vec<u32> {
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// One record range of a source buffer, clamped to what the buffer has — a
/// cache file is derived data this process wrote, but it is still a file.
fn range(first: u32, count: u32, len: usize) -> std::ops::Range<usize> {
    let start = (first as usize).min(len);
    let end = start.saturating_add(count as usize).min(len);
    start..end
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    /// Strips cut along y, consulted by a sample's y, walked by a +x ray.
    Horizontal,
    /// Strips cut along x, consulted by a sample's x, walked by a +y ray.
    Vertical,
}

/// The box containing every control point, which therefore contains every
/// curve: a quadratic lies inside the convex hull of its three control points.
fn ink_bounds(curves: &[(u32, Quad)]) -> [f32; 4] {
    let mut bounds = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for (_, quad) in curves {
        bounds[0] = bounds[0].min(quad.min_x());
        bounds[1] = bounds[1].min(quad.min_y());
        bounds[2] = bounds[2].max(quad.max_x());
        bounds[3] = bounds[3].max(quad.max_y());
    }
    bounds
}

/// The band count for one axis, and the lists it produces.
///
/// The Slug notes say to pick the count that minimises the largest number of
/// curves in any one band. Taken literally that always wants the maximum, since
/// more bands never means more curves in the worst one; so the count minimises
/// the worst band *plus* a thirty-second of the total list length, which buys a
/// band only when it actually removes work from the worst case.
fn choose_bands(curves: &[(u32, Quad)], axis: Axis, bounds: [f32; 4]) -> (u32, Vec<Vec<CurveRef>>) {
    let extent = match axis {
        Axis::Horizontal => bounds[3] - bounds[1],
        Axis::Vertical => bounds[2] - bounds[0],
    };
    // A glyph with no extent along this axis — an underscore's vertical extent,
    // say — gets one band and the assignment below clamps everything into it.
    // `is_finite` is not decoration: a coordinate the font got wrong would make
    // the band thickness a NaN and every index it produced zero.
    if !extent.is_finite() || extent <= 0.0 {
        return (1, vec![assign(curves, axis, bounds, 1, 0)]);
    }

    let mut best: Option<(f32, u32, Vec<Vec<CurveRef>>)> = None;
    for count in 1..=MAX_BANDS {
        let lists: Vec<Vec<CurveRef>> = (0..count)
            .map(|band| assign(curves, axis, bounds, count, band))
            .collect();
        let worst = lists.iter().map(Vec::len).max().unwrap_or(0) as f32;
        let total = lists.iter().map(Vec::len).sum::<usize>() as f32;
        let cost = worst + total / 32.0;
        if best.as_ref().is_none_or(|(seen, _, _)| cost < *seen) {
            best = Some((cost, count, lists));
        }
    }
    let (_, count, lists) = best.expect("the loop runs at least once");
    (count, lists)
}

/// The curves crossing one band, sorted the way the walk's early-out needs.
fn assign(
    curves: &[(u32, Quad)],
    axis: Axis,
    bounds: [f32; 4],
    count: u32,
    band: u32,
) -> Vec<CurveRef> {
    let (lo_bound, hi_bound) = match axis {
        Axis::Horizontal => (bounds[1], bounds[3]),
        Axis::Vertical => (bounds[0], bounds[2]),
    };
    let thickness = (hi_bound - lo_bound) / count as f32;
    let lo = lo_bound + thickness * band as f32 - BAND_EPSILON;
    let hi = lo_bound + thickness * (band + 1) as f32 + BAND_EPSILON;

    let mut chosen: Vec<(f32, u32)> = Vec::new();
    for (slot, quad) in curves {
        let (curve_lo, curve_hi, key, skip) = match axis {
            // A horizontal band is crossed by curves spanning its y range, and
            // the walk sorts them by how far right they reach.
            Axis::Horizontal => (
                quad.min_y(),
                quad.max_y(),
                quad.max_x(),
                quad.is_horizontal(),
            ),
            Axis::Vertical => (quad.min_x(), quad.max_x(), quad.max_y(), quad.is_vertical()),
        };
        if skip || curve_hi < lo || curve_lo > hi {
            continue;
        }
        chosen.push((key, *slot));
    }
    // Descending by the key, so the walk may stop at the first curve that
    // cannot reach the sample. Slot order breaks ties, to keep the output of a
    // build reproducible.
    chosen.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    chosen.into_iter().map(|(_, slot)| CurveRef(slot)).collect()
}

#[cfg(test)]
mod tests;
