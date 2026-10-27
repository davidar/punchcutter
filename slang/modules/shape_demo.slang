// The entry points `shape/examples/conform.rs` dispatches, and the smallest
// possible example of hosting `shape.slang`.
//
// `shape.slang` declares no bindings, so a host decides its own numbers — a
// consumer can keep set 0 for its own buffers and bind the shaper's in a set
// of their own. Hosting the module is this file: bind the buffers, gather the
// handles into a `ShapeTables`, and declare one entry point per stage, each
// calling its stage once per thread. Nothing else.
//
// The font is one arena of verbatim OpenType bytes, so a host binds **two**
// buffers for it: the arena and the strings. The stages hand a string's
// working state from one dispatch to the next through a third, the scratch
// buffer: one `ShapeScratch` per request.
//
// A host dispatches the four entries in order over the same batch:
// `shape_decode`, `shape_substitute`, `shape_position`, `shape_finish`
// (`punchcutter_shape::STAGE_ENTRIES`). Each is its own pipeline, so a driver
// compiles four smaller programs rather than one large one, and a host can
// build them in parallel.
//
// It is also what the reflection test compiles: `shape/tests/layout.rs` reflects
// it and compares Slang's own per-field byte offsets against `offset_of!` on
// the Rust side, which is how Slang's layout of `shape_abi.slang` is kept equal
// to `shape/src/abi.rs`.

import shape;

[[vk::binding(0, 0)]]
ConstantBuffer<ShapeParams> params;

[[vk::binding(1, 0)]]
StructuredBuffer<ShapeRequest> requests;

/// The arena: the header, the face directory and the faces' five tables, exactly
/// as they sit in the font files.
[[vk::binding(2, 0)]]
StructuredBuffer<uint> ot;

// Read-write because `ShapeTables.utf8` is: see the note on that field.
[[vk::binding(3, 0)]]
RWStructuredBuffer<uint> utf8;

[[vk::binding(4, 0)]]
RWStructuredBuffer<ShapedGlyph> out_glyphs;

[[vk::binding(5, 0)]]
RWStructuredBuffer<ShapeRun> out_runs;

/// One `ShapeScratch` per request: `punchcutter_shape::scratch_bytes`.
[[vk::binding(6, 0)]]
RWStructuredBuffer<ShapeScratch> scratch;

/// The handles, gathered. The module needs no bindings of its own, which is
/// the whole point of `ShapeTables`.
ShapeTables tables()
{
    ShapeTables t;
    t.ot = ot;
    // One buffer, three times: a host with a single strings buffer sets all
    // three fields to it and leaves `source` at 0. A host whose text lives in
    // two or three buffers is what the selector is for.
    t.utf8 = utf8;
    t.utf8_alt0 = utf8;
    t.utf8_alt1 = utf8;
    t.source = 0u;
    t.scratch = scratch;
    return t;
}

// One thread, one string, in every stage. `SHAPE_WORKGROUP` in
// `shape/src/abi.rs` mirrors the 64 below; `Batch::workgroups` divides by it.
// The scratch slot is the request's index.

[shader("compute")]
[numthreads(64, 1, 1)]
void shape_substitute(uint3 gid: SV_DispatchThreadID)
{
    if (gid.x < params.request_count)
    {
        shapeSubstitute(tables(), params, requests[gid.x], gid.x);
    }
}

[shader("compute")]
[numthreads(64, 1, 1)]
void shape_position(uint3 gid: SV_DispatchThreadID)
{
    if (gid.x < params.request_count)
    {
        shapePosition(tables(), params, requests[gid.x], gid.x);
    }
}

[shader("compute")]
[numthreads(64, 1, 1)]
void shape_finish(uint3 gid: SV_DispatchThreadID)
{
    if (gid.x < params.request_count)
    {
        out_runs[gid.x] = shapeFinish(tables(), params, requests[gid.x], gid.x, out_glyphs,
                                      params.glyph_capacity);
    }
}

/// The device clock's rate, measured by the host, so that a run's wall-clock
/// deadline (`ShapeParams::deadline_ticks`) can be stated in ticks.
///
/// `getRealtimeClock` is a **wall clock at a vendor-fixed rate**, and no Vulkan
/// limit reports what that rate is — `timestampPeriod` describes the *timestamp*
/// clock, which is a different one. So the host dispatches this twice a known
/// wall-clock interval apart and divides: one thread, one clock read, one word.
///
/// It is here rather than in `shape.slang` because it is the host's question
/// and not the shaper's; `ShapeParams::deadline_ticks` is the answer arriving.
[shader("compute")]
[numthreads(1, 1, 1)]
void clock_probe(uint3 gid: SV_DispatchThreadID)
{
    if (gid.x != 0u)
    {
        return;
    }
    ShapeRun r;
    r.glyph_count = getRealtimeClockLow();
    r.advance = 0;
    r.flags = 0u;
    r.depth = 0u;
    out_runs[0] = r;
}
