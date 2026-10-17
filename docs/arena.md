# The buffers

This is the byte-level contract between the CPU side of the engine (the
`punchcutter-text`, `punchcutter-shape` and `punchcutter-fonts` crates) and
the three Slang modules a consumer's shader imports (`slang/modules/text.slang`,
`slang/modules/shape.slang`, and `slang/modules/ot.slang` beneath it). The CPU fills buffers;
the consumer uploads them as they are; the modules read them. Nothing on the
CPU side interprets a font table, and nothing on the GPU side trusts one.

There are three groups:

- **The curve buffers**: four storage buffers holding every glyph's quadratic
  Béziers and Slug's band tables. `text.slang` declares them, in descriptor
  set 1.
- **The arena**: one storage buffer of 32-bit words holding a face directory
  and, behind it, every face's `cmap`, `hmtx`, `GDEF`, `GSUB` and `GPOS`
  exactly as they sit in the font file. `shape.slang` reads it through
  `ShapeTables`, which declares no bindings of its own.
- **A shaping dispatch**: the parameters, the requests, the UTF-8 bytes, and
  the two output arrays.

The Rust declarations are the source of truth: `text/src/layout.rs` for the
curve buffers and `shape/src/abi.rs` for the rest. Both are pinned against
Slang's own reflection by `text/tests/layout.rs` and `shape/tests/layout.rs`.

## Layout rules

std430. A struct that carries a vector is 16-byte aligned. Every scalar field
is a `uint`, an `int` or a `float`, so every bit pattern is valid and the plain
byte views in `punchcutter_core::plain` are sound. **No 3-vectors**: std430
aligns one to 16 bytes and Slang's reflection has been seen to put one at the
wrong offset, so a reserved tail is a run of scalar words named `_rsvdN`,
written as zero and read by neither side. Little-endian throughout, except the
OpenType bytes inside the arena, which are big-endian because the format is.

Both halves are built from one tree, so the layout has no binary stability to
keep across versions of the engine. `SHAPE_ABI_VERSION` is a label for whoever
reads a buffer dump, not a negotiation.

## The curve buffers

Set 1 of any shader that imports `text`, so the importer keeps its own set 0
and adds one bind group:

| binding | Slang | Rust | element |
|---|---|---|---|
| 0 | `textPoints` | `Point` | 16 B |
| 1 | `textBands` | `Band` | 8 B |
| 2 | `textBandCurves` | `CurveRef` | 4 B |
| 3 | `textGlyphs` | `Glyph` | 64 B |

**Points.** `{ float2 p; float2 c; }`, em units, y up, origin at the glyph's
pen. Quadratic curve `i` is `{points[i].p, points[i].c, points[i + 1].p}`, so a
contour's `n` curves cost `n + 1` entries and neighbours share endpoints. A
contour is always a contiguous run; its last entry carries only a point.

**Bands.** `{ uint count; uint first; }`: how many curves cross the band, and
where their indices start in the band-curves buffer. A glyph's horizontal
bands come first, then its vertical ones. Within a band the curve indices are
sorted by descending maximum x (horizontal) or y (vertical), which is what
lets the per-pixel walk stop early.

**Band curves.** One `uint` per entry: the index into the points buffer of a
curve some band lists.

**Glyphs.** One record per glyph id, 64 bytes:

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

