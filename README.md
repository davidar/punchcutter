# punchcutter

https://github.com/user-attachments/assets/496f2b47-ef36-44e0-9432-cca2ab3cab32

punchcutter is a GPU text engine written in Slang compute shaders. It shapes
text with OpenType `GSUB` and `GPOS`, matching HarfBuzz's default shaper, and
renders glyphs directly from the font's quadratic curves using the Slug
algorithm. There is no glyph atlas, rasteriser or bitmap cache: each glyph is
evaluated from its curves every frame, at whatever size it is drawn. The CPU
only parses fonts into GPU buffers and finds them through fontconfig.

The name refers to the craftsman who cut letterforms into steel type punches
by hand.

## Overview

### GPU side

Three Slang modules in `slang/modules/`:

- `text.slang` is glyph coverage. Given a glyph id, a sample position and the
  size in pixels per em, it returns coverage in `[0, 1]`, computed from the
  glyph's Bézier curves with two rays and an analytic filter. It is a port of
  Eric Lengyel's Slug reference pixel shader; the file header has the credit.
- `shape.slang` is the shaper, one thread per string. It handles UTF-8
  decoding, `cmap` lookup per face, feature selection by script and language,
  `GSUB` lookup types 1 to 8, `GPOS` types 1 to 9, `GDEF` classes and lookup
  flags, mark and cursive attachment, cluster tracking and default-ignorables.
  Output is glyph ids and 26.6 fixed-point pen positions. On the test corpora
  below the results match HarfBuzz exactly.
- `ot.slang` is a reader for OpenType's big-endian binary tables, used by
  `shape.slang`. Tables are read in place; nothing is converted on the CPU.

### CPU side

Five Rust crates:

| crate | what it does | main exports |
|---|---|---|
| `punchcutter-text` | a font's outlines, or an SVG's filled paths, into quadratic curves and Slug's band tables; the curve cache file; a face's `GSUB` closure | `Font`, `icon::load`, `Builder`, `CurveSet`, `Cached`, `font::codepoint_curves`, `closure::gsub_closure` |
| `punchcutter-shape` | the five layout tables copied unparsed from the font file into one arena; the batch of strings for a dispatch | `Arena`, `Batch`, `Feature`, the `abi` structs and flags |
| `punchcutter-fonts` | fontconfig, the cache directory, and the fallback chain that merges other faces into the same glyph space | `Loaded`, `Merged`, `fallback::Chain` |
| `punchcutter-slang` | the Slang modules themselves, and the compiler that builds a shader against them in process: SPIR-V, reflection (bindings, struct offsets, the `getStringHash` table); the compiler is the default `compiler` feature | `compile`, `compile_program`, `reflect`, `Program`, `Reflection`, `modules` |
| `punchcutter-core` | plain-bytes views, the declare-once ABI macros, wgpu helpers (`gpu` feature); std-only | `plain`, `decl`, `gpu::open_device` |

The CPU never interprets a lookup, a coverage table or a subtable format.
Adding a new lookup type means editing a shader, not writing a second
implementation.

## Using it

A consumer uploads two sets of buffers and imports two modules.

**Curve buffers.** Four read-only storage buffers: points, bands, band curves
and glyph records. `text.slang` declares them in descriptor set 1, leaving set
0 for the consumer's own bindings. A glyph id is the index of its record.

**The arena.** One word buffer containing a 1040-byte directory followed by
each face's `cmap`, `hmtx`, `GDEF`, `GSUB` and `GPOS` tables, copied byte for
byte from the font file. `shape.slang` declares no bindings. The consumer
binds the arena, the requests, the UTF-8 bytes and two output arrays wherever
it likes and passes the handles in a `ShapeTables` struct.

Fallback faces share one merged glyph space. Each fallback face reserves a
contiguous glyph range, so its tables map to merged ids by adding an offset,
and it kerns and substitutes within itself. docs/arena.md is the byte-level
contract: every struct, offset and flag, how fallback faces join, and how to
update a GPU copy incrementally instead of re-uploading everything.

