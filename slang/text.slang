// Glyph coverage from Bézier outlines: the Slug algorithm, in Slang.
//
// ---------------------------------------------------------------------------
// PROVENANCE
//
// The algorithm is Slug: Eric Lengyel, "GPU-Centered Font Rendering Directly
// from Glyph Outlines", Journal of Computer Graphics Techniques 6(2), 2017
// (https://jcgt.org/published/0006/02/02/). The Slug patent (US 10,373,352) was
// permanently and irrevocably dedicated to the public domain, so the algorithm
// may be implemented freely.
//
// The coverage walk below — `textRootCode`, `textSolveHoriz`, `textSolveVert`,
// `textCombine`, and the two band loops inside `textCoverageAniso` — is a port
// of the reference pixel shader published at https://github.com/EricLengyel/Slug
// under "MIT OR Apache-2.0". That repository asks that distributed use give
// credit, which this header is:
//
//     Slug shader code Copyright 2017 by Eric Lengyel.
//     SPDX-License-Identifier: MIT OR Apache-2.0
// Changed from the reference: the resource layout (`punchcutter-text`'s four storage
// buffers in place of the reference's two 4096-wide textures, which deletes the
// reference's `CalcBandLoc` wrap arithmetic entirely), the preprocessing that
// fills them (the reference publishes none — see `text/src/bands.rs`), the
// per-glyph constants as a buffer indexed by glyph rather than as two packed
// vertex attributes, the pixels-per-em as an argument rather than `fwidth` of an
// interpolated texcoord, and the dilation, which is a rect here rather than a
// displaced quad. Everything else follows the reference line for line, including
// the `0x2E74` root-eligibility table, whose derivation is in the paper.
// ---------------------------------------------------------------------------
//
// WHAT IT IS FOR
//
// An atlas text path rasterises every glyph at every size into an atlas, and
// pays for that choice with a packer, an eviction policy, and a re-raster on
// every scale change. Coverage straight from the outlines has none of those: a
// font reaches the GPU once as curves, a glyph is an index, and a size is a
// multiply. Resolution independence is the point — a fractional-scale output and
// a 2x output read the same buffers.
//
// THE SHAPE OF A GLYPH'S DATA
//
// `textPoints` holds on-curve/control point pairs in em units, y up, origin at
// the pen. Quadratic curve `i` is `{points[i].p, points[i].c, points[i + 1].p}`,
// so a contour's `n` curves cost `n + 1` entries and neighbours share endpoints.
//
// `textBands` holds per-band `(count, first)` records: a glyph's horizontal
// records first, then its vertical ones. `textBandCurves` holds the curve
// indices each record names, sorted descending by the curve's maximum x (for
// horizontal bands) or y (for vertical), which is what the early `break` rests
// on. `textGlyphs` holds the per-glyph constants.
//
// WHAT IT DOES NOT DO
//
// No shaping — a glyph index and a position come from elsewhere
// (`slang/shape.slang`). No strokes: a stroked path must be expanded to a fill
// before it reaches the buffers. No colour: this returns coverage, and the
// caller owns the paint. No hinting, which is Slug's own limitation; the paper's
// remedy is to choose a size at which `sCapHeight` lands on a whole pixel.

module text;

// ---------------------------------------------------------------------------
// The buffers, mirrored by `text/src/layout.rs` and pinned by its reflection
// test. Set 1, so an importer keeps its own set 0 exactly as it is and adds
// one bind group. Binding numbers are literals because a `[[vk::binding]]`
// argument cannot be an expression.
// ---------------------------------------------------------------------------

/// One entry of the curve buffer: an on-curve point and the control point
/// following it. See the packing note above.
public struct TextPoint
{
    public float2 p;
    public float2 c;
};

/// One band: how many curves cross it, and where their indices begin.
public struct TextBand
{
    public uint count;
    public uint first;
};

/// Everything the walk needs about one glyph. `band_transform` is
/// `(1/band width, 1/band height, -min x/band width, -min y/band height)`, which
/// turns a sample position into a band index with one multiply-add; `bounds` is
/// the ink box over the control points, em units, `(min x, min y, max x, max y)`.
public struct TextGlyph
{
    public uint band_base;
    public uint hband_count;
    public uint vband_count;
    public uint flags;
    public float4 band_transform;
    public float4 bounds;
    public float advance;
    public uint _rsvd0;
    public uint _rsvd1;
    public uint _rsvd2;
};

/// `TextGlyph.flags`: fill with the even-odd rule rather than non-zero. Fonts
/// are non-zero; a caller's own outline may be either.
public static const uint TEXT_FILL_EVEN_ODD = 1u;

[[vk::binding(0, 1)]]
public StructuredBuffer<TextPoint> textPoints;

