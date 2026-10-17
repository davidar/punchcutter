// The demo reel's three passes: shape, typeset, draw.
//
// Every frame runs all three, in one submit. `reel_shape` is `shape.slang`
// hosted exactly as `shape_demo.slang` hosts it, one thread per string.
// `reel_typeset` is one thread per block of text: it reads the shaper's
// output where the shaper left it, breaks it into lines, justifies or aligns
// them, and writes every glyph's place in the world. `reel_frame` is one
// thread per output pixel: a camera, a lens, four samples of `text.slang`'s
// coverage for every glyph near each of them, the curve overlay, and the
// colour. Nothing comes back to the CPU but the finished pixels.
//
// Set 0 is this file's own; `text.slang`'s four curve buffers are set 1.

import shape;
import text;

/// The size every string is shaped at, pixels per em. Positions come back in
/// 26.6 at this size and are scaled to each block's own; 256 keeps a 64th of a
/// pixel at a sixteen-thousandth of an em. `SHAPE_PX` in `main.rs`.
static const float SHAPE_PX = 256.0;

/// `ReelBlock.flags`.
static const uint BLOCK_SCREEN = 1u; // placed in output pixels, not the world
static const uint BLOCK_HERO = 2u; // holds the glyph the overlay draws
static const uint BLOCK_WAVE = 4u; // swayed by `ReelFrame.look.yzw`
static const uint BLOCK_WRAP = 8u; // broken into lines at `width`
static const uint BLOCK_REVEAL = 16u; // shaped whole, its glyphs shown in turn

/// `ReelBlock.align`.
static const uint ALIGN_LEFT = 0u;
static const uint ALIGN_CENTRE = 1u;
static const uint ALIGN_RIGHT = 2u;
static const uint ALIGN_JUSTIFY = 3u;

/// `ReelRun.flags`.
static const uint RUN_PARAGRAPH = 1u; // starts a paragraph
static const uint RUN_BLANK = 2u; // and leaves an empty line before it

/// `ReelGlyph.flags`.
static const uint GLYPH_SPACE = 1u;
static const uint GLYPH_PARAGRAPH = 2u;
static const uint GLYPH_BLANK = 4u;

static const float NEVER = -1.0e9;

/// A block of text: a title, a caption, a page in columns. Mirrors `Block` in
/// `main.rs`.
struct ReelBlock
{
    float2 origin; // the first baseline's left end, or its centre or right end
    float width; // a column's width; zero for a line that is never broken
    float size; // world units (or pixels) per em
    float leading;
    float gap; // between columns
    uint columns;
    uint rows; // lines per column; zero to balance the columns
    uint align;
    uint flags;
    uint run_first;
    uint run_count;
    uint glyph_first;
    uint glyph_cap;
    uint line_first;
    uint line_cap;
    float type_start; // seconds; with `type_rate`, when each byte appears
    float type_rate; // bytes per second; zero for text that is simply there
    float opacity;
    uint hero_byte; // the hero glyph's byte offset in the block's text
};

/// What the typesetter needs of one shaped string beyond the shaper's own
/// request. Mirrors `RunMeta`.
struct ReelRun
{
    uint block;
    uint byte_in_block;
    uint flags;
    uint _rsvd0;
    float4 colour;
};

/// One glyph, placed: the typesetter's output and the frame pass's input.
struct ReelGlyph
{
    float2 pos; // pen on the baseline, world (or screen) units
    uint gid;
    uint run;
    float appear; // seconds
    float formed; // when a ligature's last component arrived, or NEVER
    float advance; // world units
    uint flags;
};

/// One line: a slice of a block's glyphs and its ink's horizontal extent.
struct ReelLine
{
    uint first;
    uint count;
    float x0;
    float x1;
};

/// What the typesetter learned about a block.
struct ReelBlockOut
{
    float4 rect; // world (or screen) box around everything it placed
    uint lines;
    uint glyphs;
    uint rows; // lines per column, as set
    uint _rsvd0;
};

/// The glyph the overlay draws, found by the typesetter.
struct ReelScene
{
    float2 hero_pos;
    float hero_size;
    uint hero_gid;
    uint curve_min;
    uint curve_max;
    uint hero_found;
    uint hero_glyph;
};

/// Blocks one frame can show at most; past this, the rest are not drawn.
static const uint MAX_VISIBLE = 64u;

/// `ReelView.list`: the block is placed in pixels, not in the world.
static const uint VISIBLE_SCREEN = 0x80000000u;

/// The frame's camera and the blocks on screen, worked out once a frame by
/// `reel_view` so that no pixel does it again.
struct ReelView
{
    float4 camera; // xy centre, z zoom
    uint count;
    uint _rsvd0;
    uint _rsvd1;
    uint _rsvd2;
    uint4 list[MAX_VISIBLE / 4u]; // block indices, VISIBLE_SCREEN set for a screen block
};

/// The frame's clock and camera: the only things the CPU decides per frame.
/// Mirrors `Frame`.
struct ReelFrame
{
    float4 clock; // x seconds
    float4 cam_a; // x, y: first and last block framed (x < 0: the hero glyph); z: fill; w: lift, pixels
    float4 cam_b;
    float4 cam; // x: from a to b; y: the captions' scrim
    float4 lens; // xy centre in pixels, z radius, w magnification
    float4 hero; // x curves, y how much of them is drawn, z fill, w probe
    float4 probe; // xy the probe within the ink box, z bands
    float4 look; // x brightness, y wave height (em), z wave length (em), w wave phase
    uint4 counts; // x blocks, y width, z height, w the strip's first row
    uint4 out; // x pixels per row of `image`; y nonzero to write blue first; z samples, 1 or 4
    uint4 partial; // x strings in `run_list`, or EVERY; y blocks in `block_list`, or EVERY
    uint4 run_list[4]; // the strings to shape again this frame
    uint4 block_list[8]; // the blocks to set again this frame
};