**Compiling shaders.** `punchcutter-slang` bundles the modules and the Slang
compiler. `punchcutter_slang::compile_program` compiles a shader's entry
points to SPIR-V in process, with `text.slang`, `shape.slang` and their
imports on the search path, so a program always runs against the engine
version it was built with and nothing needs to be installed. It works from a
build script, a test or a program that recompiles at run time, and returns the
program's reflection: bindings, struct field offsets and the string literal
behind each `getStringHash`. A program that only loads shaders baked by its
build script can depend on the crate with `default-features = false`, which
gives it the reflection types and their JSON form without linking Slang.

Loading a font and covering a string the primary face lacks:

```rust
use punchcutter_fonts::{Loaded, Merged};

let mut fonts = Merged::new(Loaded::load()?);
let [points, bands, band_curves, glyphs] = fonts.loaded.curve_bytes();
let arena = fonts.loaded.arena_bytes();
// ... upload the five buffers, shape a `punchcutter_shape::Batch` ...

// A run came back with SHAPE_RUN_NOTDEF: open the faces that cover it.
let cover = fonts.cover("日本語のウィンドウ");
// ... write the curve tails, the rewritten records and the arena's changes,
// then shape the run again.
```

Drawing a shaped glyph in Slang:

```slang
import text;

float coverage = textCoverage(glyph.gid,
    textSampleAt(pixelCentre, origin, pxPerEm), pxPerEm);
```

`slang/modules/text_demo.slang` and `slang/modules/shape_demo.slang` are
minimal complete hosts for each module.

## Building

Requirements:

- **Rust** 1.88 or later. `Cargo.lock` is committed; build with `--locked`.
- **Vulkan** with wgpu's `PASSTHROUGH_SHADERS`, for the examples and the GPU
  tests. The shaper uses `VK_KHR_shader_clock`, which
  `punchcutter_core::gpu::open_device` enables. Mesa's lavapipe works.
- **fontconfig** and at least one sans-serif font, for the tests that load a
  system font. Those tests skip if none is found.
- **Slang** does not need to be installed. `punchcutter-slang` embeds the
  pinned release (2026.14.1) through the `shader-slang` crate, whose build
  script downloads the release archive once and checks its SHA-256. Set
  `SLANG_RELEASE_ARCHIVE=<path>` to build offline from a prefetched copy.
  Binaries built this way include Slang's libraries under Slang's licence (see
  NOTICE). `spirv-val` from SPIRV-Tools is used if present.

```sh
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
```

If your system lacks the unversioned `.so` symlinks the linker expects, as
some immutable distributions do, point it at a directory that has them:

```sh
LIBRARY_PATH=/home/linuxbrew/.linuxbrew/lib cargo test --locked --workspace
```

`slang/modules/shape_abi.slang` is generated from `shape/src/abi.rs` and
checked in. After changing a declaration there, regenerate it. A test fails
until you do.

```sh
cargo run -p punchcutter-shape --bin gen-slang
```

## Examples

### render

```sh
cargo run -p punchcutter-text --example render
```

Renders a line of the system sans-serif at 16, 28 and 64 px, plus four
Adwaita symbolic icons at 16 and 32 px if that theme is installed. Every glyph
is compared pixel by pixel against swash's CPU rasterisation of the same
outlines, and glyph ids past the last record are checked to draw nothing. The
PNGs go to `text/test-runs/`.

The example runs headless and picks an integrated or software adapter,
refusing a discrete NVIDIA one, so it works on lavapipe. On a machine with two
GPUs it sets `VK_DRIVER_FILES` to the integrated GPU's driver if that variable
is unset.

### conform

```sh
cargo run -p punchcutter-shape --example conform
```

