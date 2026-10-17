# Keeping a frame short

What a frame costs when text is shaped and drawn on the GPU, and what keeps
that cost small. Each point comes with what it measured in the demo reel
(`fonts/examples/reel`), which shapes, sets and draws about a hundred strings
a frame, including this repository's README; unless a point says otherwise,
the numbers are GPU time on a laptop RTX 4060 drawing a 2881×1684 window at
one coverage sample a pixel, against the 8.3 ms a 120 Hz display allows.

## Measuring

- **Time each pass on the GPU**, with timestamp queries written by the pass
  itself (`ComputePassTimestampWrites`), not with a CPU clock around a submit
  and a wait: the wait is most of what that measures.
- **Never wait for the timestamps.** Copy a frame's timestamps out, ask for
  the copy to be mapped, and read it on a later frame once it has arrived. A
  host that waits for every frame lets the GPU idle between frames, and an
  idle GPU clocks down: waiting made the reel's shaping read 3 ms a frame
  where it takes 0.3 ms.
- **Report the worst frame, not only the mean.** A display shows a frame late
  whenever one frame misses its interval, which a mean over two seconds
  hides.
- **Check that an optimisation changed nothing.** Render the same instants
  before and after and compare the frames byte for byte; the reel's
  `--still` always takes the full path, so comparing it against a frame
  reached incrementally also checks the incremental path.

The reel's `--profile` does the first three, and prints each pass's mean and
worst time every two seconds.

## Shaping

**A dispatch takes as long as its longest string.** One thread shapes one
string and walks its lookups alone (docs/arena.md, *What a dispatch costs*),
so a hundred short strings cost about what one long one does, and a string
shaped every frame costs its whole walk every frame.

- **Shape a string when it changes.** A run's glyphs stay valid in the output
  buffers until its text, its features or the font change. Dispatch only the
  requests that changed, named by a list of indices the host's own entry
  point reads, rather than rebuilding the batch. The reel shapes its strings
  once and, after that, only a string being typed: shaping all of them took
  about 35 ms of every frame, on a GPU kept busy; the typed ones alone take
  0.1 to 0.4 ms.
- **Do not shape again for a new size.** Positions scale linearly with
  `px_size`, so shape once at a large reference size and scale the positions
  at draw time; the reel shapes everything at 256 pixels an em, where a
  sixty-fourth of a pixel is a sixteen-thousandth of an em, and zooms from a
  single glyph filling the screen to a whole page without shaping again.
- **Pay the first shaping before the clock starts.** The reel's first
  frame, shaping every string from a GPU that was idle until then, takes 120
  to 140 ms; a host that shows text over time does that before its first
  visible frame rather than during it.

## Work done one thread per item

Anything one thread does per string, per paragraph or per block has the same
shape as the shaper, and the same remedy: do it when its input changes. The
reel's typesetter breaks lines, justifies and balances columns one thread per
block; setting the README page takes 40 to 50 ms because one thread walks
nine thousand glyphs twice, and the reel sets it once. A resize moves only
what is placed in pixels (the reel's captions), so it sets only those again.

## Drawing

- **Work out per-frame things once a frame, not once a pixel.** A camera
  framed on a group of blocks, the list of blocks on screen: one thread in a
  pass of its own computes them after the typesetter and before the draw,
  and every pixel reads the answer. In the reel, every pixel framing the
  camera itself against up to forty blocks, and then walking all forty-one
  blocks, cost 6 to 9 ms a frame; with a one-thread pass doing it once, the
  draw takes 1 to 1.7 ms.
- **Test a box before reading a record.** Check a sample against a block's
  rectangle before loading the block, and a glyph's rectangle
  (`textBounds`, `textGlyphRect`) before calling `textCoverage`.
- **Find glyphs by position, not by scanning.** Within a line the pens are in
  order, so a binary search on the sample's x finds the few glyphs that can
  reach it (the reel's `firstRightOf`, and the three before it).
- **Use the bands as a spatial index.** Any per-curve work at a pixel (an
  outline, a distance, a hit test) needs only the curves the glyph's bands
  near that pixel name: the horizontal bands within reach of its y and the
  vertical bands within reach of its x, since a band leaves out the curves
  flat along it. Band membership spans all three control points. The reel's
  outline overlay drew from every curve of its glyph at 4.4 ms a frame, and
  from the bands' curves at 2.3.
- **Match samples to pixel density.** Four samples on a rotated grid, each
  half a pixel across (twice the pixels per em), are worth it at an ordinary
  density; past about 1920×1200 one sample a pixel looks the same and costs a
  quarter.

## Getting pixels to the screen

Present by copying the image buffer into the swapchain's image on the GPU
(the surface configured with `COPY_DST`, rows padded to a multiple of 256
bytes, the shader writing BGRA when the swapchain wants it) rather than
reading the frame back and uploading it. A frame read back to the CPU is a
full frame of bytes across the bus and a wait for the GPU to finish, every
frame.

## Sharing a GPU with the desktop

An integrated GPU also draws the desktop, and cannot while a dispatch runs:
a 100 ms dispatch is a 100 ms pause of the pointer. Submit heavy work in
pieces of a few milliseconds and wait between them; the reel's offline draw
on an integrated GPU goes 48 rows a submit, about 7 ms each, for under 5 %
more time in all. A long render is better on a discrete GPU, which the
desktop is not drawing on, when there is one; integrated stays the default.

On a laptop whose display is driven by the integrated GPU, a discrete GPU's
frames are copied across to it to be shown. The reel's live window dropped
about three frames a second at 120 Hz while its GPU work was 4.4 ms a frame,
well inside the interval, and none once that was halved: leave room in the
interval rather than filling it.