/// `ReelFrame.partial`: shape, or set, all of them.
static const uint EVERY = 0xFFFFFFFFu;

[[vk::binding(0, 0)]] ConstantBuffer<ShapeParams> params;
[[vk::binding(1, 0)]] StructuredBuffer<ShapeRequest> requests;
[[vk::binding(2, 0)]] StructuredBuffer<uint> ot;
[[vk::binding(3, 0)]] RWStructuredBuffer<uint> utf8;
[[vk::binding(4, 0)]] RWStructuredBuffer<ShapedGlyph> out_glyphs;
[[vk::binding(5, 0)]] RWStructuredBuffer<ShapeRun> out_runs;
[[vk::binding(6, 0)]] ConstantBuffer<ReelFrame> frame;
[[vk::binding(7, 0)]] StructuredBuffer<ReelBlock> blocks;
[[vk::binding(8, 0)]] StructuredBuffer<ReelRun> runs;
[[vk::binding(9, 0)]] RWStructuredBuffer<ReelGlyph> placed;
[[vk::binding(10, 0)]] RWStructuredBuffer<ReelLine> lines;
[[vk::binding(11, 0)]] RWStructuredBuffer<ReelBlockOut> block_out;
[[vk::binding(12, 0)]] RWStructuredBuffer<ReelScene> scene;
[[vk::binding(13, 0)]] RWStructuredBuffer<uint> image;
[[vk::binding(14, 0)]] RWStructuredBuffer<ReelView> view;

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

[shader("compute")]
[numthreads(64, 1, 1)]
void reel_shape(uint3 gid: SV_DispatchThreadID)
{
    // Every string on the first frame; after it, only those that change,
    // which is the ones being typed.
    uint index = gid.x;
    if (frame.partial.x != EVERY)
    {
        if (gid.x >= frame.partial.x)
        {
            return;
        }
        index = frame.run_list[gid.x >> 2u][gid.x & 3u];
    }
    if (index >= params.request_count)
    {
        return;
    }
    ShapeTables t;
    t.ot = ot;
    t.utf8 = utf8;
    t.utf8_alt0 = utf8;
    t.utf8_alt1 = utf8;
    t.source = 0u;
    out_runs[index] = shapeRun(t, params, requests[index], out_glyphs, params.glyph_capacity);
}

// ---------------------------------------------------------------------------
// Typeset
// ---------------------------------------------------------------------------

uint byteAt(uint offset)
{
    return (utf8[offset >> 2] >> ((offset & 3u) * 8u)) & 0xFFu;
}

/// Bytes in the UTF-8 sequence a lead byte starts.
uint sequenceLength(uint lead)
{
    return lead < 0x80u ? 1u : (lead < 0xE0u ? 2u : (lead < 0xF0u ? 3u : 4u));
}

/// Lay glyphs `[a, e)` of a block out as its line `index`, `rows` lines to a
/// column: trailing spaces dropped from the measure, the spaces stretched
/// when `justify`, the line aligned otherwise. Returns the ink's box,
/// `(x0, y0, x1, y1)` of pens.
float4 setLine(ReelBlock b, uint a, uint e, uint index, bool justify, uint rows)
{
    uint base = b.glyph_first;
    uint end = e;
    while (end > a && (placed[base + end - 1u].flags & GLYPH_SPACE) != 0u)
    {
        end -= 1u;
    }
    float x0 = a < e ? placed[base + a].pos.x : 0.0;
    float natural = end > a ? placed[base + end - 1u].pos.x + placed[base + end - 1u].advance - x0 : 0.0;
    uint spaces = 0u;
    for (uint k = a; k < end; ++k)
    {
        spaces += (placed[base + k].flags & GLYPH_SPACE) != 0u ? 1u : 0u;
    }
    float extra = 0.0;
    float shift = 0.0;
    if (justify && b.align == ALIGN_JUSTIFY && spaces > 0u && b.width > natural)
    {
        // A line with one word on it and a lot of room would be all gaps.
        extra = min((b.width - natural) / float(spaces), b.size * 0.8);
    }
    else if (b.align == ALIGN_CENTRE)
    {
        shift = (b.width - natural) * 0.5;
    }
    else if (b.align == ALIGN_RIGHT)
    {
        shift = b.width - natural;
    }

    uint col = rows > 0u ? index / rows : 0u;
    uint row = rows > 0u ? index % rows : index;
    float2 origin = b.origin + float2(float(col) * (b.width + b.gap) + shift, float(row) * b.leading);

    float4 box = float4(1.0e30, origin.y, -1.0e30, origin.y);
    uint seen = 0u;
    for (uint k = a; k < e; ++k)
    {
        ReelGlyph g = placed[base + k];
        if ((g.flags & GLYPH_SPACE) != 0u)
        {
            seen += 1u;
        }
        g.pos = float2(origin.x + (g.pos.x - x0) + extra * float(seen), origin.y + g.pos.y);
        placed[base + k] = g;
        if ((g.flags & GLYPH_SPACE) == 0u)
        {
            box.x = min(box.x, g.pos.x);
            box.z = max(box.z, g.pos.x + g.advance);
        }
    }
    if (index < b.line_cap)
    {
        ReelLine line;
        line.first = base + a;
        line.count = e - a;
        line.x0 = box.x;
        line.x1 = box.z;
        lines[b.line_first + index] = line;
    }
    return box;
}

