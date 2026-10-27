# Keeping a frame short

This document covers what a frame costs when text is shaped and drawn on the
GPU, and how to keep that cost down. Each point includes a measurement from
the demo reel (`fonts/examples/reel`), which shapes, typesets and draws about
a hundred strings per frame, including this repository's README. Unless
stated otherwise, the numbers are GPU time on a laptop RTX 4060 drawing a
2881×1684 window with one coverage sample per pixel. The budget is the 8.3 ms
a 120 Hz display allows.

## Measuring

- **Time each pass on the GPU.** Use timestamp queries written by the pass
  itself (`ComputePassTimestampWrites`), not a CPU clock around a submit and
  a wait. The wait is most of what the CPU clock measures.
- **Never wait for the timestamps.** Copy a frame's timestamps out, request
  that the copy be mapped, and read it on a later frame once it has arrived.
  A host that waits every frame lets the GPU idle between frames, and an idle
  GPU clocks down. Waiting made the reel's shaping read 3 ms per frame when
  it actually takes 0.3 ms.
- **Report the worst frame, not just the mean.** A display shows a late frame
  whenever any single frame misses its interval. A mean over two seconds hides
  that.
- **Check that an optimisation changed nothing.** Render the same instants
  before and after and compare the frames byte for byte. The reel's `--still`
  always takes the full path, so comparing it against a frame reached
  incrementally also checks the incremental path.

The reel's `--profile` flag does the first three, printing each pass's mean
and worst time every two seconds.

## Shaping

A dispatch takes as long as its longest string. One thread shapes one string
and walks its lookups alone (docs/arena.md, *What a dispatch costs*), so a
hundred short strings cost about the same as one long one, and a string
shaped every frame costs its whole walk every frame.

- **Shape a string when it changes.** A run's glyphs stay valid in the output
  buffers until its text, its features or the font change. Dispatch only the
  requests that changed, selected by a list of indices the host's own entry
  point reads, rather than rebuilding the batch. The reel shapes its strings
  once and after that only the string being typed. Shaping all of them took
  about 35 ms per frame on a busy GPU; the typed strings alone take 0.1 to
  0.4 ms.
- **Do not reshape for a new size.** Positions scale linearly with
  `px_size`, so shape once at a large reference size and scale the positions
  at draw time. The reel shapes everything at 256 pixels per em, where a
  sixty-fourth of a pixel is a sixteen-thousandth of an em, and zooms from a
  single glyph filling the screen to a whole page without reshaping.
- **Do the first shaping before the clock starts.** The reel's first frame,
  which shapes every string on a GPU that was idle until then, takes 120 to
  140 ms. A host that shows text over time should do this before its first
  visible frame.

## What a cold start waits for

On a machine whose driver has never seen the shaper, the first shaping waits
for the driver to compile it. That compile, not Slang's, is the cost.

- **Measure it with the shader cache off.**

  ```text
  MESA_SHADER_CACHE_DISABLE=true cargo run --locked --release -p punchcutter-shape --example coldstart
  ```

  It prints each pipeline's creation and first dispatch. RADV compiles at
  creation; lavapipe compiles at the first dispatch. Run it three times and
  keep the median, and only while `/proc/loadavg` is below 2.
- **The numbers.** On an integrated Radeon 780M (RADV, Mesa 26.2.3), the one
  shaping pipeline takes about 3.3 s to create. On lavapipe it takes 1.0 s to
  create and 4.0 s at the first dispatch. Slang's own compile is about 3 s and
  happens once per build on the host.
- **The driver's time follows the inlined copies, not the bytes.** RADV
  wants about 520 vector registers for the shaper against 256 the hardware
  has, and spends most of the compile on spilling. Halving the SPIR-V with
  Slang's `-O2` or `spirv-opt -O` doubles RADV's time. Fewer call sites of a
  walker, a matcher or the iterator is what shortens it
  (`slang/modules/shape.slang`, *WHAT SLANGC DOES WITH THIS*).
- **Build the pipeline off the critical path.** A host that creates the
  shaper's pipeline on a worker thread, or before its first frame, shows its
  first frame without waiting for it. A warm Mesa shader cache makes the next
  start take tens of milliseconds.

## Work done one thread per item

Anything done by one thread per string, paragraph or block has the same
cost profile as the shaper and the same remedy: do it when its input changes.
The reel's typesetter breaks lines, justifies and balances columns with one
thread per block. Setting the README page takes 40 to 50 ms because one
thread walks nine thousand glyphs twice, so the reel sets it once. A resize
moves only what is positioned in pixels (the reel's captions), so only those
are set again.

## Drawing

- **Compute per-frame values once per frame, not once per pixel.** Things
  like a camera framed on a group of blocks, or the list of blocks on screen,
  belong in a one-thread pass that runs after the typesetter and before the
  draw. Every pixel then reads the answer. In the reel, having every pixel
  frame the camera against up to forty blocks and then walk all forty-one
  blocks cost 6 to 9 ms per frame. With a one-thread pass doing it once, the
  draw takes 1 to 1.7 ms.
- **Test a box before reading a record.** Check a sample against a block's
  rectangle before loading the block, and against a glyph's rectangle
  (`textBounds`, `textGlyphRect`) before calling `textCoverage`.
- **Find glyphs by position, not by scanning.** Within a line the pen
  positions are in order, so a binary search on the sample's x finds the few
  glyphs that can reach it (the reel's `firstRightOf`, plus the three before
  it).
- **Use the bands as a spatial index.** Any per-curve work at a pixel (an
  outline, a distance, a hit test) only needs the curves listed by the glyph's
  bands near that pixel: the horizontal bands within reach of its y and the
  vertical bands within reach of its x. A band omits the curves that are flat
  along it, and band membership spans all three control points. The reel's
  outline overlay took 4.4 ms per frame drawing from every curve of its glyph,
  and 2.3 ms drawing from the bands' curves.
- **Match samples to pixel density.** Four samples on a rotated grid, each
  half a pixel across (twice the pixels per em), are worth it at ordinary
  densities. Above about 1920×1200, one sample per pixel looks the same and
  costs a quarter as much.

## Getting pixels to the screen

Present by copying the image buffer into the swapchain image on the GPU. This
means configuring the surface with `COPY_DST`, padding rows to a multiple of
256 bytes, and having the shader write BGRA when the swapchain wants it. Do
not read the frame back and upload it. A readback moves a full frame of bytes
across the bus and waits for the GPU to finish, every frame.

## Sharing a GPU with the desktop

An integrated GPU also draws the desktop, and cannot do so while a dispatch
runs. A 100 ms dispatch is a 100 ms pause of the pointer. Submit heavy work in
pieces of a few milliseconds and wait between them. The reel's offline draw on
an integrated GPU goes 48 rows per submit, about 7 ms each, for under 5 % more
total time. A long render is better on a discrete GPU, which the desktop does
not use, when one is available. Integrated stays the default.

On a laptop whose display is driven by the integrated GPU, a discrete GPU's
frames are copied across to it for display. The reel's live window dropped
about three frames per second at 120 Hz while its GPU work was 4.4 ms per
frame, well inside the interval, and dropped none once that was halved. Leave
room in the interval rather than filling it.