[[vk::binding(1, 1)]]
public StructuredBuffer<TextBand> textBands;

[[vk::binding(2, 1)]]
public StructuredBuffer<uint> textBandCurves;

[[vk::binding(3, 1)]]
public StructuredBuffer<TextGlyph> textGlyphs;

// ---------------------------------------------------------------------------
// Every index into those four buffers is bounded by the buffer, and this is the
// one place that is decided.
//
// A glyph index arrives here from somewhere else — from a run a compute pass
// wrote, or in `punchcutter-text`'s harness from a host array — and the
// importer cannot always promise it is a record this buffer holds. A consumer
// may size its curve buffers with *unwritten* headroom so that glyphs merged
// later do not rebuild them, and then "inside the buffer" and "a glyph that
// exists" are not the same question: a record read out of the headroom has a
// garbage `band_base` whose band and curve indices are garbage in turn. So
// this module bounds every read itself.
//
// **What this is not.** It is not what stops an out-of-range read from being a
// wild pointer, because nothing here ever was one: wgpu's Vulkan backend turns
// on `robustBufferAccess` (and `robustBufferAccess2` where the device has it)
// on every device it creates, so an out-of-range *storage buffer* read is
// bounds-checked by the driver and returns zero — measured both ways on
// lavapipe, which returns a blank glyph for id 0xFFFFFFFF with the bound in
// place and without it. What these bounds buy is a defined answer that does not rest
// on a driver feature: a blank glyph, in this module's own terms, whatever the
// backend does.
//
// The cost is one `GetDimensions` and one compare per glyph, and two more per
// band: not per curve, and not in the inner loop.
// ---------------------------------------------------------------------------

/// Elements in a buffer, the one spelling of the idiom.
uint textCountOf<T>(StructuredBuffer<T> buffer)
{
    uint count;
    uint stride;
    buffer.GetDimensions(count, stride);
    return count;
}

/// A glyph that draws nothing: no bands, so every walk below returns on its
/// first test and no other buffer is touched.
TextGlyph textGlyphBlank()
{
    TextGlyph g;
    g.band_base = 0u;
    g.hband_count = 0u;
    g.vband_count = 0u;
    g.flags = 0u;
    g.band_transform = float4(0.0, 0.0, 0.0, 0.0);
    g.bounds = float4(0.0, 0.0, 0.0, 0.0);
    g.advance = 0.0;
    g._rsvd0 = 0u;
    g._rsvd1 = 0u;
    g._rsvd2 = 0u;
    return g;
}

/// Glyph `glyphIndex`, or a blank one when the buffer has no such record.
///
/// Public because a caller that wants to *know* — to report a missing glyph,
/// say — can ask [`textGlyphCount`] instead of inferring it from an empty rect.
public TextGlyph textGlyphAt(uint glyphIndex)
{
    return glyphIndex < textCountOf(textGlyphs) ? textGlyphs[glyphIndex]
                                                : textGlyphBlank();
}

/// Glyph records the buffer holds. Its *capacity*, which an importer with
/// headroom should compare against its own written count rather than trust.
public uint textGlyphCount()
{
    return textCountOf(textGlyphs);
}

// ---------------------------------------------------------------------------
// The root eligibility table, from the reference shader.
// ---------------------------------------------------------------------------

/// Which of a sample-relative quadratic's two roots can contribute to the
/// winding number, from the signs of the three control points' y coordinates
/// alone — bit 0 for the first root, bit 8 for the second.
///
/// The three sign bits index a sixteen-entry table of two-bit answers, which is
/// the `0x2E74` literal; the shifts gather the signs into a 3-bit index without
/// a branch. Verbatim from the reference, whose derivation is in the paper.
uint textRootCode(float y1, float y2, float y3)
{
    uint i1 = asuint(y1) >> 31u;
    uint i2 = asuint(y2) >> 30u;
    uint i3 = asuint(y3) >> 29u;

    uint shift = (i2 & 2u) | (i1 & ~2u);
    shift = (i3 & 4u) | (shift & ~4u);

    return ((0x2E74u >> shift) & 0x0101u);
}

/// The x coordinates where a sample-relative curve crosses `y = 0`.
///
/// In `t` the curve's y is `a t^2 - 2b t + c` with `a = p1 - 2 p2 + p3`,
/// `b = p1 - p2`, `c = p1`. The discriminant is clamped at zero, so imaginary
/// roots become a double root at the minimum; a nearly linear polynomial takes
/// the linear solution instead.
float2 textSolveHoriz(float2 p1, float2 p2, float2 p3)
{
    float2 a = p1 - p2 * 2.0 + p3;
    float2 b = p1 - p2;
    float ra = 1.0 / a.y;
    float rb = 0.5 / b.y;

    float d = sqrt(max(b.y * b.y - a.y * p1.y, 0.0));
    float t1 = (b.y - d) * ra;
    float t2 = (b.y + d) * ra;

    if (abs(a.y) < 1.0 / 65536.0)
    {
        t1 = p1.y * rb;
        t2 = t1;
    }

    return float2((a.x * t1 - b.x * 2.0) * t1 + p1.x, (a.x * t2 - b.x * 2.0) * t2 + p1.x);
}