/// Break a block's `n` glyphs into lines, greedily, and count them. With
/// `commit`, every line is also set (`rows` to a column) and `box` grows
/// around them; without, nothing is written, which is how the columns are
/// balanced: one count, then the real thing.
uint flow(ReelBlock b, uint n, uint rows, bool commit, inout float4 box)
{
    uint count = 0u;
    uint i0 = 0u;
    int lastSpace = -1;
    bool wrap = (b.flags & BLOCK_WRAP) != 0u && b.width > 0.0;
    for (uint i = 0u; i < n; ++i)
    {
        ReelGlyph g = placed[b.glyph_first + i];
        if (i > i0 && (g.flags & GLYPH_PARAGRAPH) != 0u)
        {
            if (commit)
            {
                float4 one = setLine(b, i0, i, count, false, rows);
                box = float4(min(box.xy, one.xy), max(box.zw, one.zw));
            }
            count += 1u;
            if ((g.flags & GLYPH_BLANK) != 0u)
            {
                if (commit && count < b.line_cap)
                {
                    ReelLine empty;
                    empty.first = b.glyph_first + i;
                    empty.count = 0u;
                    empty.x0 = 0.0;
                    empty.x1 = 0.0;
                    lines[b.line_first + count] = empty;
                }
                count += 1u;
            }
            i0 = i;
            lastSpace = -1;
        }
        if ((g.flags & GLYPH_SPACE) != 0u)
        {
            lastSpace = int(i);
            continue;
        }
        // Uncommitted, every glyph still holds its place on the unbroken
        // paragraph, so the measure is the same both times.
        float x0 = placed[b.glyph_first + i0].pos.x;
        if (wrap && g.pos.x + g.advance - x0 > b.width && lastSpace > int(i0))
        {
            if (commit)
            {
                float4 one = setLine(b, i0, uint(lastSpace), count, true, rows);
                box = float4(min(box.xy, one.xy), max(box.zw, one.zw));
            }
            count += 1u;
            i0 = uint(lastSpace) + 1u;
            lastSpace = -1;
        }
    }
    if (n > i0)
    {
        if (commit)
        {
            float4 one = setLine(b, i0, n, count, false, rows);
            box = float4(min(box.xy, one.xy), max(box.zw, one.zw));
        }
        count += 1u;
    }
    return count;
}

[shader("compute")]
[numthreads(64, 1, 1)]
void reel_typeset(uint3 tid: SV_DispatchThreadID)
{
    uint bi = tid.x;
    if (frame.partial.y != EVERY)
    {
        if (tid.x >= frame.partial.y)
        {
            return;
        }
        bi = frame.block_list[tid.x >> 2u][tid.x & 3u];
    }
    if (bi >= frame.counts.x)
    {
        return;
    }
    ReelBlock b = blocks[bi];
    float scale = b.size / (64.0 * SHAPE_PX);

    // --- every glyph of every run, on one unbroken line per paragraph -----

    uint n = 0u;
    uint hero = 0xFFFFFFFFu;
    float pen = 0.0;
    for (uint r = b.run_first; r < b.run_first + b.run_count; ++r)
    {
        ReelRun meta = runs[r];
        ShapeRun res = out_runs[r];
        ShapeRequest req = requests[r];
        if ((meta.flags & RUN_PARAGRAPH) != 0u)
        {
            pen = 0.0;
        }
        uint count = min(res.glyph_count, req.out_cap);
        for (uint j = 0u; j < count && n < b.glyph_cap; ++j)
        {
            ShapedGlyph s = out_glyphs[req.out_start + j];
            ReelGlyph g;
            g.gid = s.gid;
            g.run = r;
            g.pos = float2(pen + float(s.x) * scale, -float(s.y) * scale);
            g.advance = textAdvance(s.gid) * b.size;
            uint lead = byteAt(req.byte_start + s.cluster);
            g.flags = lead == 0x20u ? GLYPH_SPACE : 0u;
            if (j == 0u && (meta.flags & RUN_PARAGRAPH) != 0u)
            {
                g.flags |= GLYPH_PARAGRAPH | ((meta.flags & RUN_BLANK) != 0u ? GLYPH_BLANK : 0u);
            }

            g.appear = NEVER;
            g.formed = NEVER;
            if (b.type_rate > 0.0)
            {
                g.appear = b.type_start + float(meta.byte_in_block + s.cluster) / b.type_rate;
                // A ligature is a glyph whose cluster spans more than one
                // character and is not shared with a neighbour (a base and its
                // mark share one).
                uint next = j + 1u < count ? out_glyphs[req.out_start + j + 1u].cluster : req.byte_len;
                bool shared = (j > 0u && out_glyphs[req.out_start + j - 1u].cluster == s.cluster)
                    || next == s.cluster;
                if (!shared && next > s.cluster + sequenceLength(lead))
                {
                    g.formed = b.type_start + float(meta.byte_in_block + next - 1u) / b.type_rate;
                }
            }
            if ((b.flags & BLOCK_HERO) != 0u && meta.byte_in_block + s.cluster == b.hero_byte)
            {
                hero = n;
            }
            placed[b.glyph_first + n] = g;
            n += 1u;
        }
        pen += float(res.advance) * scale;
    }

    // --- greedy line breaking, twice when the columns are to balance -----

    uint rows = b.rows;
    float4 box = float4(1.0e30, 1.0e30, -1.0e30, -1.0e30);
    if (rows == 0u && b.columns > 1u)
    {
        uint total = flow(b, n, 0u, false, box);
        rows = max((total + b.columns - 1u) / b.columns, 1u);
    }
    uint count = flow(b, n, rows, true, box);

    ReelBlockOut out;
    out.rect = n > 0u
        ? float4(box.x - b.size * 0.2, box.y - b.size * 1.1, box.z + b.size * 0.2, box.w + b.size * 0.45)
        : float4(b.origin, b.origin);
    out.lines = min(count, b.line_cap);
    out.glyphs = n;
    out.rows = rows;
    out._rsvd0 = 0u;
    block_out[bi] = out;

    if (hero != 0xFFFFFFFFu)
    {
        ReelGlyph g = placed[b.glyph_first + hero];
        ReelScene s;
        s.hero_pos = g.pos;
        s.hero_size = b.size;
        s.hero_gid = g.gid;
        s.hero_found = 1u;
        s.hero_glyph = b.glyph_first + hero;
        // The glyph's curves are one contiguous stretch of the point buffer;
        // its bands name every one of them.
        TextGlyph tg = textGlyphAt(g.gid);
        uint lo = 0xFFFFFFFFu;
        uint hi = 0u;
        for (uint h = 0u; h < tg.hband_count; ++h)
        {
            TextBand band = textBands[tg.band_base + h];
            for (uint k = 0u; k < band.count; ++k)
            {
                uint slot = textBandCurves[band.first + k];
                lo = min(lo, slot);
                hi = max(hi, slot);
            }
        }
        s.curve_min = lo;
        s.curve_max = hi;
        scene[0] = s;
    }
}

