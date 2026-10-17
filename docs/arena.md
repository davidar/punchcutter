# The buffers

This document is the byte-level contract between the CPU side of the engine
(the `punchcutter-text`, `punchcutter-shape` and `punchcutter-fonts` crates)
and the three Slang modules a consumer's shader imports
(`slang/modules/text.slang`, `slang/modules/shape.slang` and
`slang/modules/ot.slang`, which `shape.slang` uses). The CPU fills buffers,
the consumer uploads them unchanged, and the modules read them. The CPU does
not interpret font tables, and the GPU does not trust them.

There are three groups of buffers:

- **The curve buffers.** Four storage buffers holding every glyph's quadratic
  Béziers and Slug's band tables. `text.slang` declares them in descriptor
  set 1.
- **The arena.** One storage buffer of 32-bit words holding a face directory
  followed by every face's `cmap`, `hmtx`, `GDEF`, `GSUB` and `GPOS` tables,
  copied byte for byte from the font file. `shape.slang` reads it through
  `ShapeTables`, which declares no bindings.
- **A shaping dispatch.** The parameters, the requests, the UTF-8 bytes and
  the two output arrays.

The Rust declarations are the source of truth: `text/src/layout.rs` for the
curve buffers and `shape/src/abi.rs` for everything else. Both are checked
against Slang's reflection by `text/tests/layout.rs` and
`shape/tests/layout.rs`.

## Layout rules

All layouts are std430. A struct that contains a vector is 16-byte aligned.
Every scalar field is a `uint`, an `int` or a `float`, so every bit pattern is
valid and the plain byte views in `punchcutter_core::plain` are sound.

**No 3-vectors.** std430 aligns them to 16 bytes, and Slang's reflection has
placed one at the wrong offset. Reserved space is a run of scalar words named
`_rsvdN`, written as zero and read by neither side.

Everything is little-endian except the OpenType bytes inside the arena, which
are big-endian because the format is.

Both halves are built from one source tree, so the layout does not need to
stay binary-compatible across engine versions. `SHAPE_ABI_VERSION` is a label
for anyone reading a buffer dump, not a negotiation.

## The curve buffers

A shader that imports `text` gets these in descriptor set 1, so it keeps set 0
for itself and adds one bind group:

| binding | Slang | Rust | element |
|---|---|---|---|
| 0 | `textPoints` | `Point` | 16 B |
| 1 | `textBands` | `Band` | 8 B |
| 2 | `textBandCurves` | `CurveRef` | 4 B |
| 3 | `textGlyphs` | `Glyph` | 64 B |

**Points.** `{ float2 p; float2 c; }` in em units, y up, origin at the
glyph's pen. Quadratic curve `i` is `{points[i].p, points[i].c,
points[i + 1].p}`, so a contour of `n` curves takes `n + 1` entries and
neighbouring curves share endpoints. A contour is always a contiguous run, and
its last entry carries only a point.

**Bands.** `{ uint count; uint first; }`: how many curves cross the band, and
where their indices start in the band-curves buffer. A glyph's horizontal
bands come first, then its vertical ones. Within a band the curve indices are
sorted by descending maximum x (horizontal bands) or y (vertical bands). This
ordering is what lets the per-pixel walk stop early.

**Band curves.** One `uint` per entry: the index in the points buffer of a
curve that some band lists.

**Glyphs.** One 64-byte record per glyph id:

| offset | field | |
|---|---|---|
| 0 | `band_base` | index of the glyph's first horizontal band record |
| 4 | `hband_count` | horizontal bands; zero means no ink |
| 8 | `vband_count` | vertical bands; zero means no ink |
| 12 | `flags` | `FILL_EVEN_ODD = 1`; clear is non-zero winding |
| 16 | `band_transform` | `float4`: `(1/band w, 1/band h, -min x/band w, -min y/band h)` |
| 32 | `bounds` | `float4`: ink box over the control points, em units |
| 48 | `advance` | horizontal advance, em units |
| 52 | `_rsvd0..2` | zero |

The glyph id is the record index. A font's glyphs occupy ids
`0..glyph_count` in `gid` order with no gaps. Anything merged later (a
fallback face, a caller's own outline) is appended after them.

`text.slang` bounds-checks every index against the buffer it was given. A
glyph id past the last record, or a record in unwritten headroom, draws
nothing.

## The arena

One `StructuredBuffer<uint>`, laid out as:

```text
byte 0      OtHeader                  16 B
byte 16     OtFace * MAX_OT_FACES     16 * 64 B, the directory, fixed so it never moves
byte 1040   table bytes, 4-aligned, appended face by face
            fill bitmaps, one per face that needs one, beside its tables