/// The y coordinates where a sample-relative curve crosses `x = 0`.
float2 textSolveVert(float2 p1, float2 p2, float2 p3)
{
    float2 a = p1 - p2 * 2.0 + p3;
    float2 b = p1 - p2;
    float ra = 1.0 / a.x;
    float rb = 0.5 / b.x;

    float d = sqrt(max(b.x * b.x - a.x * p1.x, 0.0));
    float t1 = (b.x - d) * ra;
    float t2 = (b.x + d) * ra;

    if (abs(a.x) < 1.0 / 65536.0)
    {
        t1 = p1.x * rb;
        t2 = t1;
    }

    return float2((a.y * t1 - b.y * 2.0) * t1 + p1.y, (a.y * t2 - b.y * 2.0) * t2 + p1.y);
}

/// The two rays' coverages, weighted and combined.
///
/// The weights say how close a crossing fell to the sample, so a ray that
/// grazes an edge counts for less than one that crosses it squarely. The
/// `min(abs(xcov), abs(ycov))` term is what keeps a stem thinner than a pixel
/// visible: neither ray alone resolves it, but both find *some* crossing, and
/// the smaller of the two is an honest floor on the ink present.
float textCombine(float xcov, float ycov, float xwgt, float ywgt, uint flags)
{
    float coverage = max(
        abs(xcov * xwgt + ycov * ywgt) / max(xwgt + ywgt, 1.0 / 65536.0),
        min(abs(xcov), abs(ycov)));

    if ((flags & TEXT_FILL_EVEN_ODD) == 0u)
    {
        coverage = saturate(coverage);
    }
    else
    {
        coverage = 1.0 - abs(1.0 - frac(coverage * 0.5) * 2.0);
    }

    return coverage;
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Coverage of glyph `glyphIndex` at sample `p`, with separate pixels-per-em for
/// x and y.
///
/// `p` is in em units with y up and the origin at the glyph's pen — exactly the
/// space the buffers hold. `pxPerEm` is how many output pixels one em spans,
/// which is the whole of the size dependence: the reference takes it from
/// `fwidth` of an interpolated texcoord, and here the caller passes it, both because a compute
/// shader has no derivatives to take and because axis-aligned text knows the
/// number exactly rather than to within a quad.
public float textCoverageAniso(uint glyphIndex, float2 p, float2 pxPerEm)
{
    TextGlyph g = textGlyphAt(glyphIndex);

    // A space, an empty glyf entry, a path with nothing filled, an index this
    // buffer has no record for: no bands, no ink, and no buffer touched.
    if (g.hband_count == 0u || g.vband_count == 0u)
    {
        return 0.0;
    }

    // The glyph's own band records must lie inside the band buffer before
    // either walk reads one. A record whose `band_base` points past the end is
    // not a glyph at all — it is the headroom, or a stale upload — and the
    // honest answer for a mask is no ink.
    let bandCount = textCountOf(textBands);
    if (g.band_base + g.hband_count + g.vband_count > bandCount)
    {
        return 0.0;
    }
    let curveCount = textCountOf(textBandCurves);
    let pointCount = textCountOf(textPoints);

    int2 bandMax = int2(int(g.vband_count) - 1, int(g.hband_count) - 1);
    int2 bandIndex = clamp(
        int2(p * g.band_transform.xy + g.band_transform.zw),
        int2(0, 0),
        bandMax);

    // --- the horizontal band: a ray along +x, crossings from y = 0 ---------

    float xcov = 0.0;
    float xwgt = 0.0;

    TextBand hband = textBands[g.band_base + uint(bandIndex.y)];
    // The band's own slice of the curve buffer, clipped to it: `count` comes
    // out of a buffer, so it is data and not a promise.
    uint hcount = hband.first < curveCount ? min(hband.count, curveCount - hband.first) : 0u;
    for (uint i = 0u; i < hcount; ++i)
    {
        uint slot = textBandCurves[hband.first + i];
        if (slot + 1u >= pointCount)
        {
            break;
        }
        float2 p1 = textPoints[slot].p - p;
        float2 p2 = textPoints[slot].c - p;
        float2 p3 = textPoints[slot + 1u].p - p;

        // Sorted descending by maximum x: once the rightmost control point of a
        // curve falls left of this pixel, no later curve in the band can reach
        // it either.
        if (max(max(p1.x, p2.x), p3.x) * pxPerEm.x < -0.5)
        {
            break;
        }

        uint code = textRootCode(p1.y, p2.y, p3.y);
        if (code != 0u)
        {
            float2 r = textSolveHoriz(p1, p2, p3) * pxPerEm.x;

            if ((code & 1u) != 0u)
            {
                xcov += saturate(r.x + 0.5);
                xwgt = max(xwgt, saturate(1.0 - abs(r.x) * 2.0));
            }

            if (code > 1u)
            {
                xcov -= saturate(r.y + 0.5);
                xwgt = max(xwgt, saturate(1.0 - abs(r.y) * 2.0));
            }
        }
    }

    // --- the vertical band: a ray along +y, crossings from x = 0 -----------

    float ycov = 0.0;
    float ywgt = 0.0;

    TextBand vband = textBands[g.band_base + g.hband_count + uint(bandIndex.x)];
    uint vcount = vband.first < curveCount ? min(vband.count, curveCount - vband.first) : 0u;
    for (uint i = 0u; i < vcount; ++i)
    {
        uint slot = textBandCurves[vband.first + i];
        if (slot + 1u >= pointCount)
        {
            break;
        }
        float2 p1 = textPoints[slot].p - p;
        float2 p2 = textPoints[slot].c - p;
        float2 p3 = textPoints[slot + 1u].p - p;

        if (max(max(p1.y, p2.y), p3.y) * pxPerEm.y < -0.5)
        {
            break;
        }

        uint code = textRootCode(p1.x, p2.x, p3.x);
        if (code != 0u)
        {
            float2 r = textSolveVert(p1, p2, p3) * pxPerEm.y;

            if ((code & 1u) != 0u)
            {
                ycov -= saturate(r.x + 0.5);
                ywgt = max(ywgt, saturate(1.0 - abs(r.x) * 2.0));
            }

            if (code > 1u)
            {
                ycov += saturate(r.y + 0.5);
                ywgt = max(ywgt, saturate(1.0 - abs(r.y) * 2.0));
            }
        }
    }

    return textCombine(xcov, ycov, xwgt, ywgt, g.flags);
}

/// [`textCoverageAniso`] for the isotropic case, which is all axis-aligned text:
/// `pxPerEm` is the font size in output pixels.
public float textCoverage(uint glyphIndex, float2 p, float pxPerEm)
{
    return textCoverageAniso(glyphIndex, p, float2(pxPerEm, pxPerEm));
}

// ---------------------------------------------------------------------------
// Placement helpers. Nothing here decides anything; they are the arithmetic
// every caller would otherwise write twice.
// ---------------------------------------------------------------------------

/// Horizontal advance, em units. Multiply by the size for pixels.
public float textAdvance(uint glyphIndex)
{
    return textGlyphAt(glyphIndex).advance;
}

/// The em-space ink box, `(min x, min y, max x, max y)`.
public float4 textBounds(uint glyphIndex)
{
    return textGlyphAt(glyphIndex).bounds;
}

/// The pixel rect a glyph can put ink in, given its origin in pixels (y down,
/// the origin on the baseline at the pen) and its size — **dilated by one pixel
/// on every side**.
///
/// This is Slug's dilation, in the form a compute pass wants. The reference
/// displaces the drawn quad's vertices outward along their normals so that a
/// stem narrower than a pixel still has geometry to be shaded in; a caller here
/// has no quad, only a loop bound, so the same guarantee is a rect one pixel
/// larger. Sampling inside the rect and outside the ink is safe and cheap: the
/// band index clamps and the early `break` fires immediately.
///
/// Returns `(x0, y0, x1, y1)`, a half-open pixel rect.
public float4 textGlyphRect(uint glyphIndex, float2 originPx, float pxPerEm)
{
    float4 b = textGlyphAt(glyphIndex).bounds;
    return float4(
        originPx.x + b.x * pxPerEm - 1.0,
        originPx.y - b.w * pxPerEm - 1.0,
        originPx.x + b.z * pxPerEm + 1.0,
        originPx.y - b.y * pxPerEm + 1.0);
}

/// A pixel-space sample turned into the em-space sample `textCoverage` wants.
///
/// `pxPerEm` scales, the origin translates, and y flips: output pixels run down,
/// and an em square runs up from its baseline.
public float2 textSampleAt(float2 pixelCentre, float2 originPx, float pxPerEm)
{
    return float2(pixelCentre.x - originPx.x, originPx.y - pixelCentre.y) / pxPerEm;
}