// ---------------------------------------------------------------------------
// Draw
// ---------------------------------------------------------------------------

static const float3 INK_BG = float3(0.047, 0.043, 0.040);
static const float3 IVORY = float3(0.93, 0.89, 0.81);
static const float3 COPPER = float3(0.91, 0.50, 0.30);
static const float3 TEAL = float3(0.42, 0.74, 0.80);

/// A box in world units: the hero's ink, or blocks `first..=last`.
float4 targetRect(float4 target)
{
    if (target.x < 0.0)
    {
        ReelScene s = scene[0];
        float4 ink = textBounds(s.hero_gid) * s.hero_size;
        return float4(s.hero_pos.x + ink.x, s.hero_pos.y - ink.w, s.hero_pos.x + ink.z, s.hero_pos.y - ink.y);
    }
    float4 r = float4(1.0e30, 1.0e30, -1.0e30, -1.0e30);
    for (uint i = uint(target.x); i <= uint(target.y) && i < frame.counts.x; ++i)
    {
        if (block_out[i].glyphs > 0u)
        {
            float4 o = block_out[i].rect;
            r = float4(min(r.xy, o.xy), max(r.zw, o.zw));
        }
    }
    // Nothing placed yet: frame the screen at unit zoom rather than divide by
    // an empty box.
    return r.z > r.x && r.w > r.y ? r : float4(0.0, 0.0, float2(frame.counts.yz));
}

/// The camera as a centre and a zoom (pixels per world unit), between two
/// framings.
///
/// The zoom is interpolated geometrically. The centre moves in proportion to
/// the *view's* size rather than to time, so that a zoom out from a glyph
/// keeps the glyph where it is while the view is still small and only drifts
/// once it is large: what a single continuous camera move looks like.
float3 camera()
{
    float2 screen = float2(frame.counts.yz);
    float4 ra = targetRect(frame.cam_a);
    float4 rb = targetRect(frame.cam_b);
    float za = min(screen.x / (ra.z - ra.x), screen.y / (ra.w - ra.y)) * frame.cam_a.z;
    float zb = min(screen.x / (rb.z - rb.x), screen.y / (rb.w - rb.y)) * frame.cam_b.z;
    float s = frame.cam.x;
    float z = exp(lerp(log(za), log(zb), s));
    float spread = abs(1.0 / zb - 1.0 / za);
    float w = spread > 0.05 * max(1.0 / za, 1.0 / zb) ? (1.0 / z - 1.0 / za) / (1.0 / zb - 1.0 / za) : s;
    float2 centre = lerp((ra.xy + ra.zw) * 0.5, (rb.xy + rb.zw) * 0.5, saturate(w));
    // The lift raises the framing on screen, clear of the captions.
    centre.y += lerp(frame.cam_a.w, frame.cam_b.w, s) / z;
    return float3(centre, z);
}

/// The lens: a screen point inside it moves toward the centre, by the full
/// magnification in the middle and by none at the rim.
float2 throughLens(float2 s)
{
    float4 lens = frame.lens;
    if (lens.z <= 0.0 || lens.w <= 1.0)
    {
        return s;
    }
    float2 d = s - lens.xy;
    float r = length(d) / lens.z;
    if (r >= 1.0)
    {
        return s;
    }
    float m = lerp(lens.w, 1.0, smoothstep(0.72, 1.0, r));
    return lens.xy + d / m;
}

float2 toWorld(float2 s, float3 cam)
{
    float2 half = float2(frame.counts.yz) * 0.5;
    return (throughLens(s) - half) / cam.z + cam.xy;
}