The glyph id **is** the record index. A font's glyphs occupy ids
`0..glyph_count` in `gid` order with no gaps; anything merged later (a
fallback face, a caller's own outline) is appended after them.

`text.slang` bounds every index it reads against the buffer it was given, so a
glyph id past the last record, or a record read out of unwritten headroom,
draws nothing rather than reading garbage.

## The arena

One `StructuredBuffer<uint>`:

```text
byte 0      OtHeader                  16 B
byte 16     OtFace * MAX_OT_FACES     16 * 64 B, the directory, fixed so it never moves
byte 1040   table bytes, 4-aligned, appended face by face
            fill bitmaps, one per face that needs one, beside its tables
```

`OT_TABLES_START` is 1040: every table offset is at or past it.

**`OtHeader`**, 16 bytes:

| offset | field | |
|---|---|---|
| 0 | `version` | `SHAPE_ABI_VERSION` |
| 4 | `face_count` | faces in the directory, at most `MAX_OT_FACES` (16) |
| 8 | `arena_bytes` | bytes in use, the front block included |
| 12 | `_rsvd0` | zero |

**`OtFace`**, 64 bytes, one per face:

| offset | field | |
|---|---|---|
| 0 | `glyph_base` | the first merged glyph id this face owns; 0 for the primary |
| 4 | `glyph_count` | `maxp.numGlyphs`: the whole range the face reserved |
| 8 | `upem` | `head.unitsPerEm`, this face's own |
| 12 | `num_h_metrics` | `hhea.numberOfHMetrics` |
| 16..52 | `cmap_off`, `cmap_len`, `hmtx_off`, `hmtx_len`, `gdef_off`, `gdef_len`, `gsub_off`, `gsub_len`, `gpos_off`, `gpos_len` | byte offset from the arena's start (zero when the face lacks the table) and length |
| 56 | `filled_off` | byte offset of the fill bitmap |
| 60 | `flags` | `OT_FACE_ALL_FILLED = 1` |

The fill bitmap is `ceil(glyph_count / 32)` words, one bit per face glyph, set
when that glyph's curves are in the curve buffers. A face with
`OT_FACE_ALL_FILLED` (the primary, always) has none and its `filled_off` is
not read.

`ot.slang` adds a table's offset to the subtable-relative offsets OpenType
states, and bounds every read by `arena_bytes` clamped to the words the driver
actually bound. A read past that answers zero.

## The merged glyph space

One glyph-id space spans every face. A face opened into the arena reserves its
**whole** glyph range at `glyph_base`, so a merged id is `glyph_base + face
gid`, and the face's own `GSUB` and `GPOS`, which name its own ids, work in the
merged space by one addition. A fallback face therefore kerns and substitutes
with itself.

The curve records inside a reserved range are blank until a cover fills them.
A glyph whose fill bit is clear shapes to its id, comes back flagged
`SHAPE_GLYPH_UNFILLED`, and raises `SHAPE_RUN_NOTDEF` on its run, which is the
consumer's signal to cover the string (`punchcutter_fonts::Merged::cover`) and
shape it again.

The shaper picks a codepoint's face as the first in the directory whose `cmap`
has it, so the directory's order is the fallback order. A string is cut into
maximal segments of one face, and each segment is shaped with its own face's
tables and scaled by its own face's `upem`.

## Keeping a GPU copy current

Everything the CPU side holds only grows, with two exceptions, so a consumer
that keeps GPU copies brings them up to date after a cover with a handful of
writes and never a full upload:

- **Points, bands, band curves**: append only. Write the tail past what the
  copy holds.
- **Glyphs**: append, plus the records a cover filled **in place** inside a
  range reserved earlier. `CurveSet::take_rewritten` names them (sorted,
  once each). Write the tail, then every named record below the old length.
  The records go after the tails because a record names bands and curves that
  arrive in the tails.
- **The arena**: the front block (bytes `0..OT_TABLES_START`), rewritten
  whenever a face is opened; each face's fill bitmap
  (`Arena::bitmap_range`), rewritten whenever a glyph is filled; and the tail.
- **`ShapeFont`**: rewrite it whole (16 bytes); its `glyph_count` follows the
  record table.

Write the curve buffers before anything that names a glyph: a front block
that arrives first points the shaper at records the copy does not have yet.
`fonts/src/tests.rs`, `a_copy_kept_by_tails_and_rewritten_records_stays_whole`,
is this procedure as a test.

## A shaping dispatch

`shape_demo.slang` is the smallest host, and its bindings are one choice among
many:

| binding | | |
|---|---|---|
| 0 | `ConstantBuffer<ShapeParams>` | the dispatch's scalars |
| 1 | `StructuredBuffer<ShapeRequest>` | one per string |
| 2 | `StructuredBuffer<uint>` | the arena |
| 3 | `RWStructuredBuffer<uint>` | the strings, UTF-8, four bytes a word, little-endian |
| 4 | `RWStructuredBuffer<ShapedGlyph>` | output glyphs |
| 5 | `RWStructuredBuffer<ShapeRun>` | one output run per request |

`ShapeTables` carries three strings buffers and a `source` selector so a host
whose text lives in several buffers can choose per call without branching
around the shaper; a host with one buffer sets all three to it. The strings
buffer is read-write only so that a host may compose bytes into it in an
earlier pass; the shaper never writes it.

One thread shapes one string, in workgroups of `SHAPE_WORKGROUP` (64).
`punchcutter_shape::Batch` packs the strings, the requests and the feature
pool, and gives each string `min(bytes * 4 + 8, MAX_RUN_GLYPHS)` glyph slots.

**`ShapeFont`**, 16 bytes: `upem`, `glyph_count` (records in the merged space,
the bound on every id the shader writes), and the primary's `hhea`
`ascender` and `descender` (signed, design units).

**`ShapeParams`**, 96 bytes:

| offset | field | |
|---|---|---|
| 0 | `font` | `ShapeFont` |
| 16 | `request_count` | strings in this dispatch |
| 20 | `glyph_capacity` | slots in the output array |
| 24 | `version` | `SHAPE_ABI_VERSION` |
| 28 | `deadline_ticks` | device-clock ticks one run may spend; zero disables |
| 32 | `features` | `uint4[4]`: up to `MAX_SHAPE_FEATURES` (16) packed tags |

A feature word is an OpenType tag packed big-endian with `SHAPE_FEATURE_ON`
(bit 31) set to force it on and clear to force it off; zero is an empty slot.
It is a `uint4` array because a constant buffer gives a scalar array a 16-byte
stride.

**`ShapeRequest`**, 32 bytes: `byte_start` and `byte_len` into the strings,
`px_size` (device pixels per em), `out_start` and `out_cap` (the run's slots),
`feature_start` and `feature_count` (its slice of the feature pool), and a
reserved word.

**`ShapedGlyph`**, 32 bytes:

| offset | field | |
|---|---|---|
| 0 | `gid` | merged glyph id: draw it with `textCoverage` |
| 4 | `x` | pen position, 26.6 fixed point, GPOS offset included |
| 8 | `y` | the same on y: a mark's attachment, or zero |
| 12 | `cluster` | byte offset of the first codepoint the glyph came from |
| 16 | `flags` | `SHAPE_GLYPH_*` |
| 20 | `_rsvd0..2` | zero |

**`ShapeRun`**, 16 bytes: `glyph_count`, `advance` (26.6), `flags`
(`SHAPE_RUN_*`) and `depth` (the deepest nested lookup the run reached).

Positions accumulate in design units and are converted to 26.6 once per glyph
at the request's size, rounding half away from zero as HarfBuzz does, so the
result matches HarfBuzz exactly rather than within a rounding.

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

The arena is somebody else's file, so the shader treats it as hostile. Every
list it states is capped (`MAX_LIST`, `MAX_LOOKUPS`, `MAX_NESTING`,
`MAX_CONTEXT`) and charged against a per-string operation budget (`MAX_OPS`)
before its first iteration, and every read is clamped to the bytes bound. On a
device with `VK_KHR_shader_clock` each run also holds a wall-clock deadline:
`punchcutter_core::gpu::open_device` enables the extension where the adapter
has it, and a host sets `deadline_ticks` either by measuring the clock's rate
(`shape_demo.slang`'s `clock_probe`) or to `SHAPE_DEADLINE_TICKS`. A malformed
font draws wrong; it does not hang the GPU. `shape/examples/garbage.rs`
measures what that costs.

## Declared once

Every struct and constant above is written once, in Rust, through the
`abi_struct!` and `abi_consts!` macros in `punchcutter_core::decl`. Each emits
the Rust item and a table describing it; `cargo run -p punchcutter-shape --bin
gen-slang` renders the tables into `slang/modules/shape_abi.slang`, which is checked
in because Slang resolves an import from the importing file's directory. A
unit test fails when the file on disk differs from the rendering, and the
layout test compares every bound struct's offsets with Slang's reflection.
Never edit `shape_abi.slang` by hand. The curve buffers are mirrored by hand
in `text.slang` and pinned by the same kind of reflection test.