```

`OT_TABLES_START` is 1040. Every table offset is at or past it.

**`OtHeader`**, 16 bytes:

| offset | field | |
|---|---|---|
| 0 | `version` | `SHAPE_ABI_VERSION` |
| 4 | `face_count` | faces in the directory, at most `MAX_OT_FACES` (16) |
| 8 | `arena_bytes` | bytes in use, including the front block |
| 12 | `_rsvd0` | zero |

**`OtFace`**, 64 bytes, one per face:

| offset | field | |
|---|---|---|
| 0 | `glyph_base` | the first merged glyph id this face owns; 0 for the primary |
| 4 | `glyph_count` | `maxp.numGlyphs`: the whole range the face reserved |
| 8 | `upem` | `head.unitsPerEm` for this face |
| 12 | `num_h_metrics` | `hhea.numberOfHMetrics` |
| 16..52 | `cmap_off`, `cmap_len`, `hmtx_off`, `hmtx_len`, `gdef_off`, `gdef_len`, `gsub_off`, `gsub_len`, `gpos_off`, `gpos_len` | byte offset from the arena's start (zero when the face lacks the table) and length |
| 56 | `filled_off` | byte offset of the fill bitmap |
| 60 | `flags` | `OT_FACE_ALL_FILLED = 1` |

The fill bitmap is `ceil(glyph_count / 32)` words, one bit per glyph in the
face, set when that glyph's curves are present in the curve buffers. A face
with `OT_FACE_ALL_FILLED` set (always true for the primary) has no bitmap and
its `filled_off` is not read.

`ot.slang` adds a table's offset to the subtable-relative offsets OpenType
uses, and bounds every read by `arena_bytes` clamped to the number of words
the driver actually bound. A read past that returns zero.

## The merged glyph space

One glyph-id space spans every face. A face opened into the arena reserves its
whole glyph range starting at `glyph_base`, so a merged id is `glyph_base +
face gid`. The face's `GSUB` and `GPOS` tables refer to its own gids, so they
work in the merged space by adding that one offset. As a result, a fallback
face kerns and substitutes within itself.

The curve records inside a reserved range are blank until a cover fills them.
A glyph whose fill bit is clear still shapes to its id, but comes back flagged
`SHAPE_GLYPH_UNFILLED` and raises `SHAPE_RUN_NOTDEF` on its run. That is the
consumer's signal to cover the string (`punchcutter_fonts::Merged::cover`) and
shape it again.

The shaper assigns a codepoint to the first face in the directory whose
`cmap` has it, so the directory order is the fallback order. A string is cut
into maximal single-face segments, and each segment is shaped with its face's
tables and scaled by its face's `upem`.

## Keeping a GPU copy current

Everything on the CPU side only grows, with two exceptions, so a consumer that
keeps GPU copies can bring them up to date after a cover with a few writes
rather than a full upload:

- **Points, bands, band curves.** Append only. Write the tail past what the
  copy already holds.
- **Glyphs.** Append, plus the records a cover filled in place inside a range
  reserved earlier. `CurveSet::take_rewritten` lists them, sorted and
  deduplicated. Write the tail first, then every listed record below the old
  length. The records must go after the tails because a record refers to bands
  and curves that arrive in the tails.
- **The arena.** Three parts: the front block (bytes `0..OT_TABLES_START`),
  rewritten whenever a face is opened; each face's fill bitmap
  (`Arena::bitmap_range`), rewritten whenever a glyph is filled; and the tail.
- **`ShapeFont`.** Rewrite it whole (16 bytes). Its `glyph_count` follows the
  record table.

Write the curve buffers before anything that refers to a glyph. If the front
block arrives first, the shaper can be pointed at records the copy does not
have yet. The test
`a_copy_kept_by_tails_and_rewritten_records_stays_whole` in
`fonts/src/tests.rs` exercises this procedure.

## A shaping dispatch

`shape_demo.slang` is the smallest host. Its bindings are one layout among
many possible ones:

| binding | | |
|---|---|---|
| 0 | `ConstantBuffer<ShapeParams>` | the dispatch's scalars |
| 1 | `StructuredBuffer<ShapeRequest>` | one per string |
| 2 | `StructuredBuffer<uint>` | the arena |
| 3 | `RWStructuredBuffer<uint>` | the strings, UTF-8, four bytes a word, little-endian |
| 4 | `RWStructuredBuffer<ShapedGlyph>` | output glyphs |
| 5 | `RWStructuredBuffer<ShapeRun>` | one output run per request |

`ShapeTables` carries three strings buffers and a `source` selector, so a
host whose text lives in several buffers can choose one per call without
branching around the shaper. A host with one buffer sets all three to it. The
strings buffer is read-write only so that a host can compose bytes into it in
an earlier pass; the shaper never writes to it.

One thread shapes one string, in workgroups of `SHAPE_WORKGROUP` (64).
`punchcutter_shape::Batch` packs the strings, the requests and the feature
pool, and gives each string `min(bytes * 4 + 8, MAX_RUN_GLYPHS)` glyph slots.

### What a dispatch costs

A dispatch takes as long as its longest string, because that string is one
thread walking its lookups alone. A hundred short strings cost about the same
as one long one.

So shape a string when it changes, not every frame. A run's output stays
valid in the output buffers until the string, its size or the font changes. A
host that draws every frame should keep the runs it already has and dispatch
only the requests that changed, which it can select through its own list of
indices rather than by rebuilding the batch. In the demo reel
(`fonts/examples/reel`), shaping all 107 strings took 35 ms a frame on a
laptop RTX 4060. Shaping only the strings being typed brought the whole frame
(shaping, typesetting and drawing) under 8 ms. The same applies to anything
done by one thread per string or per paragraph, such as line breaking.

A long dispatch has a second cost on an integrated GPU. The desktop is drawn
with the same GPU and cannot be while the dispatch runs, so the pointer
stutters. A host doing heavy work on an integrated GPU should submit it in
pieces of a few milliseconds each. docs/performance.md covers this and the
rest of frame budgeting.

**`ShapeFont`**, 16 bytes: `upem`, `glyph_count` (records in the merged
space, which bounds every id the shader writes), and the primary face's
`hhea` `ascender` and `descender` (signed, design units).

**`ShapeParams`**, 96 bytes:

| offset | field | |
|---|---|---|
| 0 | `font` | `ShapeFont` |
| 16 | `request_count` | strings in this dispatch |
| 20 | `glyph_capacity` | slots in the output array |
| 24 | `version` | `SHAPE_ABI_VERSION` |
| 28 | `deadline_ticks` | device-clock ticks one run may spend; zero disables |
| 32 | `features` | `uint4[4]`: up to `MAX_SHAPE_FEATURES` (16) packed tags |

A feature word is an OpenType tag packed big-endian, with `SHAPE_FEATURE_ON`
(bit 31) set to force the feature on and clear to force it off. Zero is an
empty slot. The array is `uint4` because a constant buffer gives a scalar
array a 16-byte stride.

**`ShapeRequest`**, 32 bytes: `byte_start` and `byte_len` into the strings,
`px_size` (device pixels per em), `out_start` and `out_cap` (the run's
slots), `feature_start` and `feature_count` (its slice of the feature pool),
and a reserved word.

**`ShapedGlyph`**, 32 bytes:

| offset | field | |
|---|---|---|
| 0 | `gid` | merged glyph id; draw it with `textCoverage` |
| 4 | `x` | pen position, 26.6 fixed point, GPOS offset included |
| 8 | `y` | the same for y: a mark's attachment offset, or zero |
| 12 | `cluster` | byte offset of the first codepoint the glyph came from |
| 16 | `flags` | `SHAPE_GLYPH_*` |
| 20 | `_rsvd0..2` | zero |

**`ShapeRun`**, 16 bytes: `glyph_count`, `advance` (26.6), `flags`
(`SHAPE_RUN_*`) and `depth` (the deepest nested lookup the run reached).

Positions accumulate in design units and are converted to 26.6 once per glyph
at the request's size, rounding half away from zero as HarfBuzz does. This is
why the results match HarfBuzz exactly rather than within a rounding error.

### Run flags

| bit | | |
|---|---|---|
| 0 | `SHAPE_RUN_NOTDEF` | a codepoint had no glyph, or one whose curves have not arrived: cover the string |
| 1 | `SHAPE_RUN_TRUNCATED` | the run needed more slots than it was given |
| 2 | `SHAPE_RUN_BAD_UTF8` | the decoder met bytes it could not read |
| 3 | `SHAPE_RUN_SUBSTITUTED` | a GSUB lookup changed the run |
| 4 | `SHAPE_RUN_POSITIONED` | a GPOS lookup moved something |
| 5 | `SHAPE_RUN_OUT_OF_OPS` | the operation budget (`MAX_OPS`) ran out |
| 6 | `SHAPE_RUN_MULTI_FACE` | the string used more than one face |
| 7 | `SHAPE_RUN_UNSUPPORTED` | a format or version the shader does not walk; drawn anyway |
| 8 | `SHAPE_RUN_DEADLINE` | the run passed `deadline_ticks` of device wall clock |

### Glyph flags

| bit | | |
|---|---|---|
| 0 | `SHAPE_GLYPH_NOTDEF` | `.notdef` standing in for a codepoint no face has |
| 1 | `SHAPE_GLYPH_SUBSTITUTED` | a GSUB lookup put it here |
| 2 | `SHAPE_GLYPH_POSITIONED` | a GPOS lookup moved it |
| 3 | `SHAPE_GLYPH_IGNORABLE` | a default-ignorable codepoint, hidden as a zero-width space |
| 4 | `SHAPE_GLYPH_LIGATURE` | two or more input glyphs became it |
| 5 | `SHAPE_GLYPH_MARK` | GDEF calls it a mark |
| 6 | `SHAPE_GLYPH_UNFILLED` | its curves have not arrived; it draws blank |

## Bounded in time

The arena comes from a file the engine does not control, so the shader treats
it as hostile. Every list a table declares is capped (`MAX_LIST`,
`MAX_LOOKUPS`, `MAX_NESTING`, `MAX_CONTEXT`) and charged against a per-string
operation budget (`MAX_OPS`) before its first iteration, and every read is
clamped to the bytes bound.

On a device with `VK_KHR_shader_clock`, each run also has a wall-clock
deadline. `punchcutter_core::gpu::open_device` enables the extension when the
adapter supports it. A host sets `deadline_ticks` either by measuring the
clock rate (see `clock_probe` in `shape_demo.slang`) or by using
`SHAPE_DEADLINE_TICKS`.

A malformed font draws wrong, but it does not hang the GPU.
`shape/examples/garbage.rs` measures what the bounds cost.

## Declared once

Every struct and constant above is written once, in Rust, using the
`abi_struct!` and `abi_consts!` macros in `punchcutter_core::decl`. Each macro
emits the Rust item and a table describing it. Running
`cargo run -p punchcutter-shape --bin gen-slang` renders those tables into
`slang/modules/shape_abi.slang`, which is checked in because Slang resolves an
import relative to the importing file's directory.

A unit test fails when the file on disk differs from the rendering, and the
layout test compares every bound struct's offsets with Slang's reflection.
Never edit `shape_abi.slang` by hand.

The curve buffers are the exception: they are mirrored by hand in
`text.slang` and checked by the same kind of reflection test.