/// Index of the first glyph of `line` whose pen is right of `x`.
uint firstRightOf(ReelLine line, float x)
{
    uint lo = line.first;
    uint hi = line.first + line.count;
    while (lo < hi)
    {
        uint mid = (lo + hi) / 2u;
        if (placed[mid].pos.x <= x)
        {
            lo = mid + 1u;
        }
        else
        {
            hi = mid;
        }
    }
    return lo;
}

/// One sample of every world block, or of every screen one: `p` is the world
/// point and `ps` the screen point; `pxPerUnit` is pixels per world unit at
/// this point, and `footprint` how many samples span a pixel along each axis.
float3 shadeSample(float3 colour, float2 p, float2 ps, float2 pxPerUnit, float footprint, float t, bool screenPass)
{
    ReelScene sc = scene[0];
    uint visible = view[0].count;
    for (uint k = 0u; k < visible; ++k)
    {
        uint entry = view[0].list[k >> 2u][k & 3u];
        bool screen = (entry & VISIBLE_SCREEN) != 0u;
        if (screen != screenPass)
        {
            continue;
        }
        uint bi = entry & ~VISIBLE_SCREEN;
        float2 q = screen ? ps : p;
        // The box first, and the block's record only for a sample inside it.
        float4 rect = block_out[bi].rect;
        float pad = blocks[bi].size * 0.5;
        if (q.x < rect.x - pad || q.x > rect.z + pad || q.y < rect.y - pad || q.y > rect.w + pad)
        {
            continue;
        }
        ReelBlock b = blocks[bi];
        ReelBlockOut o = block_out[bi];
        float2 unitPx = screen ? float2(1.0, 1.0) : pxPerUnit;
        if ((b.flags & BLOCK_WAVE) != 0u && frame.look.y != 0.0)
        {
            float u = (q.x - b.origin.x) / b.size;
            q.y -= frame.look.y * b.size * sin(u / frame.look.z * 6.2831853 + frame.look.w);
        }
        float2 pxPerEm = b.size * unitPx * footprint;

        // The lines a sample can touch: those whose ascender or descender
        // reaches it, in the column it falls in.
        float2 local = q - b.origin;
        uint col = 0u;
        if (b.columns > 1u)
        {
            col = uint(clamp(floor(local.x / (b.width + b.gap)), 0.0, float(b.columns - 1u)));
        }
        int rowLo = int(ceil((local.y - 0.45 * b.size) / b.leading));
        int rowHi = int(floor((local.y + 1.15 * b.size) / b.leading));
        int rowCap = o.rows > 0u ? int(o.rows) - 1 : int(o.lines) - 1;
        rowLo = max(rowLo, 0);
        rowHi = min(min(rowHi, rowCap), rowLo + 3);
        for (int row = rowLo; row <= rowHi; ++row)
        {
            uint index = col * o.rows + uint(row);
            if (index >= o.lines)
            {
                continue;
            }
            ReelLine line = lines[b.line_first + index];
            if (line.count == 0u || q.x < line.x0 - b.size || q.x > line.x1 + b.size)
            {
                continue;
            }
            uint k = firstRightOf(line, q.x);
            uint from = max(k, line.first + 3u) - 3u;
            uint to = min(k + 1u, line.first + line.count);
            for (uint gi = from; gi < to; ++gi)
            {
                ReelGlyph g = placed[gi];
                if ((g.flags & GLYPH_SPACE) != 0u)
                {
                    continue;
                }
                float a = b.opacity;
                float rise = 0.0;
                if (g.appear > NEVER)
                {
                    float f = smoothstep(g.appear, g.appear + 0.22, t);
                    a *= f;
                    rise = (1.0 - f) * 0.35;
                }
                if (sc.hero_found != 0u && gi == sc.hero_glyph)
                {
                    a = frame.hero.z * (a / max(b.opacity, 1.0e-6));
                }
                if (a <= 0.0)
                {
                    continue;
                }
                float2 e = float2(q.x - g.pos.x, g.pos.y - q.y) / b.size + float2(0.0, rise);
                float4 ink = textBounds(g.gid);
                float2 m = 1.0 / pxPerEm;
                if (e.x < ink.x - m.x || e.x > ink.z + m.x || e.y < ink.y - m.y || e.y > ink.w + m.y)
                {
                    continue;
                }
                float c = textCoverageAniso(g.gid, e, pxPerEm);
                float3 paint = runs[g.run].colour.rgb;
                if (g.formed > NEVER && t >= g.formed)
                {
                    paint = lerp(paint, COPPER * 1.15, exp(-(t - g.formed) * 1.6));
                }
                colour = lerp(colour, paint, saturate(c) * a);
            }
        }
    }
    return colour;
}

// --- the overlay's geometry, in em units -----------------------------------

float segment(float2 p, float2 a, float2 b)
{
    float2 ab = b - a;
    float h = saturate(dot(p - a, ab) / max(dot(ab, ab), 1.0e-12));
    return length(p - a - ab * h);
}