Shapes every test string twice, once with `shape.slang` and once with harfrust
0.12.0 (HarfBuzz's Rust port), and compares glyph ids, positions and clusters
in font units with zero tolerance. There are two corpora:

- **UI strings.** 80 strings of the kind a user interface shows: clocks, menu
  rows, settings labels, ligatures, kerning pairs, joiners and combining marks.
  They are shaped in Adwaita Sans, Noto Sans and Noto Sans Mono where
  installed, otherwise the system sans-serif, at 16, 28 and 32 px. `--ui-only`
  runs just this corpus.
- **HarfBuzz's shaping tests.** The `.tests` files under `test/shape/data` in
  <https://github.com/harfbuzz/harfbuzz>, fetched on first run with a sparse,
  blobless clone into `$XDG_CACHE_HOME/punchcutter/harfrust/hb-tests`. They
  are never committed, since the fonts carry their own licences. Cases that
  need features the shaper does not implement yet are skipped or excused by
  name and counted. `--tests-only` runs just this corpus.

Results on an integrated AMD GPU (RADV) against HarfBuzz revision 5365d27:

| corpus | compared | exact | mismatched | excused |
|---|---|---|---|---|
| UI strings, 3 fonts × 3 sizes | 720 | 711 | 0 | 9 |
| HarfBuzz's tests | 4160 | 4105 | 4 | 51 |

The 9 excused UI runs are three strings where harfrust's normaliser composes a
base and a combining mark into one glyph and this shaper does not yet, in both
Noto faces at each size. Of HarfBuzz's cases, 1264 were skipped because they
need a complex-script shaper, 124 need Unicode properties not yet on the GPU,
606 use an option outside the default shaper, and 49 were unreadable. The 51
excused cases involve AAT fonts, the legacy `kern` table, HarfBuzz's fallback
mark positioning, automatic fractions, the `rand` feature, the normaliser,
HarfBuzz's space fallback, and one run that hit `MAX_RUN_GLYPHS` or `MAX_OPS`.
The 4 mismatches are hostile-font cases newer than harfrust 0.12.0; see
docs/roadmap.md, section Conformance.

### reel

```sh
cargo run -p punchcutter-fonts --example reel | \
    ffmpeg -f rawvideo -pix_fmt rgba -s 1280x720 -r 60 -i - \
    -c:v libx264 -pix_fmt yuv420p -crf 18 reel.mp4
```

Produces about 30 seconds of video rendered entirely by the engine. It opens
on an ampersand's curves drawn over Slug's bands, with a probe showing the two
rays the coverage walk casts. The camera then pulls back to this README,
shaped, line-broken and justified into three columns on the GPU, and a lens
magnifies it per pixel. Finally a board of ligatures, kerning, small capitals,
old-style figures, fractions, mark attachment and a fallback face is typed
out, reshaped every frame.

Each frame is three compute passes in `fonts/examples/reel/reel.slang`:
shape, typeset and draw. The CPU only advances the clock and chooses what the
camera frames. After the first frame, only the strings currently being typed
are reshaped and reset.

Frames go to stdout as raw RGBA. Options:

- `--still <seconds> <file.png>` writes a single frame instead.
- `--live` plays the reel in a window, in real time and on a loop, copying
  each frame from the image buffer to the swapchain on the GPU. It needs the
  `live` feature, which is off by default so the tests never build a window
  system:

  ```sh
  cargo run -p punchcutter-fonts --features live --example reel -- --live --adapter discrete
  ```

- `--profile` prints each pass's GPU time, mean and worst, every two seconds,
  from timestamp queries that do not stall the frame.
- `--samples 1|4` sets the number of coverage samples per pixel.
- `--font <path>` overrides the font. The default is EB Garamond if fontconfig
  has it, otherwise Noto Serif.
- `--adapter discrete` picks a discrete GPU. By default the reel picks an
  adapter the same way the render example does. A discrete GPU is the better
  choice for a long render, since it leaves the GPU driving the desktop alone.
  On an integrated GPU the draw pass is submitted a strip of rows at a time so
  the desktop stays responsive.

docs/performance.md describes what it takes to hold 120 frames per second.

## Status

Not implemented yet: colour fonts, Unicode itemisation and bidi, the
normaliser, complex-script shapers (Arabic joining, Hangul, Indic and the
Universal Shaping Engine), and variable-font instances. docs/roadmap.md has
the full list and the planned order.

## Licence

MIT; see `LICENSE`. `NOTICE` credits the Slug reference shaders (MIT OR
Apache-2.0), which `slang/modules/text.slang` ports, and the Unicode data
whose ranges `slang/modules/shape.slang` writes out.

See CONTRIBUTING.md for how to contribute.
