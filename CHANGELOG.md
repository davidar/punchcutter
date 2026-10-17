# Changelog

## Unreleased

What exists before the first tagged release.

**Rendering** (`punchcutter-text`, `slang/modules/text.slang`). Glyph
coverage computed from the font's outlines by the Slug algorithm. `glyf`
quadratics and split CFF cubics are flattened once per font into four storage
buffers with Slug's band tables, then walked per pixel at any size with an
analytic filter. There is no atlas, rasteriser or hinting. An SVG's filled
paths load into the same buffers, so an icon is a glyph with a name and draws
through the same coverage function. The band tables are cached on disk, keyed
by the font's content and the layout version. A headless harness compares
every glyph against swash's CPU raster and writes PNGs.

**Shaping** (`punchcutter-shape`, `slang/modules/shape.slang`,
`slang/modules/ot.slang`). HarfBuzz's default shaper on the GPU, one thread
per string: `cmap` formats 0, 4, 6, 12 and 13, `GDEF` classes and lookup
flags, `GSUB` types 1 to 8, `GPOS` types 1 to 9, feature selection by script
and language with user features forced on or off, clusters,
default-ignorables, and 26.6 positions matching HarfBuzz to the unit. The
font's tables reach the GPU unparsed, copied into one arena. Every list is
capped and charged to an operation budget, every read is bounded, and each run
has a device wall-clock deadline where `VK_KHR_shader_clock` is available. A
conformance runner compares against harfrust 0.12.0 with zero tolerance over a
user-interface corpus and HarfBuzz's shaping tests. A hostile-arena harness
measures the cost of the bounds.

**Fonts** (`punchcutter-fonts`). A UI font found through fontconfig (or
`$PUNCHCUTTER_FONT`), required to carry the characters a consumer composes
itself. A fallback chain in fontconfig's order: a face that draws a missing
codepoint is opened into the arena with its whole glyph range in one merged
glyph space, so it kerns and substitutes with its own tables, and the `GSUB`
closure of the glyphs it filled is filled along with them. Per-face caches of
the filled glyphs.

**The compiler** (`punchcutter-slang`). Slang in process, embedded in the
binary through a fork of the `shader-slang` crate, so a build script, a test
and a program that recompiles at run time all compile the same way with
nothing installed. One call compiles every entry point of a shader and
reflects the program: bindings, struct offsets and the `getStringHash` table,
as plain data with a JSON form for baking beside the SPIR-V. The four Slang
modules are compiled into the crate (`modules`) and are on every compile's
search path. The compiler is the `compiler` feature, on by default. Without
it the crate contains only the types and the modules.

**Machinery** (`punchcutter-core`). Plain-bytes upload views, the
declare-once macros that generate `slang/modules/shape_abi.slang`, and wgpu
helpers including a device opened with `VK_KHR_shader_clock`. Std-only, so a
build script can depend on it for the ABI alone.

**The demo reel** (`punchcutter-fonts`, `examples/reel`). A video rendered
entirely by the engine: shaping, line breaking, justification, column
balancing, coverage and compositing in three compute passes per frame. Frames
are written to stdout as raw RGBA for an encoder, or played in a window in
real time with the `live` feature.
