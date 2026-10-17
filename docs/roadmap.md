# Roadmap

What the engine does not do yet, roughly in the order it should be built.
Each item is a shader feature. Where the GPU needs data it does not have, the
CPU side packs a table and uploads it verbatim. None of these items is a CPU
fallback.

## Colour fonts

Slice `COLR` (v0 and v1) and `CPAL` into the arena verbatim and walk the paint
graph in `text.slang`: layers, solid and gradient fills, clips and transforms,
composited per pixel from the same curve buffers.

Today the fallback chain skips a face that draws a codepoint only in colour
(`COLR`, `CBDT`, `sbix`), so an emoji draws from a monochrome face if one is
installed. A regional-indicator flag draws as a pennant with its letter code.
Flags are the test case.

This comes first because it changes nothing in the shaper.

## Unicode

The properties shaping needs, packed on the CPU from the Unicode Character
Database and uploaded beside the arena:

- **Script itemisation.** The Script and Script_Extensions properties, so a
  string is cut into runs by script as well as by face. Today `scriptTagOf` in
  `shape.slang` is a range classifier for Latin, Greek, Cyrillic, Hebrew,
  Arabic, Devanagari, Thai, Hangul, kana and Han, and it uses the first strong
  character of the string.
- **Bidi.** The Unicode Bidirectional Algorithm, so right-to-left text is laid
  out in logical order and reordered for display. Today a right-to-left string
  draws in visual order as stored.
- **The normaliser.** Canonical composition and mark reordering, as HarfBuzz's
  `COMPOSED_DIACRITICS` mode does before `GSUB`. Today a base and a combining
  mark stay two glyphs even when the face has a precomposed one.
- **General_Category.** Needed for automatic fractions (`frac`, `numr`,
  `dnom` around U+2044) and for synthesising mark classes when a face has no
  `GlyphClassDef`. Today `isNonSpacingMark`, `isDefaultIgnorable` and
  `isExtendedPictographic` in `shape.slang` are hand-written ranges standing
  in for those tables.

## Complex scripts

These come after Unicode because each depends on it. First the Arabic joining
shaper (`isol`, `init`, `medi`, `fina` and related features) and Hangul jamo
composition. Then the Indic, Khmer and Myanmar syllable machines and the
Universal Shaping Engine.

Today an Arabic string shapes in isolated forms, and the conformance runner
skips the HarfBuzz cases that need a complex shaper, by name. The fallback
chain already fills the glyphs those shapers will reach, because the `GSUB`
closure is rooted at their features.

## Variable fonts

Shaping and drawing at an instance other than the default: `fvar` axes chosen
per request, `avar`, `gvar` deltas applied to the outlines when the curve
buffers are built, `HVAR` for advances, and the item variation store for
`GDEF`, `GPOS` Device tables and variation indices.

Today a variable font is read at its default instance on both sides. This
matches what harfrust does at default coordinates.

## Speed

The shaper runs one thread per string, and a wave of threads shaping different
strings executes the union of their paths. HarfBuzz keeps a per-lookup digest
(a small Bloom filter of the glyphs a lookup covers) to skip lookups cheaply.
The shader only caches a coverage extent per lookup, because it has nowhere to
keep a digest. Building one into the arena, or adding a pass that computes
them once per face, should be measured on real hardware first, not only on
software rasterisers.

## Without the shader clock

The shaper's SPIR-V requires `VK_KHR_shader_clock` for its wall-clock
deadline, so a device without the extension cannot load the module at all.
The fix is a Slang define that compiles the clock read out, leaving the
operation budget as the only bound.

## Conformance

The runner (`shape/examples/conform.rs`) compares against harfrust 0.12.0 and
fetches HarfBuzz's corpus from its main branch, which keeps moving. The
in-house `context-matching` cases on two hostile fonts are newer than harfrust
0.12.0. HarfBuzz expects one `.notdef` with the face's advance, harfrust emits
hundreds of glyphs, and this shaper emits `.notdef` with no advance and flags
the run unsupported. Matching HarfBuzz there means reading the advance of
`.notdef` from a face whose tables the shaper otherwise refuses.

## Out of scope

- **Hinting and subpixel rendering.** Slug coverage at the output's real scale
  replaces them. `Font::cap_height` is available so a consumer can snap its
  sizes.
- **Line breaking, wrapping and justification.** The engine shapes a run. A
  consumer that needs lines breaks them.
- **Vertical text.**
- **AAT** (`morx`, `kerx`, `trak`) **and Graphite.**
