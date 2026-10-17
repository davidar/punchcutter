# punchcutter

https://github.com/user-attachments/assets/496f2b47-ef36-44e0-9432-cca2ab3cab32

A GPU text engine. It shapes text (OpenType `GSUB` and `GPOS`, HarfBuzz's
default shaper) and renders glyphs (per-pixel coverage straight from the
font's own quadratic curves, by the Slug algorithm), both in Slang compute
shaders. The CPU side only parses a font into bytes for the GPU and finds
fonts through fontconfig. There is no atlas, no rasteriser and no bitmap
cache: every glyph is cut from its curves every frame, at whatever size and
scale it is drawn. The name is the craftsman who cut letterforms into steel
punches by hand.

The status bar in the caustics repository is its first consumer.

## What runs where

**On the GPU**, in three Slang modules under `slang/modules/`:

- `text.slang`: Slug's coverage walk. Given a glyph id, a sample position and
  a size in pixels per em, it returns coverage in `[0, 1]`, computed from the
  glyph's Béziers with two rays and an analytic filter. A port of Eric
  Lengyel's reference pixel shader; its header carries the credit.
- `shape.slang`: the shaper. One thread per string: UTF-8 decoding, `cmap`
  lookup per face, feature selection by script and language, `GSUB` lookup
  types 1 to 8, `GPOS` 1 to 9, `GDEF` classes and lookup flags, mark and
  cursive attachment, clusters, default-ignorables. Out come glyph ids and
  26.6 pen positions, matching HarfBuzz exactly.
- `ot.slang`, imported by `shape.slang`: a reader for OpenType's own
  big-endian binary format, walked in place.

**On the CPU**, in four Rust crates:

| crate | what it does | main exports |
|---|---|---|
| `punchcutter-text` | a font's outlines, or an SVG's filled paths, into quadratic curves and Slug's band tables; the curve cache file; a face's `GSUB` closure | `Font`, `icon::load`, `Builder`, `CurveSet`, `Cached`, `font::codepoint_curves`, `closure::gsub_closure` |
| `punchcutter-shape` | the five layout tables sliced out of the font file into one arena, unread; the batch of strings for a dispatch | `Arena`, `Batch`, `Feature`, the `abi` structs and flags |
| `punchcutter-fonts` | fontconfig, the cache directory, and the fallback chain that merges other faces into the same glyph space | `Loaded`, `Merged`, `fallback::Chain` |
| `punchcutter-slang` | the Slang modules themselves, and the compiler that builds a shader against them in process: SPIR-V, reflection (bindings, struct offsets, the `getStringHash` table); the compiler is the default `compiler` feature | `compile`, `compile_program`, `reflect`, `Program`, `Reflection`, `modules` |
| `punchcutter-core` | plain-bytes views, the declare-once ABI macros, wgpu helpers (`gpu` feature); std-only | `plain`, `decl`, `gpu::open_device` |

Nothing on the CPU reads a lookup, a coverage table or a subtable format: a
new lookup type is an edit to a shader, never a second implementation.

## The contract, briefly

A consumer uploads two things and imports two modules.

1. **The curve buffers**: four read-only storage buffers (points, bands,
   band curves, glyph records) that `text.slang` declares in descriptor set
   1, so the consumer's own bindings stay in set 0. A glyph id is the index
   of its record.
2. **The arena**: one word buffer holding a 1040-byte directory and, behind
   it, each face's `cmap`, `hmtx`, `GDEF`, `GSUB` and `GPOS` exactly as they
   sit in the font file. `shape.slang` declares no bindings; the consumer binds
   the arena, the requests, the UTF-8 bytes and two output arrays wherever it
   likes, and hands the handles over in a `ShapeTables`.

The two modules travel inside `punchcutter-slang`, and so does the
compiler: `punchcutter_slang::compile_program` compiles a shader's entry
points to SPIR-V in process, with `text.slang`, `shape.slang` and the two
`shape.slang` imports on its search path, so a program runs with the engine
it was built against, no checkout has to be where it was, and nothing has to
be installed. The same call serves a build script, a test and a program that
recompiles its shader at run time, and it comes back with the program's
reflection: every binding, every struct's field offsets, and the literal
behind every `getStringHash`. A program that only ever starts on modules
its build script baked takes the crate with `default-features = false`: the
reflection types and their JSON form, without Slang in its binary.

Every face shares one merged glyph space: a fallback face reserves its whole
glyph range, so its own tables index the merged ids by one addition and it
kerns and substitutes with itself. docs/arena.md is the byte-level contract:
every struct, offset and flag, how fallback faces join, and how to keep a GPU
copy current with tails and a few rewritten records rather than full uploads.

In Rust, loading a font and covering a string the primary lacks:

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

In Slang, drawing a shaped glyph:

```slang
import text;

float coverage = textCoverage(glyph.gid,
    textSampleAt(pixelCentre, origin, pxPerEm), pxPerEm);
```

`slang/modules/text_demo.slang` and `slang/modules/shape_demo.slang` are complete minimal
hosts for each module.

## Building

- **Slang** needs no installing: `punchcutter-slang` carries the pinned
  official release (2026.14.1) inside the binary, through the `shader-slang`
  crate, whose build downloads the release archive once and checks it against
  a pinned SHA-256 (`SLANG_RELEASE_ARCHIVE=<path>` builds from a prefetched
  copy, offline). A binary built this way carries Slang's libraries, under
  Slang's own licence (NOTICE, *Slang*). `spirv-val` from SPIRV-Tools is used
  when present.
- **Rust** 1.88 or later. `Cargo.lock` is committed; build with `--locked`.
- **Vulkan** with wgpu's `PASSTHROUGH_SHADERS` for the examples and the GPU
  tests. The shaper's SPIR-V uses `VK_KHR_shader_clock`, which
  `punchcutter_core::gpu::open_device` enables. Mesa's lavapipe works.
- **fontconfig** and at least one sans-serif font for the tests that load
  one; without them those tests skip rather than fail.

```sh
cargo check --locked --workspace --all-targets
cargo test --locked --workspace
```

On an immutable host whose image lacks the unversioned `.so` symlinks the
linker wants, point the linker at a directory that has them, for example
`LIBRARY_PATH=/home/linuxbrew/.linuxbrew/lib cargo test --locked --workspace`.

`slang/modules/shape_abi.slang` is generated from `shape/src/abi.rs` and checked in.
After changing a declaration, regenerate it; a test fails until you do:

```sh
cargo run -p punchcutter-shape --bin gen-slang
```

## The render example

```sh
cargo run -p punchcutter-text --example render
```

Renders a line of the system's sans-serif at 16, 28 and 64 px, and four
Adwaita symbolic icons at 16 and 32 px when that theme is installed, on the GPU,
compares every glyph pixel by pixel with swash's CPU raster of the same
outlines, and checks that glyph ids past the last record draw nothing. The
PNGs land in `text/test-runs/`. It runs headless, picks an integrated or
software adapter and refuses a discrete NVIDIA one, so it runs on lavapipe.
On a machine with two GPUs it pins `VK_DRIVER_FILES` to the integrated one's
driver when that variable is unset.

## The conformance runner

```sh
cargo run -p punchcutter-shape --example conform
```

Every string is shaped twice, once by `shape.slang` and once by harfrust
0.12.0 (HarfBuzz's Rust port), and every glyph id, position and cluster is
compared in font units with a tolerance of zero. Two corpora:

- **A user interface's strings**: clocks, menu rows, settings labels,
  ligatures, kerning pairs, joiners and combining marks, 80 in all, in Adwaita
  Sans, Noto Sans and Noto Sans Mono where installed (else the system's
  sans-serif), at 16, 28 and 32 px. `--ui-only` runs this corpus alone.
- **HarfBuzz's own shaping tests**: the `.tests` files under
  `test/shape/data`, fetched on first run with a sparse, blobless clone of
  <https://github.com/harfbuzz/harfbuzz> into
  `$XDG_CACHE_HOME/punchcutter/harfrust/hb-tests` (never committed: the fonts
  carry their own licences). Cases needing what the shaper does not do yet
  are skipped or excused by name and counted. `--tests-only` runs this corpus
  alone.

Measured on an integrated AMD GPU (RADV), against HarfBuzz revision 5365d27:

| corpus | compared | exact | mismatched | excused |
|---|---|---|---|---|
| UI strings, 3 fonts × 3 sizes | 720 | 711 | 0 | 9 |
| HarfBuzz's tests | 4160 | 4105 | 4 | 51 |

In the UI corpus, the 9 excused runs are a base and a combining mark that
harfrust's normaliser composes into one glyph and this shaper does not yet
(three strings, in both Noto faces, at each size). Of
HarfBuzz's cases, 1264 were skipped for a complex-script shaper, 124 for
Unicode properties not on the GPU yet, 606 for an option outside the default
shaper, and 49 were unreadable. The 51 excused are AAT fonts, the legacy
`kern` table, HarfBuzz's fallback mark positioning, automatic fractions, the
`rand` feature, the normaliser or HarfBuzz's space fallback, and one run
bounded by `MAX_RUN_GLYPHS` or `MAX_OPS`. The 4
mismatches are hostile-font cases newer than harfrust 0.12.0
(docs/roadmap.md, *Conformance*).

## The demo reel

```sh
cargo run -p punchcutter-fonts --example reel | \
    ffmpeg -f rawvideo -pix_fmt rgba -s 1280x720 -r 60 -i - \
    -c:v libx264 -pix_fmt yuv420p -crf 18 reel.mp4
```

Half a minute of video in which every pixel is the engine's. An ampersand's
quadratic curves draw themselves over Slug's bands while a probe casts the two
rays the coverage walk casts; the camera pulls back to this README, shaped,
broken into lines, justified and balanced into three columns on the GPU; a
lens magnifies it per pixel; a board of ligatures, kerning, small capitals,
old-style figures, fractions, mark attachment and a fallback face types itself
out, shaped again every frame. Each frame is three compute passes
(`fonts/examples/reel/reel.slang`: shape, typeset, draw) and the CPU decides
only the clock and what the camera frames. After the first frame only the
strings being typed are shaped and set again. The frames go to stdout as raw
RGBA; `--still <seconds> <file.png>` writes one instead, and `--live` plays the
reel in a window, in real time and on a loop, each frame copied from the
image buffer to the swapchain on the GPU:

```sh
cargo run -p punchcutter-fonts --features live --example reel -- --live --adapter discrete
```

The window is the `live` feature, so the tests never build a window system;
docs/performance.md is what it takes to hold 120 frames a second.
`--profile` prints each pass's GPU time, mean and worst, every two seconds,
from timestamp queries that never stall a frame; `--samples 1|4` sets the
coverage samples a pixel. It sets EB Garamond
when fontconfig has it (else Noto Serif, or `--font <path>`), and picks its
adapter as the render example does unless `--adapter discrete` asks for a
discrete GPU, which is the better place for a long render: it leaves the GPU
the desktop draws on alone. On the integrated GPU the draw pass goes a strip
of rows per submit, so the desktop still gets a turn between them.

## Not done yet

Colour fonts, Unicode itemisation and bidi, the normaliser, complex-script
shapers (Arabic joining, Hangul, Indic and the Universal Shaping Engine), and
variable-font instances. docs/roadmap.md has the list and the order.

## Licence

MIT; see `LICENSE`. `NOTICE` credits the Slug reference shaders (MIT OR
Apache-2.0), which `slang/modules/text.slang` ports, and the Unicode data whose ranges
`slang/modules/shape.slang` writes out.

CONTRIBUTING.md has the rules for changes.