/// Distance from `p` to the quadratic `a, b, c`: the closed form, through
/// the cubic for the nearest parameter.
float quadratic(float2 p, float2 a, float2 b, float2 c)
{
    float2 A = b - a;
    float2 B = a - 2.0 * b + c;
    if (dot(B, B) < 1.0e-10)
    {
        return segment(p, a, c);
    }
    float2 C = A * 2.0;
    float2 D = a - p;
    float kk = 1.0 / dot(B, B);
    float kx = kk * dot(A, B);
    float ky = kk * (2.0 * dot(A, A) + dot(D, B)) / 3.0;
    float kz = kk * dot(D, A);
    float pp = ky - kx * kx;
    float p3 = pp * pp * pp;
    float q = kx * (2.0 * kx * kx - 3.0 * ky) + kz;
    float h = q * q + 4.0 * p3;
    if (h >= 0.0)
    {
        h = sqrt(h);
        float2 x = (float2(h, -h) - q) * 0.5;
        float2 uv = sign(x) * pow(abs(x), float2(1.0 / 3.0, 1.0 / 3.0));
        float tt = saturate(uv.x + uv.y - kx);
        float2 d = D + (C + B * tt) * tt;
        return length(d);
    }
    float z = sqrt(-pp);
    float v = acos(clamp(q / (pp * z * 2.0), -1.0, 1.0)) / 3.0;
    float mm = cos(v);
    float nn = sin(v) * 1.732050808;
    float3 tt = saturate(float3(mm + mm, -nn - mm, nn - mm) * z - kx);
    float2 d0 = D + (C + B * tt.x) * tt.x;
    float2 d1 = D + (C + B * tt.y) * tt.y;
    return sqrt(min(dot(d0, d0), dot(d1, d1)));
}

float2 bezier(float2 a, float2 b, float2 c, float t)
{
    return lerp(lerp(a, b, t), lerp(b, c, t), t);
}

/// Antialiased ink for a distance, `width` and `px` in the same units.
float stroke(float d, float width, float px)
{
    return 1.0 - smoothstep(width * 0.5 - px * 0.5, width * 0.5 + px * 0.5, d);
}

/// Where a curve crosses the line `v = level` along one axis, as up to two
/// parameters in [0, 1); `-1` for none. `v0..v2` are the curve's coordinates
/// on that axis.
float2 crossings(float v0, float v1, float v2, float level)
{
    float a = v0 - 2.0 * v1 + v2;
    float b = v0 - v1;
    float c = v0 - level;
    float2 t = float2(-1.0, -1.0);
    if (abs(a) < 1.0e-7)
    {
        if (abs(b) > 1.0e-7)
        {
            t.x = c / (2.0 * b);
        }
    }
    else
    {
        float disc = b * b - a * c;
        if (disc >= 0.0)
        {
            float r = sqrt(disc);
            t = float2((b - r) / a, (b + r) / a);
        }
    }
    t.x = t.x >= 0.0 && t.x < 1.0 ? t.x : -1.0;
    t.y = t.y >= 0.0 && t.y < 1.0 ? t.y : -1.0;
    return t;
}

/// One curve of the hero glyph, as the overlay draws it at em-space point
/// `e`: its part drawn so far, its hull and points, and where the probe's
/// rays cross it. `reach` is how far from the curve's control box any of
/// that can be seen.
void overlayCurve(uint ci, float2 e, float pe, float reach, float2 probe, bool nearRayH,
                  bool nearRayV, float span, uint curveMin, inout float line, inout float hull,
                  inout float dots, inout float rings, inout float ringSign)
{
    float curves = frame.hero.x;
    float probeOn = frame.hero.w;
    // A band names curves only, never a contour's tail entry.
    TextPoint here = textPoints[ci];
    float2 a = here.p;
    float2 c = here.c;
    float2 z = textPoints[ci + 1u].p;
    float drawn = saturate(frame.hero.y * span - float(ci - curveMin));
    float2 lo = min(min(a, c), z) - reach;
    float2 hi = max(max(a, c), z) + reach;
    bool nearCurve = all(e >= lo) && all(e <= hi);
    if (drawn > 0.0 && curves > 0.0 && nearCurve)
    {
        // The part drawn so far, by de Casteljau: the same curve cut at
        // `drawn`.
        float2 c1 = lerp(a, c, drawn);
        float2 z1 = bezier(a, c, z, drawn);
        line = max(line, stroke(quadratic(e, a, c1, z1), 2.2 * pe, pe));
        float settled = saturate(drawn * 4.0 - 3.0);
        hull = max(hull, settled * stroke(min(segment(e, a, c), segment(e, c, z)), 1.0 * pe, pe));
        dots = max(dots, settled * stroke(length(e - a), 7.0 * pe, pe));
        float2 sq = abs(e - c);
        float box = abs(max(sq.x, sq.y) - 3.0 * pe);
        dots = max(dots, settled * 0.8 * stroke(box, 1.4 * pe, pe));
    }
    if (probeOn > 0.0 && (nearRayH || nearRayV))
    {
        // The +x ray's crossings, and the +y ray's: rings sit on the rays,
        // so only a pixel beside one can be in one.
        float2 th = nearRayH ? crossings(a.y, c.y, z.y, probe.y) : float2(-1.0, -1.0);
        float2 tv = nearRayV ? crossings(a.x, c.x, z.x, probe.x) : float2(-1.0, -1.0);
        for (uint k = 0u; k < 4u; ++k)
        {
            float tt = k < 2u ? th[k] : tv[k - 2u];
            if (tt < 0.0)
            {
                continue;
            }
            float2 at = bezier(a, c, z, tt);
            float2 slope = lerp(c - a, z - c, tt);
            bool ahead = k < 2u ? at.x > probe.x : at.y > probe.y;
            if (!ahead)
            {
                continue;
            }
            float ring = stroke(abs(length(e - at) - 6.0 * pe), 2.0 * pe, pe);
            if (ring > rings)
            {
                rings = ring;
                ringSign = (k < 2u ? slope.y : -slope.x) > 0.0 ? 1.0 : -1.0;
            }
        }
    }
}

