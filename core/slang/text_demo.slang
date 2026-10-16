// The headless harness's entry point: `text.slang`'s coverage, into a buffer.
//
// This is the only entry point `punchcutter-text`'s harness has — `text.slang` is a
// module, and a consumer's shader imports it. Nothing here is
// a drawing path: one thread per output pixel, a loop over placed glyphs, and a
// float of coverage per pixel for `text/examples/render.rs` to read back and
// turn into a PNG. A real consumer shades
// the coverage with a colour and composites; this one hands it over raw so that
// what you look at is exactly what the algorithm produced.
//
// The font's buffers are `text.slang`'s, in descriptor set 1. Set 0 is this
// file's own, which is how a consumer keeps its own bindings in set 0.

import text;

/// One glyph, placed. `origin` is in pixels, y down, on the baseline at the pen,
/// which is where a shaper's output lands.
struct TextPlacement
{
    uint glyph;
    float px_per_em;
    float2 origin;
};

struct TextDemoParams
{
    uint width;
    uint height;
    uint count;
    uint _rsvd0;
};

[[vk::binding(0, 0)]]
RWStructuredBuffer<float> coverageOut;

[[vk::binding(1, 0)]]
StructuredBuffer<TextPlacement> placements;

[[vk::binding(2, 0)]]
ConstantBuffer<TextDemoParams> demo;

[numthreads(8, 8, 1)]
void text_demo(uint3 tid : SV_DispatchThreadID)
{
    if (tid.x >= demo.width || tid.y >= demo.height)
    {
        return;
    }

    float2 centre = float2(float(tid.x) + 0.5, float(tid.y) + 0.5);
    float coverage = 0.0;

    for (uint i = 0u; i < demo.count; ++i)
    {
        TextPlacement place = placements[i];

        // The dilated rect is the cull, which is also the demonstration that the
        // dilation is a loop bound rather than a displaced quad.
        float4 rect = textGlyphRect(place.glyph, place.origin, place.px_per_em);
        if (centre.x < rect.x || centre.x >= rect.z || centre.y < rect.y || centre.y >= rect.w)
        {
            continue;
        }

        float2 p = textSampleAt(centre, place.origin, place.px_per_em);
        // Glyphs in a run rarely overlap, and where two do (a mark over its
        // base) the larger coverage is the right answer for a single-channel
        // mask.
        coverage = max(coverage, textCoverage(place.glyph, p, place.px_per_em));
    }

    coverageOut[tid.y * demo.width + tid.x] = coverage;
}