/// The hero glyph as Slug sees it: its bands, its curves drawing themselves
/// on, their control points, and a probe sample casting the two rays the
/// coverage walk casts, with every crossing it counts.
float3 overlay(float3 colour, float2 world, float px)
{
    ReelScene sc = scene[0];
    if (sc.hero_found == 0u)
    {
        return colour;
    }
    float2 e = float2(world.x - sc.hero_pos.x, sc.hero_pos.y - world.y) / sc.hero_size;
    float pe = px / sc.hero_size; // em per pixel
    TextGlyph g = textGlyphAt(sc.hero_gid);
    float4 ink = g.bounds;
    float margin = 60.0 * pe + 0.15;
    if (e.x < ink.x - margin || e.x > ink.z + margin || e.y < ink.y - margin || e.y > ink.w + margin)
    {
        return colour;
    }

    // The horizontal bands, alternately shaded across the ink box, with a
    // hairline between each.
    float bands = frame.probe.z;
    if (bands > 0.0 && e.x > ink.x - 0.04 && e.x < ink.z + 0.04 && e.y > ink.y && e.y < ink.w)
    {
        float f = e.y * g.band_transform.y + g.band_transform.w;
        float shade = (uint(floor(f)) & 1u) != 0u ? 0.045 : 0.0;
        float edge = abs(f - round(f)) / (g.band_transform.y * pe);
        colour = lerp(colour, IVORY, bands * (shade + 0.10 * (1.0 - saturate(edge))));
    }

    float curves = frame.hero.x;
    float probeOn = frame.hero.w;
    float2 probe = lerp(ink.xy, ink.zw, frame.probe.xy);
    float span = float(sc.curve_max - sc.curve_min + 1u);

    float line = 0.0;
    float hull = 0.0;
    float dots = 0.0;
    float rings = 0.0;
    float ringSign = 0.0;
    // Nothing this draws reaches further from a curve's control box than a
    // dot's radius, or further from a ray than a ring's: past that, a curve
    // costs one box test and nothing else.
    float reach = 8.0 * pe;
    bool nearRayH = abs(e.y - probe.y) < reach && e.x > probe.x - reach;
    bool nearRayV = abs(e.x - probe.x) < reach && e.y > probe.y - reach;
    // The curves near `e` are the ones its bands name: the horizontal bands
    // within reach of its y, and the vertical ones within reach of its x (a
    // band leaves out the curves flat along it, which the other direction's
    // bands hold). A curve in two of them is drawn twice, to the same result.
    float4 bt = g.band_transform;
    int hlo = clamp(int(floor((e.y - reach) * bt.y + bt.w)), 0, int(g.hband_count) - 1);
    int hhi = clamp(int(floor((e.y + reach) * bt.y + bt.w)), 0, int(g.hband_count) - 1);
    int vlo = clamp(int(floor((e.x - reach) * bt.x + bt.z)), 0, int(g.vband_count) - 1);
    int vhi = clamp(int(floor((e.x + reach) * bt.x + bt.z)), 0, int(g.vband_count) - 1);
    uint hbands = uint(hhi - hlo + 1);
    uint near = hbands + uint(vhi - vlo + 1);
    for (uint k = 0u; k < near; ++k)
    {
        uint index = k < hbands ? g.band_base + uint(hlo) + k
                                : g.band_base + g.hband_count + uint(vlo) + (k - hbands);
        TextBand band = textBands[index];
        for (uint j = 0u; j < band.count; ++j)
        {
            overlayCurve(textBandCurves[band.first + j], e, pe, reach, probe, nearRayH, nearRayV,
                         span, sc.curve_min, line, hull, dots, rings, ringSign);
        }
    }

    colour = lerp(colour, IVORY * 0.7, curves * hull * 0.55);
    colour = lerp(colour, COPPER, curves * line);
    colour = lerp(colour, IVORY, curves * dots);

    if (probeOn > 0.0)
    {
        float rayH = e.x > probe.x ? stroke(abs(e.y - probe.y), 1.3 * pe, pe) * saturate((ink.z + 0.12 - e.x) / (0.12)) : 0.0;
        float rayV = e.y > probe.y ? stroke(abs(e.x - probe.x), 1.3 * pe, pe) * saturate((ink.w + 0.12 - e.y) / (0.12)) : 0.0;
        colour = lerp(colour, IVORY, probeOn * 0.75 * max(rayH, rayV));
        colour = lerp(colour, ringSign > 0.0 ? COPPER * 1.1 : TEAL, probeOn * rings);
        float d = length(e - probe);
        colour += probeOn * IVORY * 0.35 * exp(-d / (14.0 * pe));
        colour = lerp(colour, IVORY, probeOn * stroke(d, 9.0 * pe, pe));
    }
    return colour;
}

/// The lens's glass: a slightly lighter field, a rim, and a soft shadow
/// outside it.
float3 lensGlass(float3 colour, float2 s)
{
    float4 lens = frame.lens;
    if (lens.z <= 0.0 || lens.w <= 1.0)
    {
        return colour;
    }
    float strength = saturate((lens.w - 1.0) / 0.6);
    float d = length(s - lens.xy);
    float r = d / lens.z;
    if (r < 1.0)
    {
        colour += strength * 0.025 * (1.0 - r * r);
    }
    else
    {
        colour *= 1.0 - strength * 0.45 * exp(-(d - lens.z) / 10.0);
    }
    colour = lerp(colour, COPPER, strength * stroke(abs(d - lens.z), 2.0, 1.0));
    colour = lerp(colour, IVORY, strength * 0.25 * stroke(abs(d - lens.z + 4.0), 1.0, 1.0));
    return colour;
}

/// Eight bits a channel, red first, or blue first for a BGRA swapchain.
uint pack(float3 c, bool bgra)
{
    uint3 v = uint3(round(saturate(c) * 255.0));
    v = bgra ? v.zyx : v;
    return v.x | (v.y << 8u) | (v.z << 16u) | (0xFFu << 24u);
}

/// The camera, and which blocks reach the screen: one thread, once a frame,
/// after the typesetter has placed everything.
[shader("compute")]
[numthreads(1, 1, 1)]
void reel_view(uint3 tid: SV_DispatchThreadID)
{
    float3 cam = camera();
    float2 size = float2(frame.counts.yz);
    // What the screen shows of the world. The lens shows a part of what lies
    // under it, so it never widens this.
    float2 lo = -size * 0.5 / cam.z + cam.xy;
    float2 hi = size * 0.5 / cam.z + cam.xy;
    uint count = 0u;
    for (uint bi = 0u; bi < frame.counts.x && count < MAX_VISIBLE; ++bi)
    {
        ReelBlock b = blocks[bi];
        ReelBlockOut o = block_out[bi];
        if (b.opacity <= 0.0 || o.glyphs == 0u)
        {
            continue;
        }
        bool screen = (b.flags & BLOCK_SCREEN) != 0u;
        float pad = b.size * (0.5 + abs(frame.look.y));
        float2 vlo = screen ? float2(0.0, 0.0) : lo;
        float2 vhi = screen ? size : hi;
        if (o.rect.z + pad < vlo.x || o.rect.x - pad > vhi.x || o.rect.w + pad < vlo.y || o.rect.y - pad > vhi.y)
        {
            continue;
        }
        view[0].list[count >> 2u][count & 3u] = bi | (screen ? VISIBLE_SCREEN : 0u);
        count += 1u;
    }
    view[0].camera = float4(cam, 0.0);
    view[0].count = count;
}

[shader("compute")]
[numthreads(8, 8, 1)]
void reel_frame(uint3 dispatched: SV_DispatchThreadID)
{
    // A frame is drawn a strip of rows at a time, so that no one dispatch
    // holds a GPU the desktop shares for long.
    uint3 tid = dispatched + uint3(0u, frame.counts.w, 0u);
    uint width = frame.counts.y;
    uint height = frame.counts.z;
    if (tid.x >= width || tid.y >= height)
    {
        return;
    }
    float t = frame.clock.x;
    float2 s = float2(tid.xy) + 0.5;
    float2 size = float2(width, height);
    float3 cam = view[0].camera.xyz;

    // How far one pixel reaches in the world here, along each axis: the
    // camera's zoom, and the lens's magnification where it is over.
    float2 w = toWorld(s, cam);
    float2 reach = float2(length(toWorld(s + float2(1.0, 0.0), cam) - w),
                          length(toWorld(s + float2(0.0, 1.0), cam) - w));
    float2 pxPerUnit = 1.0 / max(reach, float2(1.0e-6, 1.0e-6));

    float2 uv = s / size;
    float3 bg = INK_BG * (1.0 + 0.55 * exp(-4.0 * dot(uv - float2(0.5, 0.42), uv - float2(0.5, 0.42))));

    // Four samples on a rotated grid, each half a pixel across; or one, a
    // whole pixel across, where pixels are small enough not to need more.
    static const float2 OFFSETS[4] = { float2(-0.125, -0.375), float2(0.375, -0.125),
                                       float2(0.125, 0.375), float2(-0.375, 0.125) };
    uint samples = frame.out.z == 1u ? 1u : 4u;
    float footprint = samples == 1u ? 1.0 : 2.0;
    float3 sum = float3(0.0, 0.0, 0.0);
    for (uint i = 0u; i < samples; ++i)
    {
        float2 ss = samples == 1u ? s : s + OFFSETS[i];
        sum += shadeSample(bg, toWorld(ss, cam), ss, pxPerUnit, footprint, t, false);
    }
    float3 colour = sum / float(samples);

    if (frame.hero.x > 0.0 || frame.hero.w > 0.0 || frame.probe.z > 0.0)
    {
        colour = overlay(colour, w, reach.x);
    }
    colour = lensGlass(colour, s);

    // The captions, over a scrim that darkens the bottom of the frame while
    // one is up.
    colour *= 1.0 - frame.cam.y * 0.7 * smoothstep(0.68, 0.9, uv.y) * smoothstep(0.75, 0.0, uv.x);
    sum = float3(0.0, 0.0, 0.0);
    for (uint i = 0u; i < samples; ++i)
    {
        float2 ss = samples == 1u ? s : s + OFFSETS[i];
        sum += shadeSample(colour, ss, ss, float2(1.0, 1.0), footprint, t, true);
    }
    colour = sum / float(samples);

    float2 v = uv - 0.5;
    colour *= 1.0 - 0.45 * pow(saturate(length(v * float2(1.0, size.y / size.x)) * 1.35), 2.6);
    colour *= frame.look.x;
    // A sixth of a step of ordered dither, so the background's falloff does
    // not band.
    uint bx = tid.x & 1u;
    uint by = tid.y & 1u;
    float dither = (float((bx ^ by) * 2u + by) / 4.0 - 0.375) / 255.0;
    image[tid.y * frame.out.x + tid.x] = pack(colour + dither, frame.out.y != 0u);
}
