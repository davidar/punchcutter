// Shaping, on the GPU: codepoints in, glyph ids and pen positions out.
//
// README.md is the overview and docs/arena.md the contract; `shape/` (the
// `punchcutter-shape` crate) is the other half and is a **table slicer** — it finds `cmap`, `hmtx`, `GDEF`,
// `GSUB` and `GPOS` in the sfnt directory and copies them into one arena,
// verbatim, behind a record saying where each landed. `slang/ot.slang` reads
// OpenType's binary format out of that arena; this file is the shaper on top of
// it. `shape/src/abi.rs` declares the arena's header, rendered into `shape_abi.slang`; shape/tests/layout.rs checks that Slang lays it out as Rust does, through `shape_demo.slang`.
//
// The engine's rule: the CPU keeps I/O, and shaping a string is not I/O. A
// CPU that flattened the font into bespoke tables for this file to read would
// have to *understand* every lookup type, so each one the shaper gained would
// cost two implementations and a format change. This file takes the other
// fork: the font goes over as the bytes it is, and a lookup type is an edit to
// this file and nothing else.
//
// The reference for everything the spec leaves open is **harfrust 0.12.0**,
// HarfBuzz's own port; `shape/examples/
// conform.rs` compares against it with a tolerance of zero. Where the spec and
// harfrust disagree, the lines below say so and follow harfrust.
//
// ---------------------------------------------------------------------------
// WHAT THIS IS
//
// The HarfBuzz **default shaper**, on one thread per string:
//
//   * `cmap` formats 0, 4, 6, 12 and 13 walked per face, in harfrust's own
//     subtable preference order; `hmtx` advances walked, not copied;
//   * `GDEF` glyph classes, mark attachment classes and mark glyph sets, and
//     the lookup flags over them — IgnoreBaseGlyphs, IgnoreLigatures,
//     IgnoreMarks, MarkAttachmentType, UseMarkFilteringSet — as the skipping
//     iterator every subtable matches through;
//   * `GSUB` 1–8: Single, Multiple, Alternate (first alternate), Ligature,
//     Context (1, 2, 3), ChainContext (1, 2, 3), Extension, ReverseChainSingle.
//     Ligature ids and component indices tracked the way HarfBuzz keeps them,
//     so MarkToLigature finds its component;
//   * `GPOS` 1–9: Single, Pair, Cursive, MarkToBase, MarkToLigature,
//     MarkToMark, Context, ChainContext, Extension. ValueRecords in full;
//     mark advances zeroed by GDEF class and attachment chains resolved by the
//     same final passes HarfBuzz runs;
//   * feature selection the default shaper's way — the script's LangSys, its
//     required feature, and `abvm blwm ccmp locl mark mkmk rlig` plus
//     `calt clig curs dist kern liga rclt` for horizontal text plus `ltra ltrm`
//     (LTR), in **two GSUB stages** (`rvrn`, then the rest) and one GPOS stage,
//     each stage's lookups sorted by lookup-list index and applied in that
//     order across the whole buffer;
//   * many faces: a string is cut into maximal segments of one face (the
//     codepoint's face is the first in the arena's directory whose `cmap` has
//     it), and each segment is shaped with **its own** face's tables, so a
//     fallback face kerns and substitutes with itself.
//
// ---------------------------------------------------------------------------
// WHAT THIS IS NOT
//
//   * **No bidi, no script itemisation, no normalisation, no mirroring.** All
//     four want Unicode property tables on the GPU, which are not here yet
//     (docs/roadmap.md, *Unicode*). The script tag comes from a range
//     classifier below (`scriptTagOf`), a stand-in for the Unicode Script
//     property, and a
//     string is cut by *face* rather than by run. harfrust normalises before
//     GSUB (mode COMPOSED_DIACRITICS, short-circuiting on); a string that is
//     already NFC and whose characters the face has is the case where that is a
//     no-op, and it is the common case for UI text.
//   * **No complex-script shaper.** Arabic joining (`isol init medi fina`),
//     Hangul jamo composition, the Indic/Khmer/Myanmar/USE syllable machines
//     are not done. An Arabic string shapes here in isolated forms, in visual
//     order.
//   * **No automatic fractions.** HarfBuzz gives `frac`, `numr` and `dnom` a
//     mask it sets around U+2044 from the General_Category table; that table
//     is not here, so those three features are not enabled here at all — which is the
//     right answer for every string without a fraction slash in it, and the
//     harness skips the cases with one by name. With no non-global feature
//     left, every lookup mask and every glyph mask is the same bit and the mask
//     test is a tautology, so there is no mask in this file at all.
//   * **No Device or VariationIndex tables** (`ot.slang` reads past them and
//     flags it), no `gvar`/`HVAR` — a variable font shapes at its default
//     instance. harfrust skips the whole device block
//     at a default instance too, so this is agreement rather than a gap.
//   * **No colour** (`COLR`/`CPAL`): an emoji draws from a monochrome
//     fallback face if one is installed.
//   * No line breaking, wrapping or justification: the input is a run and the
//     output is a run. A caller that needs lines breaks them itself.
//
// A subtable format, anchor format or table version this file does not walk
// raises SHAPE_RUN_UNSUPPORTED on the run rather than answering a silent zero;
// a consumer reports it, and the harness prints it.
//
// Default_Ignorable_Code_Point codepoints are **hidden** at the end — advance
// zeroed, glyph id replaced by the face's space — which is HarfBuzz's
// `zero_width_default_ignorables` followed by `hide_default_ignorables`.
//
// ---------------------------------------------------------------------------
// POSITIONS
//
// Advances and offsets accumulate in **design units** (integers, exactly as the
// font states them) and are converted once per glyph to 26.6 fixed point at the
// request's pixel size. Accumulating in font units rather than in pixels is
// what makes the result independent of the size it is drawn at, and what makes
// it match a shaper *exactly* rather than within a rounding of one — the
// harness (`shape/examples/conform.rs`) compares font units with a tolerance of zero.
//
// A segment boundary is the one place a rounding happens early: each segment is
// scaled by **its own face's** upem, so the pen crosses from one face to the
// next in 26.6 rather than in design units that would mean different things on
// the two sides.
//
// ---------------------------------------------------------------------------
// NO RECURSION
//
// Slang has none, and OpenType's contextual lookups are defined with it. The
// nested application is an explicit stack of frames below (`fr_*`), bounded at
// MAX_NESTING, each holding its own `match_positions`; HarfBuzz's `max_ops`
// guard is a per-string budget (MAX_OPS) whose exhaustion raises
// SHAPE_RUN_OUT_OF_OPS, the sibling of SHAPE_RUN_TRUNCATED for time rather than
// room. `propagate_attachment_offsets` and `reverse_cursive_minor_offset` are
// recursive in HarfBuzz too, and are chain walks here.
//
// ---------------------------------------------------------------------------
// WHAT SLANGC DOES WITH THIS, AND WHY THE FILE IS SHAPED THE WAY IT IS
//
// SPIR-V's logical addressing has no way to pass a descriptor to a function, so
// Slang **inlines every function that takes one** — which is every function
// here that touches the arena. Three consequences run through the whole file
// and are each noted where they bite:
//
//   * the working state is module-scope `static` (a lane's own `Private`
//     storage) and not a struct passed `inout`, because `inout` is copy-in
//     copy-out and the glyph buffer is 8 KiB: five times the SPIR-V;
//   * `applySubtables` has **one call site**, and so does `applyLookupPass`,
//     and inside `applySubtables` each walker is called once: the three
//     contextual types and their formats share one matching body, reached
//     from one place whether the table is GSUB or GPOS. Every extra call site
//     is another copy of a walker, its matchers and both iterators;
//   * `otByte` is branchless, because it is inlined about three thousand times
//     and a bounds `if` in it would be three thousand basic blocks.
//
// The result is about 1 MB of SPIR-V for one entry point that shapes, and
// about three seconds of Slang. Both are large, both are measured, and neither
// is an accident. The driver's compile of that module is what a cold start
// waits for, and it scales with the copies rather than with the bytes.
//
// ---------------------------------------------------------------------------
// PARALLELISM
//
// One thread shapes one string, because UI strings are short (a clock, a
// label, a window title) and change rarely, and because substitution is a
// sequential rewrite of the glyph run. Forty labels is forty threads and one
// dispatch.
//
// It is **not fast**: 557 µs a string on Adwaita Sans on an integrated GPU,
// because a wave of 32 lanes shaping 32 different strings executes the union
// of their paths. That is with a coverage extent cached per lookup (`sb_*`),
// which makes it 3.7× faster; the rest wants HarfBuzz's per-lookup digest,
// which needs somewhere to cache one. A consumer that re-shapes when its text
// changes and not per frame makes this a number worth knowing rather than a
// problem worth solving.

module shape;

import ot;

// The ABI — every struct the host binds and every capacity and flag both sides
// name — is **generated** from `shape/src/abi.rs`, so there is no hand-written
// copy here for a test to compare against the Rust. Re-exported, so an
// importer such as `shape_demo.slang` sees these names through `import shape`.
__exported import shape_abi;

// ---------------------------------------------------------------------------
// Capacities that are this file's own: not in a buffer, and not the CPU's
// business.
// ---------------------------------------------------------------------------

/// Lookups one table may contribute to one stage. Adwaita Sans reaches 15 in
/// GSUB and 3 in GPOS; Noto Sans 24 and 4.
public static const uint MAX_LOOKUPS = 64u;

/// Entries this shaper will walk in **any** list whose length the arena states:
/// a LangSys's features, a Feature's lookups, a rule set's rules, a ligature
/// set's ligatures, a MultipleSubst's sequence.
///
/// The second bound, beside the op budget (`charge`). The budget makes a
/// pathological arena *finish*; this makes it finish quickly, because a cap of
/// 128 on each of two nested lists is 16384 steps against the 4.3 billion two
/// bare `u16`s allow. Every list in Adwaita Sans, Noto Sans, Noto Sans Mono and
/// HarfBuzz's whole corpus is far inside it, and a list that is not raises
/// `SHAPE_RUN_UNSUPPORTED` rather than being walked quietly short.
public static const uint MAX_LIST = 128u;

/// `.notdef`, by definition of the format.
public static const uint NOTDEF_GLYPH = 0u;

/// The buffers the shaper reads and writes, gathered so the entry point
/// decides its own bindings.
///
/// This module declares **no bindings of its own** — deliberately. The
/// importing shader owns its descriptor sets and places these where it likes;
/// a module that grabbed numbers would have to be edited to be imported.
/// `shape_demo.slang` shows the two lines it costs a host: the font is one arena of verbatim OpenType tables.
public struct ShapeTables
{
    /// The arena: the header, the face directory and the faces' tables.
    public StructuredBuffer<uint> ot;
    /// The UTF-8 bytes, four per word, little-endian — **three buffers and a
    /// selector**, not one buffer.
    ///
    /// A host's text may live in more than one buffer: literals in one, text
    /// that arrives at run time in another, say. Under SPIR-V's logical
    /// addressing a resource handle may not be a conditionally written local,
    /// so a host that wants one of three has to *branch around the call* — and
    /// Slang, which must inline every function taking a descriptor, then
    /// emits the whole shaper once per branch: three branches are three times
    /// the SPIR-V and the compile time of one.
    ///
    /// Reading through a selector is legal where assigning a handle is not, so
    /// `source` picks between three fields that are each a constant descriptor.
    /// A host with one buffer sets all three to it and leaves `source` at 0.
    ///
    /// **Read-write**, not because this module ever writes them — it does not —
    /// but because a host that composes bytes into the same buffer before
    /// shaping them (a clock, written by an earlier entry point) cannot bind
    /// one buffer read-only here and read-write there.
    public RWStructuredBuffer<uint> utf8;
    public RWStructuredBuffer<uint> utf8_alt0;
    public RWStructuredBuffer<uint> utf8_alt1;
    /// 0, 1 or 2: which of the three the request's `byte_start` indexes.
    public uint source;
    /// The working state one stage hands the next: one `ShapeScratch` per
    /// request, at the index the stage is given as `slot`. Only the shaper
    /// reads or writes it; the host allocates it and binds it.
    public RWStructuredBuffer<ShapeScratch> scratch;
}

// ---------------------------------------------------------------------------
// Glyph properties, as HarfBuzz keeps them
// ---------------------------------------------------------------------------

// The low bits are GDEF's classes turned into the lookup-flag bits they are
// tested against, which is why they *are* those bits: a lookup that ignores
// marks and a glyph that is a mark share the value 8.
static const uint PROPS_BASE = 0x02u;
static const uint PROPS_LIGATURE = 0x04u;
static const uint PROPS_MARK = 0x08u;
static const uint PROPS_SUBSTITUTED = 0x10u;
static const uint PROPS_LIGATED = 0x20u;
static const uint PROPS_MULTIPLIED = 0x40u;
/// What survives a substitution: everything the *lookups* said, none of what
/// GDEF said, because GDEF gets asked again about the new glyph.
static const uint PROPS_PRESERVE = PROPS_SUBSTITUTED | PROPS_LIGATED | PROPS_MULTIPLIED;
/// The flags a lookup may ignore a glyph for. The same three bits as
/// `LOOKUP_IGNORE_BASE_GLYPHS | LOOKUP_IGNORE_LIGATURES | LOOKUP_IGNORE_MARKS`,
/// which is the whole trick.
static const uint PROPS_IGNORE = PROPS_BASE | PROPS_LIGATURE | PROPS_MARK;
/// Not GDEF's: this codepoint is Default_Ignorable_Code_Point.
static const uint PROPS_IGNORABLE = 0x10000u;
/// Not GDEF's either: U+200C ZWNJ, U+200D ZWJ, and the *hidden* set (CGJ, the
/// Mongolian variation selectors, the tag characters), which the iterator
/// treats apart from the other ignorables.
static const uint PROPS_ZWNJ = 0x20000u;
static const uint PROPS_ZWJ = 0x40000u;
static const uint PROPS_HIDDEN = 0x80000u;

// `attach` packs HarfBuzz's two position fields into one word: the chain is a
// signed 16-bit delta to the glyph this one hangs off, the type says how. A
// chain of zero makes the type meaningless, which is harfrust's rule too.
static const uint ATTACH_MARK = 1u;
static const uint ATTACH_CURSIVE = 2u;

/// Attachment-chain links one glyph may hang off. HarfBuzz's limit is 64.
static const uint MAX_ATTACH = 16u;

/// The skipping iterator's three answers, HarfBuzz's own names.
static const uint SKIP_NO = 0u;
static const uint SKIP_YES = 1u;
static const uint SKIP_MAYBE = 2u;

/// What a lookup's iterator does about the two joiners and the hidden glyphs.
/// HarfBuzz builds two matchers per lookup — an *input* one and a *context*
/// one — and they differ in exactly these bits:
///
/// ```text
/// ignore_zwnj   = GPOS || (context && auto_zwnj)
/// ignore_zwj    = context || auto_zwj
/// ignore_hidden = GPOS
/// ```
///
/// with `auto_zwnj` and `auto_zwj` false for `mark` and `mkmk` alone
/// (F_GLOBAL_MANUAL_JOINERS) and true for every other feature this shaper
/// enables. Read it twice: **GSUB's input matcher never ignores ZWNJ**, whatever
/// the feature says, and that is what lets a ZWNJ break a ligature.
static const uint RULES_ZWNJ = 1u;
static const uint RULES_ZWJ = 2u;
static const uint RULES_HIDDEN = 4u;

// ---------------------------------------------------------------------------
// The working state
//
// Module-scope `static` rather than a struct passed `inout`, and the reason is
// measured rather than stylistic: Slang gives `inout` copy-in/copy-out value
// semantics, and a glyph buffer is 8 KiB. Passing it through the iterators and
// the twelve subtable walkers below produced **7.9 MB of SPIR-V**. A `static`
// here is a lane's own storage (SPIR-V `Private`), not shared state: nothing
// below is written by two threads, and nothing outlives the stage's entry
// point. What one stage hands the next goes through `ShapeTables.scratch`
// (*The stages*, below).
// ---------------------------------------------------------------------------

/// The glyph id **in the segment's face**, not the merged space. The base is
/// added once, at the write.
static uint g_gid[MAX_RUN_GLYPHS];
/// The byte offset in the request's string this glyph came from.
static uint g_cluster[MAX_RUN_GLYPHS];
/// `PROPS_*`.
static uint g_props[MAX_RUN_GLYPHS];
/// HarfBuzz's `lig_props`: `(lig_id << 5) | 0x10 | num_comps` for a ligature,
/// `(lig_id << 5) | lig_comp` for a mark inside one.
static uint g_lig[MAX_RUN_GLYPHS];
static int g_xadv[MAX_RUN_GLYPHS];
static int g_xoff[MAX_RUN_GLYPHS];
static int g_yoff[MAX_RUN_GLYPHS];
/// `(type << 16) | (chain & 0xFFFF)`, the chain signed.
static uint g_attach[MAX_RUN_GLYPHS];

static uint g_len;
/// `buffer->idx`: where the lookup pass is.
static uint g_cursor;
/// The lookup flags in force, `flag | (markFilteringSet << 16)` the way
/// HarfBuzz packs `lookup_props`.
static uint g_lookup_props;
/// `RULES_*` for the lookup in force.
static uint g_rules_input;
static uint g_rules_context;
/// Operations left before MAX_OPS bites. See `charge`.
static uint g_ops;
/// The device wall clock as this run started, its deadline in ticks (0 = the
/// driver has no `VK_KHR_shader_clock`), and the ops since the last read.
static uint g_clock_start;
static uint g_deadline;
static uint g_clock_step;
/// `SHAPE_RUN_*` accumulated for the whole request.
static uint g_flags;
/// The deepest nesting level this request reached.
static uint g_depth_seen;
/// The ligature id allocator, HarfBuzz's `buffer->serial`.
static uint g_serial;
/// Cleared when a nesting limit or the op budget aborts the run, which is what
/// HarfBuzz's `buffer->successful` does.
static bool g_ok;

// One level of contextual application: the records still to run, the positions
// they name, and the state to restore when it pops. `fr_positions` is flat:
// level `d`'s `i`th input position is `fr_positions[d * MAX_CONTEXT + i]`.
static uint fr_records[MAX_NESTING];
static uint fr_record_count[MAX_NESTING];
static uint fr_record_i[MAX_NESTING];
static uint fr_count[MAX_NESTING];
static int fr_end[MAX_NESTING];
static uint fr_saved_props[MAX_NESTING];
static uint fr_saved_input[MAX_NESTING];
static uint fr_saved_context[MAX_NESTING];
/// The sequence index a nested lookup is running at, or MAX_CONTEXT when none
/// is.
static uint fr_pending[MAX_NESTING];
/// The buffer length when that nested lookup started.
static uint fr_pending_len[MAX_NESTING];
static uint fr_positions[MAX_NESTING * MAX_CONTEXT];
static uint fr_depth;
/// Set when a contextual subtable matched and pushed a frame rather than
/// finishing on its own.
static bool fr_pushed;

/// `match_input`'s answer: where the input glyphs are, where they end, and how
/// many ligature components they carry between them. One shared buffer, because
/// a match is always consumed before the next one starts.
static uint mp_pos[MAX_CONTEXT];
static uint mp_end;
static uint mp_total;

/// Subtables one lookup may hold before this module stops prefiltering them.
/// Adwaita Sans's largest is 84 (its `calt`); Noto Sans's is 5.
static const uint MAX_SUBTABLES = 128u;

/// One lookup's subtables, **resolved and measured once**.
///
/// Every subtable of every type asks first whether one Coverage table holds the
/// glyph at the cursor, and that question costs a binary search. The extent of
/// that table — its first and last glyph id — answers "no" in three reads, and
/// resolving it does not depend on the glyph. So it is done once per lookup
/// pass rather than once per glyph: `calt` on Adwaita Sans is 84 subtables over
/// a string of fifteen glyphs, and that is 1260 extent reads against 84.
///
/// `sb_lookups` and `sb_lookup` are the memo, and it takes **both**: a lookup
/// index means nothing without the LookupList it indexes, and GSUB's lookup 1
/// and GPOS's lookup 1 are different lookups. Keying on the index alone would let GPOS reuse GSUB's subtables whenever the last GSUB lookup applied shared an
/// index with the first GPOS one — which is every font whose two tables have
/// the same small number of lookups, and is how the HarfBuzz corpus found it.
static uint sb_sub[MAX_SUBTABLES];
static uint sb_type[MAX_SUBTABLES];
static uint sb_lo[MAX_SUBTABLES];
static uint sb_hi[MAX_SUBTABLES];
static uint sb_count;
static uint sb_lookup;
static uint sb_lookups;
/// The union of the cached subtables' extents: one compare rejects a glyph no
/// subtable of the lookup could match.
static uint sb_union_lo;
static uint sb_union_hi;

/// The features this run forces on or off, copied out of
/// `ShapeParams.features` at the top of every stage — one word each, a packed
/// OpenType tag with `SHAPE_FEATURE_ON` set for on and clear for off.
///
/// A module-scope copy rather than a parameter for the reason the rest of the
/// state here is one: `collectStage` is reached through two inlined frames, and
/// a `ShapeParams` passed down them is 96 bytes copied at each.
///
/// A default-features caller sends none. This is the harness's seam — every one of the 276 cases of aots (Adobe's OpenType test suite, run by `shape/examples/conform.rs`) sets `--features="test"`, and without a way to say so the
/// purest GSUB/GPOS corpus available is entirely closed — and it is where
/// `locl` by language and the complex scripts' features will hang.
static uint uf_word[MAX_SHAPE_FEATURES];
static uint uf_count;

/// The lookups one stage of one table contributes, sorted by lookup index, and
/// the joiner rules the features that named each one agreed on.
static uint co_index[MAX_LOOKUPS];
static uint co_rules_input[MAX_LOOKUPS];
static uint co_rules_context[MAX_LOOKUPS];
static uint co_count;

/// Draw `n` steps from the run's operation budget; false when it is spent.
///
/// **Every loop whose trip count comes from arena bytes draws from this**.
/// HarfBuzz's `max_ops` is a budget against a pathological *font*; this is a
/// budget against bytes that are not a font at all, which is a stronger thing
/// to ask of it.
///
/// Charging one op per subtable actually walked is not enough. The loops around
/// that —
/// a `cmap`'s subtable records, a feature list's features, a feature's lookups,
/// a rule set's rules, a ligature set's ligatures — each take a `u16` straight
/// out of the arena as their trip count. An arena that is not an arena reads
/// 65535 for every one of them, and the product of two such loops is not a
/// number a frame budget survives: with those loops uncharged,
/// `shape/examples/garbage.rs` measured an all-0xFF arena at **1.39 s a
/// dispatch** on lavapipe, and on RADV it hung the graphics ring outright.
/// `SHAPE_RUN_OUT_OF_OPS` never fired, because none of those loops was an op.
///
/// So the rule is the plain one: a loop step that a byte of the arena chose
/// is an op. What that buys is the promise in `ot.slang`'s header — a malformed
/// arena draws wrong and **never stalls** — stated in milliseconds rather than
/// in hope.
bool charge(uint n)
{
    if (g_ops <= n)
    {
        g_ops = 0u;
        g_flags |= SHAPE_RUN_OUT_OF_OPS;
        g_ok = false;
        return false;
    }
    g_ops -= n;

    // And the clock, every SHAPE_CLOCK_EVERY ops. Counting is only ever as
    // good as the accounting; this is the bound that holds when the accounting
    // is wrong. `getRealtimeClock` is `OpReadClockKHR` at device scope — a
    // wall clock, not a cycle counter — and the low word is enough: it wraps
    // every forty-odd seconds at RADV's rate, far beyond any deadline a host sets (`SHAPE_DEADLINE_TICKS` is 250 ms at the slower rate), and an unsigned subtraction wraps with it.
    //
    // `deadline_ticks` of zero is `VK_KHR_shader_clock` absent, and the host
    // says so rather than letting this go quiet.
    g_clock_step += 1u;
    if (g_clock_step >= SHAPE_CLOCK_EVERY)
    {
        g_clock_step = 0u;
        if (g_deadline != 0u && getRealtimeClockLow() - g_clock_start >= g_deadline)
        {
            g_ops = 0u;
            g_flags |= SHAPE_RUN_DEADLINE;
            g_ok = false;
            return false;
        }
    }
    return true;
}

/// The trip count of a loop whose bound came **out of the arena**: the `u16` at
/// `off`, capped at `cap`, and charged to the budget in full before the first
/// iteration. A budget that cannot pay the whole loop answers 0, so the loop
/// does not run at all and the run carries `SHAPE_RUN_OUT_OF_OPS`.
///
/// This is the shape the lint in `reload/src/tests.rs` enforces: a `for` or
/// `while` in this file or in `ot.slang` may take its bound from a constant,
/// from the glyph buffer, or from here — never from a bare `be16`. Charging the
/// *stated* count rather than the iterations actually taken is deliberate: it
/// is the count an adversary controls, so it is the count the budget must
/// price.
uint otCount(OtArena a, uint off, uint cap)
{
    let stated = be16(a, off);
    if (stated > cap)
    {
        // A list longer than any common font states. The tail is not
        // walked, and the run says so rather than drawing quietly short.
        g_flags |= SHAPE_RUN_UNSUPPORTED;
    }
    let take = min(stated, cap);
    return charge(take) ? take : 0u;
}

/// The same for a count a caller already has in hand — a format-3 subtable's
/// single implied rule, a subtable count `ot.slang` read for its own reasons.
uint otTake(uint stated, uint cap)
{
    let take = min(stated, cap);
    return charge(take) ? take : 0u;
}

int attachChain(uint word)
{
    let raw = word & 0xFFFFu;
    return int(raw) - int((raw >> 15u) * 65536u);
}

uint attachType(uint word)
{
    return word >> 16u;
}

uint packAttach(uint type, int chain)
{
    return (type << 16u) | (uint(chain) & 0xFFFFu);
}

// HarfBuzz's lig_props accessors, written out.
uint ligId(uint lig_props)
{
    return lig_props >> 5u;
}

bool ligIsLigature(uint lig_props)
{
    return (lig_props & 0x10u) != 0u;
}

uint ligComp(uint lig_props)
{
    return ligIsLigature(lig_props) ? 0u : (lig_props & 0x0Fu);
}

uint ligNumComps(uint props, uint lig_props)
{
    if ((props & PROPS_LIGATURE) != 0u && ligIsLigature(lig_props))
    {
        return lig_props & 0x0Fu;
    }
    return 1u;
}

uint ligPropsForLigature(uint lig_id, uint num_comps)
{
    return (lig_id << 5u) | 0x10u | (num_comps & 0x0Fu);
}

uint ligPropsForMark(uint lig_id, uint comp)
{
    return (lig_id << 5u) | (comp & 0x0Fu);
}

/// Round the way HarfBuzz's `roundf` does — half **away from zero** — rather
/// than the way Slang's `round()` does, which is HLSL's round-half-to-even.
///
/// At 16 px on a 2048-upem face the scale is exactly 0.5, so every odd design
/// unit lands on a tie and the two rules disagree. Half of a typical UI's
/// strings hit such a tie, and it is the difference between a harness with a
/// tolerance of one and a harness with a tolerance of zero.
int roundAway(float value)
{
    return value >= 0.0 ? int(floor(value + 0.5)) : -int(floor(-value + 0.5));
}

/// A fresh ligature id: three bits, never zero. An equality token rather than a
/// unique id — it wraps at 7, exactly as HarfBuzz's does.
uint allocateLigId()
{
    g_serial += 1u;
    uint id = g_serial & 0x07u;
    if (id == 0u)
    {
        g_serial += 1u;
        id = g_serial & 0x07u;
    }
    return id == 0u ? 1u : id;
}

// ---------------------------------------------------------------------------
// The skipping iterator
// ---------------------------------------------------------------------------

/// GDEF's answer for a glyph, as the props word a lookup flag is tested against.
uint glyphProps(OtArena a, OtFace face, uint gid)
{
    let classes = gdefGlyphClasses(a, face);
    if (classes == 0u)
    {
        return 0u;
    }
    let klass = classDef(a, classes, gid);
    if (klass == GLYPH_CLASS_BASE)
    {
        return PROPS_BASE;
    }
    if (klass == GLYPH_CLASS_LIGATURE)
    {
        return PROPS_LIGATURE;
    }
    if (klass == GLYPH_CLASS_MARK)
    {
        // The second byte of a lookup flag names a mark attachment class, so
        // the class rides in the same word at the same shift.
        return PROPS_MARK | (classDef(a, gdefMarkAttachClasses(a, face), gid) << 8u);
    }
    // Class 0 (unlisted) and class 4 (component) are both "no flag ignores
    // this", which is the spec's own answer and HarfBuzz's `default:`.
    return 0u;
}

/// HarfBuzz's `check_glyph_property`: does this lookup see this glyph at all?
bool checkGlyphProperty(OtArena a, OtFace face, uint props, uint gid, uint lookup_props)
{
    if ((props & lookup_props & PROPS_IGNORE) != 0u)
    {
        return false;
    }
    if ((props & PROPS_MARK) == 0u)
    {
        return true;
    }
    if ((lookup_props & LOOKUP_USE_MARK_FILTERING_SET) != 0u)
    {
        return covered(a, gdefMarkGlyphSet(a, face, lookup_props >> 16u), gid);
    }
    if ((lookup_props & LOOKUP_MARK_ATTACHMENT_TYPE) != 0u)
    {
        return (lookup_props & LOOKUP_MARK_ATTACHMENT_TYPE)
            == (props & LOOKUP_MARK_ATTACHMENT_TYPE);
    }
    return true;
}

/// `may_skip`.
///
/// A glyph the lookup flags reject is SKIP_YES and never terminates a match. A
/// default-ignorable the `rules` say to ignore is SKIP_MAYBE, which a positive
/// glyph test can still consume. Everything else is SKIP_NO.
///
/// The `!substituted` in HarfBuzz's `is_default_ignorable` is the subtle one:
/// once a lookup has replaced a joiner, it stops being ignorable — so it stops
/// being skippable *and* stops being hidden at the end.
uint maySkip(OtArena a, OtFace face, uint at, uint lookup_props, uint rules)
{
    let props = g_props[at];
    if (!checkGlyphProperty(a, face, props, g_gid[at], lookup_props))
    {
        return SKIP_YES;
    }
    if ((props & PROPS_IGNORABLE) == 0u || (props & PROPS_SUBSTITUTED) != 0u)
    {
        return SKIP_NO;
    }
    if ((rules & RULES_ZWNJ) == 0u && (props & PROPS_ZWNJ) != 0u)
    {
        return SKIP_NO;
    }
    if ((rules & RULES_ZWJ) == 0u && (props & PROPS_ZWJ) != 0u)
    {
        return SKIP_NO;
    }
    if ((rules & RULES_HIDDEN) == 0u && (props & PROPS_HIDDEN) != 0u)
    {
        return SKIP_NO;
    }
    return SKIP_MAYBE;
}

/// How a contextual rule states the glyph it wants at a position:
///
///   0 — a glyph id, as Context/ChainContext format 1 states them;
///   1 — a class, as format 2 states them, against the ClassDef at `table`;
///   2 — a coverage table at `datum`, already made absolute by the caller;
///   3 — no test at all (HarfBuzz's MATCH_MAYBE), which is what PairPos,
///       CursivePos, MarkMarkPos and the mark-base search use.
bool contextMatches(OtArena a, uint at, uint kind, uint datum, uint table)
{
    if (kind == 0u)
    {
        return g_gid[at] == datum;
    }
    if (kind == 1u)
    {
        return classDef(a, table, g_gid[at]) == datum;
    }
    return covered(a, datum, g_gid[at]);
}

/// `skipping_iterator_t::next`, or `prev` when `backward`: the first position
/// past `from` in that direction that matches, or −1 when a non-skippable
/// glyph fails the test first.
///
/// One body for both directions, so the contextual walker inlines it once.
int iterStep(OtArena a, OtFace face, uint from, bool backward, uint lookup_props, uint rules,
             uint kind, uint datum, uint table)
{
    int at = int(from);
    while (backward ? at > 0 : uint(at) + 1u < g_len)
    {
        at += backward ? -1 : 1;
        let skip = maySkip(a, face, uint(at), lookup_props, rules);
        if (skip == SKIP_YES)
        {
            continue;
        }
        if (contextMatches(a, uint(at), kind, datum, table))
        {
            return at;
        }
        if (skip == SKIP_NO)
        {
            return -1;
        }
    }
    return -1;
}

/// `iterStep` forwards with no glyph test: the first position past `from`
/// that is not skipped, with every skippable one walked past.
int nextVisible(OtArena a, OtFace face, uint from, uint lookup_props, uint rules)
{
    uint at = from;
    while (at + 1u < g_len)
    {
        at += 1u;
        if (maySkip(a, face, at, lookup_props, rules) == SKIP_NO)
        {
            return int(at);
        }
    }
    return -1;
}

/// And backwards.
int prevVisible(OtArena a, OtFace face, uint from, uint lookup_props, uint rules)
{
    int at = int(from);
    while (at > 0)
    {
        at -= 1;
        if (maySkip(a, face, uint(at), lookup_props, rules) == SKIP_NO)
        {
            return at;
        }
    }
    return -1;
}

// ---------------------------------------------------------------------------
// Editing the buffer
// ---------------------------------------------------------------------------

void copySlot(uint to, uint from)
{
    g_gid[to] = g_gid[from];
    g_cluster[to] = g_cluster[from];
    g_props[to] = g_props[from];
    g_lig[to] = g_lig[from];
    g_xadv[to] = g_xadv[from];
    g_xoff[to] = g_xoff[from];
    g_yoff[to] = g_yoff[from];
    g_attach[to] = g_attach[from];
}

/// Replace the glyph at `at`, giving it GDEF's class for the new id and the
/// `PROPS_PRESERVE` bits the old one had — HarfBuzz's `_set_glyph_class`.
///
/// `class_guess` is what a face with no GlyphClassDef gets instead. With
/// neither, the output **inherits** the input's class bits, which is
/// HarfBuzz's behaviour and not an accident.
void setGlyph(OtArena a, OtFace face, uint at, uint gid, uint class_guess,
              bool ligature, bool component)
{
    uint props = g_props[at] | PROPS_SUBSTITUTED;
    if (ligature)
    {
        props |= PROPS_LIGATED;
        props &= ~PROPS_MULTIPLIED;
    }
    if (component)
    {
        props |= PROPS_MULTIPLIED;
    }
    // The Unicode-derived bits are this file's, not GDEF's, and survive.
    let unicode = props & (PROPS_IGNORABLE | PROPS_ZWNJ | PROPS_ZWJ | PROPS_HIDDEN);
    if (gdefGlyphClasses(a, face) != 0u)
    {
        props &= PROPS_PRESERVE;
        props |= glyphProps(a, face, gid);
    }
    else if (class_guess != 0u)
    {
        props &= PROPS_PRESERVE;
        props |= class_guess;
    }
    g_gid[at] = gid;
    g_props[at] = props | unicode;
    g_flags |= SHAPE_RUN_SUBSTITUTED;
}

/// Make room for `count` extra slots at `at + 1`, shifting the tail right.
bool insertAfter(uint at, uint count)
{
    if (count == 0u)
    {
        return true;
    }
    if (g_len + count > MAX_RUN_GLYPHS)
    {
        g_flags |= SHAPE_RUN_TRUNCATED;
        return false;
    }
    for (uint i = g_len; i > at + 1u; --i)
    {
        copySlot(i - 1u + count, i - 1u);
    }
    g_len += count;
    return true;
}

/// HarfBuzz's `merge_clusters` at `cluster_level` 0 (monotone graphemes), over
/// the half-open span `[start, end)`.
///
/// The span takes the **minimum** cluster any glyph in it carries, and the span
/// then grows in both directions over every neighbouring glyph that shares the
/// cluster it is about to lose — so a cluster is never split by a merge, which
/// is the property "monotone" names.
///
/// HarfBuzz writes this in two halves, one bounded by `buffer->idx` and one
/// walking back into the out-buffer, because its GSUB rewrite reads from one
/// array and writes to another. This shaper rewrites in place, so the two
/// halves are one backwards loop over the same array: everything before
/// `g_cursor` *is* the out buffer.
void mergeClusters(uint start, uint end)
{
    if (end <= start + 1u || end > g_len)
    {
        return;
    }
    uint cluster = g_cluster[start];
    for (uint i = start + 1u; i < end; ++i)
    {
        cluster = min(cluster, g_cluster[i]);
    }
    if (cluster != g_cluster[end - 1u])
    {
        while (end < g_len && g_cluster[end - 1u] == g_cluster[end])
        {
            end += 1u;
        }
    }
    if (cluster != g_cluster[start])
    {
        while (start > 0u && g_cluster[start - 1u] == g_cluster[start])
        {
            start -= 1u;
        }
    }
    for (uint i = start; i < end; ++i)
    {
        g_cluster[i] = cluster;
    }
}

/// HarfBuzz's `delete_glyph`: the cluster of a glyph about to disappear does
/// not disappear with it.
///
/// A cluster with another glyph on either side of it survives untouched; one
/// that does not merges backward into the glyph before, or — when the glyph
/// being deleted is the first of the buffer — forward into the glyph after.
void mergeForDelete(uint at)
{
    let cluster = g_cluster[at];
    if ((at + 1u < g_len && cluster == g_cluster[at + 1u])
        || (at != 0u && cluster == g_cluster[at - 1u]))
    {
        return;
    }
    if (at != 0u)
    {
        if (cluster < g_cluster[at - 1u])
        {
            let old = g_cluster[at - 1u];
            uint i = at;
            while (i > 0u && g_cluster[i - 1u] == old)
            {
                g_cluster[i - 1u] = cluster;
                i -= 1u;
            }
        }
        return;
    }
    if (at + 1u < g_len)
    {
        mergeClusters(at, at + 2u);
    }
}

/// Drop `count` slots starting at `at`.
void removeAt(uint at, uint count)
{
    if (count == 0u || at >= g_len)
    {
        return;
    }
    let drop = min(count, g_len - at);
    for (uint i = at; i + drop < g_len; ++i)
    {
        copySlot(i, i + drop);
    }
    g_len -= drop;
}

// ---------------------------------------------------------------------------
// Matching a contextual rule
// ---------------------------------------------------------------------------

/// What `matchSequence` matches.
static const uint SEQ_INPUT = 0u;
static const uint SEQ_LOOKAHEAD = 1u;
static const uint SEQ_BACKTRACK = 2u;

/// One of a rule's three sequences, HarfBuzz's `match_input`,
/// `match_lookahead` and `match_backtrack` as one body.
///
/// * `SEQ_INPUT`: the positions of the rule's `count` input glyphs, the first
///   being the cursor, into `mp_pos`; `mp_end` and `mp_total` come with them.
///   `datum_at` is where the input values start, two bytes each, beginning
///   with the *second* glyph.
/// * `SEQ_LOOKAHEAD`: `count` glyphs at or after `mp_end`, forwards. The
///   iterator starts one before it, because a step comes first.
/// * `SEQ_BACKTRACK`: `count` glyphs before the cursor, in the order the
///   table states them (which is away from the cursor).
///
/// For `kind == 2` the values are offsets from `base`. False when the
/// sequence does not match. One body because each copy of it is a copy of the
/// iterator, inlined.
bool matchSequence(OtArena a, OtFace face, uint seq, uint count, uint datum_at, uint kind,
                   uint table, uint base)
{
    let input = seq == SEQ_INPUT;
    let backward = seq == SEQ_BACKTRACK;
    let rules = input ? g_rules_input : g_rules_context;
    uint at = g_cursor;
    uint steps = count;
    uint first_lig_id = 0u;
    uint first_lig_comp = 0u;
    if (input)
    {
        mp_pos[0] = g_cursor;
        mp_end = g_cursor + 1u;
        mp_total = ligNumComps(g_props[g_cursor], g_lig[g_cursor]);
        if (count > MAX_CONTEXT)
        {
            g_flags |= SHAPE_RUN_UNSUPPORTED;
            return false;
        }
        first_lig_id = ligId(g_lig[g_cursor]);
        first_lig_comp = ligComp(g_lig[g_cursor]);
        steps = count == 0u ? 0u : count - 1u;
    }
    else if (!backward)
    {
        at = mp_end == 0u ? 0u : mp_end - 1u;
    }
    for (uint i = 0u; i < steps; ++i)
    {
        let datum = be16(a, datum_at + i * 2u);
        let value = kind == 2u ? base + datum : datum;
        let found = iterStep(a, face, at, backward, g_lookup_props, rules, kind, value, table);
        if (found < 0)
        {
            return false;
        }
        at = uint(found);
        if (!input)
        {
            continue;
        }
        mp_pos[i + 1u] = at;

        // HarfBuzz's rule for ligating glyphs that are already components of an
        // earlier ligature: either they all belong to the same component of the
        // same ligature, or none of them belongs to any. (The `Ligbase::MaySkip`
        // escape hatch — when the shared base ligature is itself skippable under
        // the current flags — is not here; it needs the whole output buffer, and
        // no common font reaches it.)
        let this_lig_id = ligId(g_lig[at]);
        let this_lig_comp = ligComp(g_lig[at]);
        if (first_lig_id != 0u && first_lig_comp != 0u)
        {
            if (first_lig_id != this_lig_id || first_lig_comp != this_lig_comp)
            {
                return false;
            }
        }
        else if (this_lig_id != 0u && this_lig_comp != 0u && this_lig_id != first_lig_id)
        {
            return false;
        }
        mp_total += ligNumComps(g_props[at], g_lig[at]);
    }
    if (input)
    {
        mp_end = at + 1u;
    }
    return true;
}

/// The three sequences a contextual walker asks for, as one loop with one call.
///
/// A `static` trip count, for the reason `g_gsub_stages` is one: a literal
/// would be unrolled, and the matcher inlined three times.
static uint g_sequences = 3u;

/// `ligate_input`: rewrite the `count` glyphs `mp_pos` names into one ligature.
///
/// The glyphs between the components — marks the iterator skipped — stay where
/// they are and have their `lig_props` rewritten to point at the component they
/// followed, which is how MarkToLigature later finds the right anchor. The
/// components themselves are dropped.
void ligateInput(OtArena a, OtFace face, uint count, uint match_end, uint lig_glyph,
                 uint total_components)
{
    // The whole matched span becomes one glyph, so it becomes one cluster
    // first — HarfBuzz's own first line in `ligate_input`, and the reason the
    // ligature carries the *minimum* of its components' clusters rather than
    // the first's.
    mergeClusters(g_cursor, match_end);

    // A ligature of a base and some marks is still a base (so the marks that
    // follow can attach to it); a ligature of nothing but marks is a mark.
    bool is_base = (g_props[mp_pos[0]] & PROPS_BASE) != 0u;
    bool is_mark = (g_props[mp_pos[0]] & PROPS_MARK) != 0u;
    for (uint i = 1u; i < count; ++i)
    {
        if ((g_props[mp_pos[i]] & PROPS_MARK) == 0u)
        {
            is_base = false;
            is_mark = false;
            break;
        }
    }
    let is_ligature = !is_base && !is_mark;

    let class_guess = is_ligature ? PROPS_LIGATURE : 0u;
    let lig_id = is_ligature ? allocateLigId() : 0u;
    uint last_lig_id = ligId(g_lig[g_cursor]);
    uint last_num_components = ligNumComps(g_props[g_cursor], g_lig[g_cursor]);
    uint components_so_far = last_num_components;

    if (is_ligature)
    {
        g_lig[g_cursor] = ligPropsForLigature(lig_id, total_components);
    }
    setGlyph(a, face, g_cursor, lig_glyph, class_guess, true, false);

    // Walk the matched span: the skipped glyphs are kept and re-componented,
    // the components themselves are dropped.
    uint write = g_cursor + 1u;
    uint read = g_cursor + 1u;
    for (uint i = 1u; i < count; ++i)
    {
        while (read < mp_pos[i] && read < g_len)
        {
            if (is_ligature)
            {
                uint this_comp = ligComp(g_lig[read]);
                if (this_comp == 0u)
                {
                    this_comp = last_num_components;
                }
                g_lig[read] = ligPropsForMark(
                    lig_id,
                    components_so_far - last_num_components + min(this_comp, last_num_components));
            }
            if (write != read)
            {
                copySlot(write, read);
            }
            write += 1u;
            read += 1u;
        }
        if (read >= g_len)
        {
            break;
        }
        last_lig_id = ligId(g_lig[read]);
        last_num_components = ligNumComps(g_props[read], g_lig[read]);
        components_so_far += last_num_components;
        read += 1u;
    }
    let removed = read - write;
    while (read < g_len)
    {
        copySlot(write, read);
        write += 1u;
        read += 1u;
    }
    g_len -= removed;

    // Re-adjust the marks that followed the last component and belonged to a
    // ligature of their own.
    let resume = match_end >= removed ? match_end - removed : 0u;
    if (!is_mark && last_lig_id != 0u)
    {
        for (uint i = resume; i < g_len; ++i)
        {
            if (ligId(g_lig[i]) != last_lig_id)
            {
                break;
            }
            let this_comp = ligComp(g_lig[i]);
            if (this_comp == 0u)
            {
                break;
            }
            g_lig[i] = ligPropsForMark(
                lig_id,
                components_so_far - last_num_components + min(this_comp, last_num_components));
        }
    }
    g_cursor = min(resume, g_len);
}

// ---------------------------------------------------------------------------
// GSUB subtables
// ---------------------------------------------------------------------------

/// GSUB types 1–4 at the cursor. The cursor moves the way that type moves it,
/// which is why `applyLookupPass` does not advance after a hit.
bool gsubSimple(OtArena a, OtFace face, uint sub, uint type)
{
    let at = g_cursor;
    let gid = g_gid[at];
    let format = be16(a, sub);
    // Types 2, 3 and 4 have one format; type 1 has two and tests its own
    // below. All four put their Coverage at the same offset, and types 2 to 4
    // index a list of the same length with the result, so both are read once.
    if (type != 1u && format != 1u)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return false;
    }
    let index = coverageIndex(a, sub + be16(a, sub + 2u), gid);
    if (index < 0 || (type != 1u && uint(index) >= be16(a, sub + 4u)))
    {
        return false;
    }

    if (type == 1u)
    {
        uint to;
        if (format == 1u)
        {
            to = (gid + be16(a, sub + 4u)) & 0xFFFFu;
        }
        else if (format == 2u)
        {
            if (uint(index) >= be16(a, sub + 4u))
            {
                return false;
            }
            to = be16(a, sub + 6u + uint(index) * 2u);
        }
        else
        {
            g_flags |= SHAPE_RUN_UNSUPPORTED;
            return false;
        }
        setGlyph(a, face, at, to, 0u, false, false);
        g_cursor = at + 1u;
        return true;
    }

    if (type == 2u)
    {
        let seq = sub + be16(a, sub + 6u + uint(index) * 2u);
        // A sequence longer than the glyph buffer cannot land in it anyway, so
        // that is this list's cap; the budget pays for it either way.
        let count = otCount(a, seq, MAX_RUN_GLYPHS);
        if (!g_ok)
        {
            // Out of budget, not a zero-length sequence: the two must not be
            // confused, because one of them deletes a glyph.
            return false;
        }
        if (count == 0u)
        {
            // Spec-illegal and Uniscribe-compatible: a zero-length sequence
            // deletes the glyph, and harfrust does the same.
            mergeForDelete(at);
            removeAt(at, 1u);
            g_flags |= SHAPE_RUN_SUBSTITUTED;
            return true;
        }
        if (count == 1u)
        {
            // Not a multiplication: replaced in place, with no MULTIPLIED bit.
            setGlyph(a, face, at, be16(a, seq + 2u), 0u, false, false);
            g_cursor = at + 1u;
            return true;
        }
        if (!insertAfter(at, count - 1u))
        {
            return false;
        }
        // The class a face with no GlyphClassDef guesses for the pieces.
        let class_guess = (g_props[at] & PROPS_LIGATURE) != 0u ? PROPS_BASE : 0u;
        let was_lig_id = ligId(g_lig[at]);
        let cluster = g_cluster[at];
        let props = g_props[at];
        let lig = g_lig[at];
        for (uint i = 0u; i < count; ++i)
        {
            let slot = at + i;
            if (i > 0u)
            {
                g_cluster[slot] = cluster;
                g_props[slot] = props;
                g_lig[slot] = lig;
                g_xadv[slot] = 0;
                g_xoff[slot] = 0;
                g_yoff[slot] = 0;
                g_attach[slot] = 0u;
            }
            if (was_lig_id == 0u)
            {
                // A piece of a multiplied glyph carries its index, so
                // MarkToBase can refuse everything but the first. A glyph that
                // was already attached to a ligature keeps what it had.
                g_lig[slot] = ligPropsForMark(0u, i);
            }
            setGlyph(a, face, slot, be16(a, seq + 2u + i * 2u), class_guess, false, true);
        }
        g_cursor = at + count;
        return true;
    }

    if (type == 3u)
    {
        let set = sub + be16(a, sub + 6u + uint(index) * 2u);
        if (be16(a, set) == 0u)
        {
            return false;
        }
        // The default shaper takes the first alternate; `aalt` and a user's
        // choice of another are not features this enables.
        setGlyph(a, face, at, be16(a, set + 2u), 0u, false, false);
        g_cursor = at + 1u;
        return true;
    }

    // Ligature.
    let set = sub + be16(a, sub + 6u + uint(index) * 2u);
    let ligatures = otCount(a, set, MAX_LIST);
    for (uint i = 0u; i < ligatures; ++i)
    {
        let liga = set + be16(a, set + 2u + i * 2u);
        let comps = be16(a, liga + 2u);
        if (comps == 0u || comps > MAX_CONTEXT)
        {
            continue;
        }
        if (!matchSequence(a, face, SEQ_INPUT, comps, liga + 4u, 0u, 0u, 0u))
        {
            continue;
        }
        ligateInput(a, face, comps, mp_end, be16(a, liga), mp_total);
        return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// GPOS subtables
// ---------------------------------------------------------------------------

/// Add a ValueRecord to one glyph's position, HarfBuzz's `apply_value`.
void applyValue(uint at, OtValue v)
{
    if (v.x_placement != 0 || v.y_placement != 0 || v.x_advance != 0)
    {
        g_xoff[at] += v.x_placement;
        g_yoff[at] += v.y_placement;
        g_xadv[at] += v.x_advance;
        g_flags |= SHAPE_RUN_POSITIONED;
    }
    // yAdvance is not applied for horizontal text — HarfBuzz subtracts it for
    // vertical and skips it here, and its two bytes are consumed either way.
}

/// GPOS types 1 and 2 at the cursor.
bool gposPair(OtArena a, OtFace face, uint sub, uint type)
{
    let at = g_cursor;
    let gid = g_gid[at];
    // Both types put their Coverage at the same offset, so it is read once.
    let format = be16(a, sub);
    let index = coverageIndex(a, sub + be16(a, sub + 2u), gid);
    if (index < 0)
    {
        return false;
    }

    if (type == 1u)
    {
        let value_format = be16(a, sub + 4u);
        uint value_at;
        if (format == 1u)
        {
            value_at = sub + 6u;
        }
        else if (format == 2u)
        {
            if (uint(index) >= be16(a, sub + 6u))
            {
                return false;
            }
            value_at = sub + 8u + uint(index) * valueRecordSize(value_format);
        }
        else
        {
            g_flags |= SHAPE_RUN_UNSUPPORTED;
            return false;
        }
        applyValue(at, readValue(a, value_at, value_format));
        g_cursor = at + 1u;
        return true;
    }

    let next = nextVisible(a, face, at, g_lookup_props, g_rules_input);
    if (next < 0)
    {
        return false;
    }
    let second = uint(next);
    let format1 = be16(a, sub + 4u);
    let format2 = be16(a, sub + 6u);
    let len1 = valueRecordSize(format1);
    let len2 = valueRecordSize(format2);

    if (format == 1u)
    {
        if (uint(index) >= be16(a, sub + 8u))
        {
            return false;
        }
        let set = sub + be16(a, sub + 10u + uint(index) * 2u);
        let count = be16(a, set);
        let record_size = 2u + len1 + len2;
        uint lo = 0u;
        uint hi = count;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be16(a, set + 2u + mid * record_size) < g_gid[second])
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo >= count || be16(a, set + 2u + lo * record_size) != g_gid[second])
        {
            return false;
        }
        let record = set + 2u + lo * record_size + 2u;
        applyValue(at, readValue(a, record, format1));
        applyValue(second, readValue(a, record + len1, format2));
        g_cursor = len2 != 0u ? second + 1u : second;
        return true;
    }

    if (format == 2u)
    {
        let class1 = classDef(a, sub + be16(a, sub + 8u), gid);
        let class2 = classDef(a, sub + be16(a, sub + 10u), g_gid[second]);
        let class1_count = be16(a, sub + 12u);
        let class2_count = be16(a, sub + 14u);
        if (class1 >= class1_count || class2 >= class2_count)
        {
            return false;
        }
        let stride = len1 + len2;
        let record = sub + 16u + (class1 * class2_count + class2) * stride;
        applyValue(at, readValue(a, record, format1));
        applyValue(second, readValue(a, record + len1, format2));
        g_cursor = len2 != 0u ? second + 1u : second;
        return true;
    }

    g_flags |= SHAPE_RUN_UNSUPPORTED;
    return false;
}

/// Walk a cursive attachment chain, reversing every link, so a glyph that was
/// already attached can take a new parent. HarfBuzz's
/// `reverse_cursive_minor_offset`, unrolled into a walk and an unwind.
void reverseCursiveChain(uint start, uint new_parent)
{
    uint nodes[MAX_ATTACH];
    uint parents[MAX_ATTACH];
    uint types[MAX_ATTACH];
    uint depth = 0u;
    uint at = start;
    while (depth < MAX_ATTACH)
    {
        let word = g_attach[at];
        let chain = attachChain(word);
        let type = attachType(word);
        if (chain == 0 || (type & ATTACH_CURSIVE) == 0u)
        {
            break;
        }
        g_attach[at] = packAttach(type, 0);
        let next = int(at) + chain;
        if (next < 0 || uint(next) >= g_len || uint(next) == new_parent)
        {
            break;
        }
        nodes[depth] = at;
        parents[depth] = uint(next);
        types[depth] = type;
        depth += 1u;
        at = uint(next);
    }
    // The recursion writes on the way out, deepest first.
    while (depth > 0u)
    {
        depth -= 1u;
        let node = nodes[depth];
        let parent = parents[depth];
        g_yoff[parent] = -g_yoff[node];
        g_attach[parent] = packAttach(types[depth], int(node) - int(parent));
    }
}

/// GPOS type 3, cursive attachment.
bool gposCursive(OtArena a, OtFace face, uint sub)
{
    if (be16(a, sub) != 1u)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return false;
    }
    let coverage = sub + be16(a, sub + 2u);
    let count = be16(a, sub + 4u);
    let i = g_cursor;
    let this_index = coverageIndex(a, coverage, g_gid[i]);
    if (this_index < 0 || uint(this_index) >= count)
    {
        return false;
    }
    let exit_offset = be16(a, sub + 6u + uint(this_index) * 4u + 2u);
    if (exit_offset == 0u)
    {
        return false;
    }
    // The cursive iterator keeps the lookup's flags but not its Ignore bits.
    let props = g_lookup_props & ~PROPS_IGNORE;
    let next = nextVisible(a, face, i, props, g_rules_input);
    if (next < 0)
    {
        return false;
    }
    let j = uint(next);
    let next_index = coverageIndex(a, coverage, g_gid[j]);
    if (next_index < 0 || uint(next_index) >= count)
    {
        return false;
    }
    let entry_offset = be16(a, sub + 6u + uint(next_index) * 4u);
    if (entry_offset == 0u)
    {
        return false;
    }
    let exit_anchor = readAnchor(a, sub + exit_offset);
    let entry_anchor = readAnchor(a, sub + entry_offset);
    if (exit_anchor.approximate || entry_anchor.approximate)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
    }

    // Major axis, LTR.
    g_xadv[i] = exit_anchor.x + g_xoff[i];
    let d = entry_anchor.x + g_xoff[j];
    g_xadv[j] -= d;
    g_xoff[j] -= d;

    // Minor axis: the RightToLeft **lookup flag** — not the run's direction —
    // decides which of the two moves.
    uint child = i;
    uint parent = j;
    int y_offset = entry_anchor.y - exit_anchor.y;
    if ((g_lookup_props & LOOKUP_RIGHT_TO_LEFT) == 0u)
    {
        child = j;
        parent = i;
        y_offset = -y_offset;
    }
    reverseCursiveChain(child, parent);
    g_attach[child] = packAttach(ATTACH_CURSIVE, int(parent) - int(child));
    g_yoff[child] = y_offset;
    // A parent that was attached to its own child is separated from it.
    if (attachChain(g_attach[parent]) != 0
        && attachChain(g_attach[parent]) == -attachChain(g_attach[child]))
    {
        g_attach[parent] = packAttach(attachType(g_attach[parent]), 0);
        g_yoff[parent] = 0;
    }
    g_flags |= SHAPE_RUN_POSITIONED;
    g_cursor = j;
    return true;
}

/// A MarkArray record's anchor and class.
bool markRecord(OtArena a, uint mark_array, uint index, out uint klass, out OtAnchor anchor)
{
    klass = 0u;
    anchor = readAnchor(a, 0u);
    if (index >= be16(a, mark_array))
    {
        return false;
    }
    let record = mark_array + 2u + index * 4u;
    klass = be16(a, record);
    let offset = be16(a, record + 2u);
    if (offset == 0u)
    {
        return false;
    }
    anchor = readAnchor(a, mark_array + offset);
    return anchor.valid;
}

/// One cell of an AnchorMatrix: `rows × class_count` offsets from the matrix.
bool matrixAnchor(OtArena a, uint matrix, uint row, uint klass, uint class_count,
                  out OtAnchor anchor)
{
    anchor = readAnchor(a, 0u);
    if (row >= be16(a, matrix) || klass >= class_count)
    {
        return false;
    }
    let offset = be16(a, matrix + 2u + (row * class_count + klass) * 2u);
    if (offset == 0u)
    {
        // No anchor for this pair: the *next subtable* gets a chance, which is
        // why this is a false rather than a zero.
        return false;
    }
    anchor = readAnchor(a, matrix + offset);
    return anchor.valid;
}

/// Attach the mark at the cursor to `to`, with the two anchors resolved.
void attachMark(uint to, OtAnchor mark_anchor, OtAnchor base_anchor)
{
    let at = g_cursor;
    // Assigned, not accumulated: HarfBuzz overwrites the mark's offsets here.
    g_xoff[at] = base_anchor.x - mark_anchor.x;
    g_yoff[at] = base_anchor.y - mark_anchor.y;
    g_attach[at] = packAttach(ATTACH_MARK, int(to) - int(at));
    g_flags |= SHAPE_RUN_POSITIONED;
    g_cursor = at + 1u;
}

/// The glyph a mark attaches to: the nearest one back that IgnoreMarks alone
/// would see, skipping every piece of a multiple substitution but the first.
///
/// The lookup's own flags, its mark filtering set and its mark attachment class
/// are all discarded for this search — which is harfrust's behaviour and
/// HarfBuzz's, and is not what a plain reading of the spec would suggest.
int markBase(OtArena a, OtFace face)
{
    int at = int(g_cursor);
    while (at > 0)
    {
        let found = prevVisible(a, face, uint(at), PROPS_MARK, g_rules_input);
        if (found < 0)
        {
            return -1;
        }
        at = found;
        if ((g_props[uint(at)] & PROPS_MULTIPLIED) == 0u || ligComp(g_lig[uint(at)]) == 0u)
        {
            return at;
        }
    }
    return -1;
}

/// GPOS types 4 and 5: MarkToBase and MarkToLigature.
bool gposMarkBase(OtArena a, OtFace face, uint sub, bool ligature)
{
    if (be16(a, sub) != 1u)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return false;
    }
    let mark_index = coverageIndex(a, sub + be16(a, sub + 2u), g_gid[g_cursor]);
    if (mark_index < 0)
    {
        return false;
    }
    let found = markBase(a, face);
    if (found < 0)
    {
        return false;
    }
    let j = uint(found);
    let base_index = coverageIndex(a, sub + be16(a, sub + 4u), g_gid[j]);
    if (base_index < 0)
    {
        return false;
    }
    let class_count = be16(a, sub + 6u);
    let mark_array = sub + be16(a, sub + 8u);
    let array = sub + be16(a, sub + 10u);

    uint klass;
    OtAnchor mark_anchor;
    if (!markRecord(a, mark_array, uint(mark_index), klass, mark_anchor))
    {
        return false;
    }

    OtAnchor base_anchor;
    if (!ligature)
    {
        if (!matrixAnchor(a, array, uint(base_index), klass, class_count, base_anchor))
        {
            return false;
        }
    }
    else
    {
        // LigatureArray: one LigatureAttach per covered ligature, each an
        // AnchorMatrix of `componentCount` rows.
        if (uint(base_index) >= be16(a, array))
        {
            return false;
        }
        let attach = array + be16(a, array + 2u + uint(base_index) * 2u);
        let comp_count = be16(a, attach);
        if (comp_count == 0u)
        {
            return false;
        }
        // The mark knows which component it followed only when it belongs to
        // the same ligature; otherwise it goes on the last one.
        let lig_id = ligId(g_lig[j]);
        let mark_id = ligId(g_lig[g_cursor]);
        let mark_comp = ligComp(g_lig[g_cursor]);
        uint comp_index = comp_count - 1u;
        if (lig_id != 0u && lig_id == mark_id && mark_comp > 0u)
        {
            comp_index = min(comp_count, mark_comp) - 1u;
        }
        if (!matrixAnchor(a, attach, comp_index, klass, class_count, base_anchor))
        {
            return false;
        }
    }
    if (mark_anchor.approximate || base_anchor.approximate)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
    }
    attachMark(j, mark_anchor, base_anchor);
    return true;
}

/// GPOS type 6, MarkToMark.
bool gposMarkMark(OtArena a, OtFace face, uint sub)
{
    if (be16(a, sub) != 1u)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return false;
    }
    let mark1_index = coverageIndex(a, sub + be16(a, sub + 2u), g_gid[g_cursor]);
    if (mark1_index < 0)
    {
        return false;
    }
    // Unlike MarkToBase, this one keeps the lookup's flags — mark filtering set
    // and attachment class included — and only clears the three Ignore bits.
    let props = g_lookup_props & ~PROPS_IGNORE;
    let found = prevVisible(a, face, g_cursor, props, g_rules_input);
    if (found < 0)
    {
        return false;
    }
    let j = uint(found);
    if ((g_props[j] & PROPS_MARK) == 0u)
    {
        return false;
    }

    // The two marks must belong to the same base, or to the same ligature
    // component, or one of them must itself be a ligature.
    let id1 = ligId(g_lig[g_cursor]);
    let id2 = ligId(g_lig[j]);
    let comp1 = ligComp(g_lig[g_cursor]);
    let comp2 = ligComp(g_lig[j]);
    bool good = false;
    if (id1 == id2)
    {
        good = id1 == 0u || comp1 == comp2;
    }
    else
    {
        good = (id1 > 0u && comp1 == 0u) || (id2 > 0u && comp2 == 0u);
    }
    if (!good)
    {
        return false;
    }

    let mark2_index = coverageIndex(a, sub + be16(a, sub + 4u), g_gid[j]);
    if (mark2_index < 0)
    {
        return false;
    }
    let class_count = be16(a, sub + 6u);
    let mark1_array = sub + be16(a, sub + 8u);
    let mark2_array = sub + be16(a, sub + 10u);

    uint klass;
    OtAnchor mark_anchor;
    if (!markRecord(a, mark1_array, uint(mark1_index), klass, mark_anchor))
    {
        return false;
    }
    OtAnchor base_anchor;
    if (!matrixAnchor(a, mark2_array, uint(mark2_index), klass, class_count, base_anchor))
    {
        return false;
    }
    if (mark_anchor.approximate || base_anchor.approximate)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
    }
    attachMark(j, mark_anchor, base_anchor);
    return true;
}

// ---------------------------------------------------------------------------
// Contextual subtables: matching, then a frame on the nesting stack
// ---------------------------------------------------------------------------

/// Push a frame for the `record_count` lookup records at `records`, over the
/// input positions `mp_pos` just matched.
void pushFrame(uint records, uint record_count, uint count, uint match_end)
{
    if (fr_depth >= MAX_NESTING)
    {
        return;
    }
    let d = fr_depth;
    fr_records[d] = records;
    fr_record_count[d] = record_count;
    fr_record_i[d] = 0u;
    fr_count[d] = count;
    fr_end[d] = int(match_end);
    fr_pending[d] = MAX_CONTEXT;
    fr_pending_len[d] = 0u;
    for (uint i = 0u; i < count; ++i)
    {
        fr_positions[d * MAX_CONTEXT + i] = mp_pos[i];
    }
    fr_depth = d + 1u;
    fr_pushed = true;
}

/// What `applyContextual` is walking.
static const uint CTX_CONTEXT = 0u;
static const uint CTX_CHAIN = 1u;
static const uint CTX_REVERSE = 2u;

/// A Context (GSUB 5 / GPOS 7), ChainContext (GSUB 6 / GPOS 8) or
/// ReverseChainSingleSubst (GSUB 8) subtable at the cursor.
///
/// The three types and their formats share one body, because they match the
/// same way and differ in what they do with a match. A Context rule is a
/// ChainContext rule with no backtrack and no lookahead, so the chained layout
/// is walked for both and the two sequences a Context rule lacks have count
/// zero. A ReverseChainSingleSubst is a ChainContext of format 3 with one input
/// glyph, the subtable's own coverage, and a list of substitutes where the
/// lookup records would be; it substitutes in place instead of pushing a
/// frame, and leaves the cursor where it is because the backward pass moves
/// it.
///
/// Format 1 states its rules as glyph ids and format 2 as classes, which is
/// the `kind` the matcher takes; format 3 has no rule *set* at all: the
/// subtable is one rule whose input is coverage tables, so it is walked as a
/// set of exactly one rule at `sub + 2`, with the first glyph's coverage left in
/// the input array rather than taken out of it.
///
/// Written as one body on purpose: Slang inlines every function that takes a
/// descriptor, so a second copy of this block is a second copy of
/// `matchSequence` and the iterator.
///
/// The order the three sequences are matched in is HarfBuzz's: input,
/// lookahead, backtrack. It matters, because the first failure decides.
bool applyContextual(OtArena a, OtFace face, uint sub, uint what)
{
    let chain = what == CTX_CHAIN;
    let reverse = what == CTX_REVERSE;
    let format = be16(a, sub);
    if (format < 1u || format > (reverse ? 1u : 3u))
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return false;
    }
    // Format 3's input array keeps its first entry; formats 1 and 2 take the
    // first glyph from the subtable's own coverage and start the array at the
    // second. That one offset is the whole difference. The reverse type has
    // no input array at all.
    let coverage_input = format == 3u || reverse;
    let first_in_array = format == 3u ? 1u : 0u;
    let kind = coverage_input ? 2u : (format == 1u ? 0u : 1u);
    let base = coverage_input ? sub : 0u;

    uint back_table = 0u;
    uint input_table = 0u;
    uint ahead_table = 0u;
    uint set = 0u;
    uint rules = 1u;
    // The reverse type's coverage index picks its substitute.
    int index = -1;
    if (reverse)
    {
        index = coverageIndex(a, sub + be16(a, sub + 2u), g_gid[g_cursor]);
        if (index < 0)
        {
            return false;
        }
    }
    else if (format != 3u)
    {
        let covered_at = coverageIndex(a, sub + be16(a, sub + 2u), g_gid[g_cursor]);
        if (covered_at < 0)
        {
            return false;
        }
        uint set_index;
        uint sets_at;
        uint set_count;
        if (format == 1u)
        {
            set_index = uint(covered_at);
            set_count = be16(a, sub + 4u);
            sets_at = sub + 6u;
        }
        else
        {
            // Context format 2 has one ClassDef where ChainContext format 2
            // has three; the input ClassDef is the one the rule set is chosen
            // by, and the other two are read only when they exist.
            if (chain)
            {
                back_table = sub + be16(a, sub + 4u);
                ahead_table = sub + be16(a, sub + 8u);
            }
            input_table = sub + be16(a, chain ? sub + 6u : sub + 4u);
            set_index = classDef(a, input_table, g_gid[g_cursor]);
            let count_at = chain ? sub + 10u : sub + 6u;
            set_count = be16(a, count_at);
            sets_at = count_at + 2u;
        }
        if (set_index >= set_count)
        {
            return false;
        }
        let offset = be16(a, sets_at + set_index * 2u);
        if (offset == 0u)
        {
            return false;
        }
        set = sub + offset;
        rules = otCount(a, set, MAX_LIST);
    }

    for (uint r = 0u; r < rules; ++r)
    {
        // Format 3's one rule is the subtable from its first glyph count on,
        // which puts its fields at the offsets a rule in a set has; the
        // reverse type's is the subtable from its backtrack count on.
        let rule = reverse ? sub + 4u : (format == 3u ? sub + 2u : set + be16(a, set + 2u + r * 2u));
        // A ChainedSequenceRule opens with its backtrack; a SequenceRule
        // opens with its input count, followed by its lookup count.
        uint back_count = 0u;
        uint back_at = rule;
        uint input_at_count = rule;
        if (chain || reverse)
        {
            back_count = be16(a, rule);
            back_at = rule + 2u;
            input_at_count = back_at + back_count * 2u;
        }
        uint input_count = 1u;
        if (!reverse)
        {
            input_count = be16(a, input_at_count);
            if (input_count == 0u || input_count > MAX_CONTEXT)
            {
                continue;
            }
        }
        let input_at = input_at_count + (chain ? 2u : 4u);
        let input_end =
            reverse ? input_at_count : input_at + (input_count - 1u + first_in_array) * 2u;
        // A SequenceRule's lookup records follow its input; a
        // ChainedSequenceRule's follow its lookahead and its lookup count,
        // and the reverse type's substitutes follow its lookahead and their
        // count the same way.
        uint ahead_count = 0u;
        uint ahead_at = input_end;
        uint lookup_at_count = input_at_count + 2u;
        uint records = input_end;
        if (chain || reverse)
        {
            ahead_count = be16(a, input_end);
            ahead_at = input_end + 2u;
            lookup_at_count = ahead_at + ahead_count * 2u;
            records = lookup_at_count + 2u;
        }
        let lookups = be16(a, lookup_at_count);

        if (format == 3u && !covered(a, base + be16(a, input_at), g_gid[g_cursor]))
        {
            return false;
        }
        bool matched = true;
        for (uint seq = 0u; seq < g_sequences && matched; ++seq)
        {
            uint seq_count = input_count;
            uint seq_at = input_at + first_in_array * 2u;
            uint seq_table = input_table;
            if (seq == SEQ_LOOKAHEAD)
            {
                seq_count = ahead_count;
                seq_at = ahead_at;
                seq_table = ahead_table;
            }
            else if (seq == SEQ_BACKTRACK)
            {
                seq_count = back_count;
                seq_at = back_at;
                seq_table = back_table;
            }
            matched = matchSequence(a, face, seq, seq_count, seq_at, kind, seq_table, base);
        }
        if (!matched)
        {
            continue;
        }
        let match_end = mp_end;
        if (reverse)
        {
            if (uint(index) >= lookups)
            {
                return false;
            }
            setGlyph(a, face, g_cursor, be16(a, records + uint(index) * 2u), 0u, false, false);
            return true;
        }
        pushFrame(records, lookups, input_count, match_end);
        return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// One lookup, and the nesting stack under it
// ---------------------------------------------------------------------------

/// Fill the subtable cache for one lookup, unless it already holds it.
void cacheLookup(OtArena a, uint lookups, uint lookup_index, bool gpos)
{
    if (sb_lookups == lookups && sb_lookup == lookup_index && sb_count != 0xFFFFFFFFu)
    {
        return;
    }
    sb_lookups = lookups;
    sb_lookup = lookup_index;
    sb_union_lo = 0xFFFFFFFFu;
    sb_union_hi = 0u;
    let lookup = lookupAt(a, lookups, lookup_index);
    let type = lookupType(a, lookup);
    let count = subtableCount(a, lookup);
    if (count > MAX_SUBTABLES)
    {
        // No common font comes near; one that did would have its tail
        // ignored, so it says so rather than drawing quietly wrong.
        g_flags |= SHAPE_RUN_UNSUPPORTED;
    }
    // Each of these costs a coverage resolution and a bounds read, and the
    // count is the arena's — so the budget pays for the whole cache fill
    // before it starts.
    sb_count = otTake(count, MAX_SUBTABLES);
    for (uint s = 0u; s < sb_count; ++s)
    {
        uint sub = subtableAt(a, lookup, s);
        uint real = type;
        if ((!gpos && type == 7u) || (gpos && type == 9u))
        {
            sub = extensionTarget(a, sub, real);
            // An extension of an extension is not a thing, and harfrust
            // refuses it too.
            if (real == 7u || real == 9u)
            {
                g_flags |= SHAPE_RUN_UNSUPPORTED;
                sub = 0u;
            }
        }
        uint lo;
        uint hi;
        coverageBounds(a, primaryCoverage(a, sub, real, gpos), lo, hi);
        sb_sub[s] = sub;
        sb_type[s] = real;
        sb_lo[s] = lo;
        sb_hi[s] = hi;
        if (lo <= hi)
        {
            sb_union_lo = min(sb_union_lo, lo);
            sb_union_hi = max(sb_union_hi, hi);
        }
    }
}

/// Try a lookup's subtables at the cursor, in order, stopping at the first that
/// applies. The lookup's own flags and joiner rules are in force while it runs.
///
/// **One call site** in this file, and that is deliberate: every subtable walker
/// above is inlined into this function, and a second call would be a second copy
/// of all of them. Three call sites cost megabytes of SPIR-V.
bool applySubtables(OtArena a, OtFace face, uint lookups, uint lookup_index, bool gpos,
                    uint rules_input, uint rules_context)
{
    let lookup = lookupAt(a, lookups, lookup_index);
    if (lookup == 0u)
    {
        return false;
    }
    let saved_props = g_lookup_props;
    let saved_input = g_rules_input;
    let saved_context = g_rules_context;
    g_lookup_props = lookupFlag(a, lookup) | (markFilteringSet(a, lookup) << 16u);
    g_rules_input = rules_input;
    g_rules_context = rules_context;

    cacheLookup(a, lookups, lookup_index, gpos);
    let count = sb_count;
    let here = g_gid[g_cursor];
    bool applied = false;
    fr_pushed = false;
    for (uint s = 0u; s < count; ++s)
    {
        // The extent reject, out of the cache: two reads against the twenty a
        // binary search costs, and a subtable whose coverage cannot hold this
        // glyph is not walked at all.
        if (here < sb_lo[s] || here > sb_hi[s])
        {
            continue;
        }
        if (!charge(1u))
        {
            break;
        }
        let sub = sb_sub[s];
        let real = sb_type[s];
        if (sub == 0u)
        {
            continue;
        }
        // The contextual walker is dispatched **before** the table is asked,
        // so that it has one call site here. GSUB 5 and GPOS 7 are the same
        // Context subtable, GSUB 6 and GPOS 8 the same ChainContext, and
        // GSUB 8 matches the same way; a call under each branch would inline
        // the walker, its matchers and both iterators twice, which is a third
        // of the module and about a fifth of the driver's compile time.
        bool hit = false;
        let context = gpos ? real == 7u : real == 5u;
        let chain = gpos ? real == 8u : real == 6u;
        let reverse = !gpos && real == 8u;
        if (context || chain || reverse)
        {
            hit = applyContextual(a, face, sub,
                                  chain ? CTX_CHAIN : (reverse ? CTX_REVERSE : CTX_CONTEXT));
        }
        else if (!gpos)
        {
            if (real >= 1u && real <= 4u)
            {
                hit = gsubSimple(a, face, sub, real);
            }
            else
            {
                g_flags |= SHAPE_RUN_UNSUPPORTED;
            }
        }
        else
        {
            if (real == 1u || real == 2u)
            {
                hit = gposPair(a, face, sub, real);
            }
            else if (real == 3u)
            {
                hit = gposCursive(a, face, sub);
            }
            else if (real == 4u || real == 5u)
            {
                hit = gposMarkBase(a, face, sub, real == 5u);
            }
            else if (real == 6u)
            {
                hit = gposMarkMark(a, face, sub);
            }
            else
            {
                g_flags |= SHAPE_RUN_UNSUPPORTED;
            }
        }
        if (hit)
        {
            applied = true;
            break;
        }
    }
    if (fr_pushed)
    {
        // The frame keeps the state to restore; it is still this lookup's while
        // its records run.
        let d = fr_depth - 1u;
        fr_saved_props[d] = saved_props;
        fr_saved_input[d] = saved_input;
        fr_saved_context[d] = saved_context;
    }
    else
    {
        g_lookup_props = saved_props;
        g_rules_input = saved_input;
        g_rules_context = saved_context;
    }
    return applied;
}

/// Apply one lookup at the cursor, running every nested lookup it reaches to
/// completion. HarfBuzz's `recurse` plus `apply_lookup`, as one loop.
bool applyLookupTree(OtArena a, OtFace face, uint lookups, uint lookup_index, bool gpos,
                     uint rules_input, uint rules_context)
{
    fr_depth = 0u;
    bool applied = false;
    bool first = true;
    bool have_pending = true;
    uint pending_lookup = lookup_index;
    uint pending_input = rules_input;
    uint pending_context = rules_context;

    while (g_ok)
    {
        if (have_pending)
        {
            have_pending = false;
            let hit = applySubtables(a, face, lookups, pending_lookup, gpos,
                                     pending_input, pending_context);
            if (first)
            {
                applied = hit;
                first = false;
            }
            continue;
        }
        if (fr_depth == 0u)
        {
            break;
        }
        let d = fr_depth - 1u;
        g_depth_seen = max(g_depth_seen, fr_depth);

        if (fr_pending[d] != MAX_CONTEXT)
        {
            // A nested lookup just finished: move the input positions the way
            // HarfBuzz's `apply_lookup` moves them, which is an *assumption*
            // about where the glyphs went (grown ⇒ right after the position,
            // shrunk ⇒ the positions after it) and not the truth. Reproducing
            // the assumption is the point, because the reference has it too.
            let idx = fr_pending[d];
            int delta = int(g_len) - int(fr_pending_len[d]);
            fr_pending[d] = MAX_CONTEXT;
            if (delta == 0)
            {
                continue;
            }
            let base = d * MAX_CONTEXT;
            int end = fr_end[d] + delta;
            let at = int(fr_positions[base + idx]);
            if (end < at)
            {
                // The nested lookup removed so much that the end would rewind
                // past the position itself; `delta` shrinks with it so the
                // shift below stays consistent (chromium 659496).
                delta += at - end;
                end = at;
            }
            fr_end[d] = end;
            uint count = fr_count[d];
            uint next = idx + 1u;
            if (delta > 0)
            {
                if (uint(delta) + count > MAX_CONTEXT)
                {
                    // HarfBuzz breaks out of the record loop here.
                    fr_record_i[d] = fr_record_count[d];
                    continue;
                }
                for (uint m = count; m > next; --m)
                {
                    fr_positions[base + uint(int(m - 1u) + delta)] = fr_positions[base + m - 1u];
                }
            }
            else
            {
                delta = max(delta, int(next) - int(count));
                next = uint(int(next) - delta);
                for (uint m = next; m < count; ++m)
                {
                    fr_positions[base + uint(int(m) + delta)] = fr_positions[base + m];
                }
            }
            next = uint(int(next) + delta);
            count = uint(int(count) + delta);
            fr_count[d] = count;
            for (uint j = idx + 1u; j < next; ++j)
            {
                fr_positions[base + j] = fr_positions[base + j - 1u] + 1u;
            }
            while (next < count)
            {
                fr_positions[base + next] = uint(int(fr_positions[base + next]) + delta);
                next += 1u;
            }
            continue;
        }

        if (fr_record_i[d] >= fr_record_count[d])
        {
            g_cursor = uint(max(fr_end[d], 0));
            g_lookup_props = fr_saved_props[d];
            g_rules_input = fr_saved_input[d];
            g_rules_context = fr_saved_context[d];
            fr_depth = d;
            continue;
        }

        let record = fr_records[d] + fr_record_i[d] * 4u;
        fr_record_i[d] += 1u;
        let sequence = be16(a, record);
        let nested = be16(a, record + 2u);
        if (sequence >= fr_count[d])
        {
            continue;
        }
        let position = fr_positions[d * MAX_CONTEXT + sequence];
        if (position >= g_len)
        {
            continue;
        }
        if (!charge(1u))
        {
            break;
        }
        if (fr_depth >= MAX_NESTING)
        {
            // HarfBuzz gives up on the whole run here; so does this.
            g_flags |= SHAPE_RUN_UNSUPPORTED;
            g_ok = false;
            break;
        }
        g_cursor = position;
        fr_pending[d] = sequence;
        fr_pending_len[d] = g_len;
        have_pending = true;
        pending_lookup = nested;
        pending_input = g_rules_input;
        pending_context = g_rules_context;
    }
    return applied;
}

/// One lookup over the whole buffer: HarfBuzz's `apply_forward`, or
/// `apply_backward` for the one reverse type.
void applyLookupPass(OtArena a, OtFace face, uint table, uint lookup_index, bool gpos,
                     uint rules_input, uint rules_context)
{
    let lookups = layoutLookups(a, table);
    let lookup = lookupAt(a, lookups, lookup_index);
    if (lookup == 0u || g_len == 0u)
    {
        return;
    }
    let props = lookupFlag(a, lookup) | (markFilteringSet(a, lookup) << 16u);
    let reverse = !gpos && lookupType(a, lookup) == 8u;

    cacheLookup(a, lookups, lookup_index, gpos);
    let lookup_lo = sb_union_lo;
    let lookup_hi = sb_union_hi;
    if (lookup_lo > lookup_hi)
    {
        // No subtable of this lookup covers anything this module can read.
        return;
    }

    // Forwards and backwards share the body, so that `applyLookupTree` — and
    // the whole subtable dispatch inside it — is inlined once.
    g_cursor = reverse ? g_len - 1u : 0u;
    uint step = 0u;
    while (g_ok)
    {
        if (!reverse && g_cursor >= g_len)
        {
            break;
        }
        if (reverse && step >= g_len)
        {
            break;
        }
        let at = g_cursor;
        bool applied = false;
        let gid = at < g_len ? g_gid[at] : 0u;
        if (at < g_len && gid >= lookup_lo && gid <= lookup_hi
            && checkGlyphProperty(a, face, g_props[at], gid, props))
        {
            applied = applyLookupTree(a, face, lookups, lookup_index, gpos,
                                      rules_input, rules_context);
        }
        if (reverse)
        {
            // A reverse lookup never moves the cursor and never recurses.
            step += 1u;
            if (at == 0u)
            {
                break;
            }
            g_cursor = at - 1u;
            continue;
        }
        // The apply moved the cursor itself. A lookup that claims to have
        // applied and left the cursor where it was would loop for ever, so the
        // cursor is nudged.
        if (!applied || g_cursor <= at)
        {
            g_cursor = at + 1u;
        }
    }
    g_cursor = 0u;
}

// ---------------------------------------------------------------------------
// Feature selection
// ---------------------------------------------------------------------------

/// The features HarfBuzz's default shaper enables for horizontal LTR text: the
/// direction pair, the seven common ones and the seven horizontal ones.
///
/// `rvrn` is not in this list because it is a **stage of its own** — HarfBuzz
/// puts a GSUB pause right after it — so its lookups run before every other
/// GSUB lookup whatever their indices. The rest share one stage and are applied
/// in lookup-index order.
///
/// The features HarfBuzz also enables and this list leaves out are the ones no
/// OpenType font has (`Harf`, `HARF`, `Buzz`, `BUZZ`), the ones this shaper
/// has no use for yet (`rand`, `trak`), and `frac`/`numr`/`dnom`, whose mask needs the
/// General_Category table, which is not on the GPU yet.
static const uint FEATURE_COUNT = 16u;

uint defaultFeature(uint i)
{
    switch (i)
    {
    case 0u: return otTag(0x6Cu, 0x74u, 0x72u, 0x61u);  // ltra
    case 1u: return otTag(0x6Cu, 0x74u, 0x72u, 0x6Du);  // ltrm
    case 2u: return otTag(0x61u, 0x62u, 0x76u, 0x6Du);  // abvm
    case 3u: return otTag(0x62u, 0x6Cu, 0x77u, 0x6Du);  // blwm
    case 4u: return otTag(0x63u, 0x63u, 0x6Du, 0x70u);  // ccmp
    case 5u: return otTag(0x6Cu, 0x6Fu, 0x63u, 0x6Cu);  // locl
    case 6u: return otTag(0x6Du, 0x61u, 0x72u, 0x6Bu);  // mark
    case 7u: return otTag(0x6Du, 0x6Bu, 0x6Du, 0x6Bu);  // mkmk
    case 8u: return otTag(0x72u, 0x6Cu, 0x69u, 0x67u);  // rlig
    case 9u: return otTag(0x63u, 0x61u, 0x6Cu, 0x74u);  // calt
    case 10u: return otTag(0x63u, 0x6Cu, 0x69u, 0x67u); // clig
    case 11u: return otTag(0x63u, 0x75u, 0x72u, 0x73u); // curs
    case 12u: return otTag(0x64u, 0x69u, 0x73u, 0x74u); // dist
    case 13u: return otTag(0x6Bu, 0x65u, 0x72u, 0x6Eu); // kern
    case 14u: return otTag(0x6Cu, 0x69u, 0x67u, 0x61u); // liga
    default: return otTag(0x72u, 0x63u, 0x6Cu, 0x74u);  // rclt
    }
}

/// The tag `rvrn` wears, which is GSUB stage 0 all by itself.
uint rvrnTag()
{
    return otTag(0x72u, 0x76u, 0x72u, 0x6Eu);
}

/// What this run says about `tag`: 1 forced on, 0 forced off, -1 not named.
///
/// HarfBuzz's user features are richer — a value, a start and an end — and this
/// is the subset a conformance harness needs: global over the run, and on or
/// off.
int userFeature(uint tag)
{
    for (uint i = 0u; i < uf_count; ++i)
    {
        let word = uf_word[i];
        if ((word & ~SHAPE_FEATURE_ON) == tag)
        {
            return (word & SHAPE_FEATURE_ON) != 0u ? 1 : 0;
        }
    }
    return -1;
}

/// The two tags HarfBuzz marks F_GLOBAL_MANUAL_JOINERS: the only features whose
/// lookups do *not* skip ZWJ while matching their input.
bool manualJoiners(uint tag)
{
    return tag == otTag(0x6Du, 0x61u, 0x72u, 0x6Bu)  // mark
        || tag == otTag(0x6Du, 0x6Bu, 0x6Du, 0x6Bu); // mkmk
}

/// The script tag for a run, from a **range classifier** — a placeholder for
/// the Unicode Script property table, which is not on the GPU yet.
///
/// It answers what HarfBuzz would answer for the script it guesses from the
/// first strong character. A string of Common characters (a clock, a
/// percentage) answers 0, which `selectScript` turns into the same `DFLT`
/// fallback HarfBuzz's unfindable `zyyy` produces.
uint scriptTagOf(uint codepoint)
{
    if ((codepoint >= 0x0041u && codepoint <= 0x005Au)
        || (codepoint >= 0x0061u && codepoint <= 0x007Au)
        || (codepoint >= 0x00C0u && codepoint <= 0x024Fu)
        || (codepoint >= 0x1E00u && codepoint <= 0x1EFFu))
    {
        return otTag(0x6Cu, 0x61u, 0x74u, 0x6Eu); // latn
    }
    if ((codepoint >= 0x0370u && codepoint <= 0x03FFu)
        || (codepoint >= 0x1F00u && codepoint <= 0x1FFFu))
    {
        return otTag(0x67u, 0x72u, 0x65u, 0x6Bu); // grek
    }
    if ((codepoint >= 0x0400u && codepoint <= 0x052Fu)
        || (codepoint >= 0x2DE0u && codepoint <= 0x2DFFu))
    {
        return otTag(0x63u, 0x79u, 0x72u, 0x6Cu); // cyrl
    }
    if (codepoint >= 0x0590u && codepoint <= 0x05FFu)
    {
        return otTag(0x68u, 0x65u, 0x62u, 0x72u); // hebr
    }
    if ((codepoint >= 0x0600u && codepoint <= 0x06FFu)
        || (codepoint >= 0x0750u && codepoint <= 0x077Fu)
        || (codepoint >= 0xFB50u && codepoint <= 0xFEFCu))
    {
        return otTag(0x61u, 0x72u, 0x61u, 0x62u); // arab
    }
    if (codepoint >= 0x0900u && codepoint <= 0x097Fu)
    {
        return otTag(0x64u, 0x65u, 0x76u, 0x61u); // deva
    }
    if (codepoint >= 0x0E00u && codepoint <= 0x0E7Fu)
    {
        return otTag(0x74u, 0x68u, 0x61u, 0x69u); // thai
    }
    if ((codepoint >= 0x1100u && codepoint <= 0x11FFu)
        || (codepoint >= 0xAC00u && codepoint <= 0xD7AFu))
    {
        return otTag(0x68u, 0x61u, 0x6Eu, 0x67u); // hang
    }
    if ((codepoint >= 0x3040u && codepoint <= 0x30FFu)
        || (codepoint >= 0x31F0u && codepoint <= 0x31FFu))
    {
        return otTag(0x6Bu, 0x61u, 0x6Eu, 0x61u); // kana
    }
    if ((codepoint >= 0x3400u && codepoint <= 0x4DBFu)
        || (codepoint >= 0x4E00u && codepoint <= 0x9FFFu)
        || (codepoint >= 0xF900u && codepoint <= 0xFAFFu)
        || (codepoint >= 0x20000u && codepoint <= 0x2FA1Fu))
    {
        return otTag(0x68u, 0x61u, 0x6Eu, 0x69u); // hani
    }
    return 0u;
}

/// HarfBuzz's `hb_ot_layout_table_select_script`: the run's tag, then `DFLT`,
/// then `dflt`, then `latn`. A Script table's offset, or 0.
uint selectScript(OtArena a, uint table, uint tag)
{
    let scripts = layoutScripts(a, table);
    if (scripts == 0u)
    {
        return 0u;
    }
    if (tag != 0u)
    {
        let found = scriptFor(a, scripts, tag);
        if (found != 0u)
        {
            return found;
        }
    }
    let upper = scriptFor(a, scripts, otTag(0x44u, 0x46u, 0x4Cu, 0x54u)); // DFLT
    if (upper != 0u)
    {
        return upper;
    }
    let lower = scriptFor(a, scripts, otTag(0x64u, 0x66u, 0x6Cu, 0x74u)); // dflt
    if (lower != 0u)
    {
        return lower;
    }
    return scriptFor(a, scripts, otTag(0x6Cu, 0x61u, 0x74u, 0x6Eu)); // latn
}

/// One lookup into the sorted collection.
///
/// HarfBuzz merges duplicates with `mask |=`, `auto_zwnj &=`, `auto_zwj &=`, so
/// a lookup reached from both `mark` and `liga` takes `mark`'s stricter rules.
/// With every feature global there is no mask left to merge: HarfBuzz's is
/// `1 << 31` on every lookup and every glyph, and the test is a tautology.
void collectOne(uint lookup, bool auto_zwnj, bool auto_zwj, bool gpos)
{
    // GPOS ignores both joiners and the hidden glyphs whatever the feature
    // said; GSUB's input matcher never ignores ZWNJ, and its context matcher
    // always ignores ZWJ.
    let rules_input = gpos ? (RULES_ZWNJ | RULES_ZWJ | RULES_HIDDEN)
                           : (auto_zwj ? RULES_ZWJ : 0u);
    let rules_context = gpos ? (RULES_ZWNJ | RULES_ZWJ | RULES_HIDDEN)
                             : (RULES_ZWJ | (auto_zwnj ? RULES_ZWNJ : 0u));

    uint slot = 0u;
    while (slot < co_count && co_index[slot] < lookup)
    {
        slot += 1u;
    }
    if (slot < co_count && co_index[slot] == lookup)
    {
        co_rules_input[slot] &= rules_input;
        co_rules_context[slot] &= rules_context;
        return;
    }
    if (co_count >= MAX_LOOKUPS)
    {
        g_flags |= SHAPE_RUN_UNSUPPORTED;
        return;
    }
    for (uint m = co_count; m > slot; --m)
    {
        co_index[m] = co_index[m - 1u];
        co_rules_input[m] = co_rules_input[m - 1u];
        co_rules_context[m] = co_rules_context[m - 1u];
    }
    co_index[slot] = lookup;
    co_rules_input[slot] = rules_input;
    co_rules_context[slot] = rules_context;
    co_count += 1u;
}

/// Collect one stage's lookups from a table into `co_*`, sorted by lookup index.
///
/// `stage` is 0 for `rvrn` (and the required feature, unless its tag is one of
/// the enabled ones) and 1 for everything else. GPOS has one stage and passes 1.
void collectStage(OtArena a, uint table, uint script_tag, uint stage, bool gpos)
{
    co_count = 0u;
    if (table == 0u)
    {
        return;
    }
    let script = selectScript(a, table, script_tag);
    let langsys = defaultLangSys(a, script);
    if (langsys == 0u)
    {
        return;
    }
    let features = layoutFeatures(a, table);
    // Both of this function's list lengths come out of the arena, and their
    // product is what an all-0xFF arena would turn into four billion steps.
    let listed = otTake(langSysFeatureCount(a, langsys), MAX_LIST);
    let required = langSysRequired(a, langsys);

    // The required feature: stage 0 unless a collected feature shares its tag.
    if (required != 0xFFFFu)
    {
        let tag = featureTag(a, features, required);
        bool enabled = false;
        for (uint f = 0u; f < FEATURE_COUNT; ++f)
        {
            if (defaultFeature(f) == tag)
            {
                enabled = true;
                break;
            }
        }
        let required_stage = (enabled || tag == rvrnTag() || gpos) ? 1u : 0u;
        if (required_stage == stage)
        {
            let feature = featureTable(a, features, required);
            let lookups = otTake(featureLookupCount(a, feature), MAX_LIST);
            for (uint l = 0u; l < lookups; ++l)
            {
                collectOne(featureLookup(a, feature, l), true, true, gpos);
            }
        }
    }

    for (uint i = 0u; i < listed; ++i)
    {
        let index = langSysFeature(a, langsys, i);
        let tag = featureTag(a, features, index);
        bool wanted = false;
        bool joiners = true;
        if (stage == 0u && !gpos)
        {
            wanted = tag == rvrnTag();
        }
        else
        {
            for (uint f = 0u; f < FEATURE_COUNT; ++f)
            {
                if (defaultFeature(f) == tag)
                {
                    wanted = true;
                    joiners = !manualJoiners(tag);
                    break;
                }
            }
            // The run's own features, added after the shaper's as HarfBuzz
            // adds them. Forced off is a mask of zero over there, which
            // is a lookup that never applies — so it is simply not collected
            // here. A tag that is both defaulted and forced keeps the default's
            // joiner rules, because HarfBuzz merges the two map entries with
            // `auto_zwnj &=` and a user feature's are both true.
            let forced = userFeature(tag);
            if (forced == 0)
            {
                wanted = false;
            }
            else if (forced == 1)
            {
                wanted = true;
            }
        }
        if (!wanted)
        {
            continue;
        }
        let feature = featureTable(a, features, index);
        let lookups = otTake(featureLookupCount(a, feature), MAX_LIST);
        for (uint l = 0u; l < lookups; ++l)
        {
            collectOne(featureLookup(a, feature, l), joiners, joiners, gpos);
        }
    }
}

// ---------------------------------------------------------------------------
// Unicode facts this file needs, until the property tables reach the GPU
// ---------------------------------------------------------------------------

/// Whether a codepoint is Default_Ignorable_Code_Point, as Unicode defines the
/// property — zero-width spaces and joiners, soft hyphen, the bidi controls,
/// the variation selectors, the tag characters.
///
/// The ranges are the property's, written out; nothing is ported. A property
/// table packed on the CPU and uploaded beside the arena is meant to replace
/// this (docs/roadmap.md, *Unicode*).
bool isDefaultIgnorable(uint codepoint)
{
    let plane = codepoint >> 16u;
    if (plane == 0u)
    {
        switch (codepoint >> 8u)
        {
        case 0x00u: return codepoint == 0x00ADu;
        case 0x03u: return codepoint == 0x034Fu;
        case 0x06u: return codepoint == 0x061Cu;
        case 0x17u: return codepoint >= 0x17B4u && codepoint <= 0x17B5u;
        case 0x18u: return codepoint >= 0x180Bu && codepoint <= 0x180Fu;
        case 0x20u:
            return (codepoint >= 0x200Bu && codepoint <= 0x200Fu)
                || (codepoint >= 0x202Au && codepoint <= 0x202Eu)
                || (codepoint >= 0x2060u && codepoint <= 0x206Fu);
        case 0xFEu:
            return (codepoint >= 0xFE00u && codepoint <= 0xFE0Fu) || codepoint == 0xFEFFu;
        case 0xFFu: return codepoint >= 0xFFF0u && codepoint <= 0xFFF8u;
        default: return false;
        }
    }
    if (plane == 1u)
    {
        return (codepoint >= 0x1BCA0u && codepoint <= 0x1BCA3u)
            || (codepoint >= 0x1D173u && codepoint <= 0x1D17Au);
    }
    if (plane == 0x0Eu)
    {
        return codepoint >= 0xE0000u && codepoint <= 0xE0FFFu;
    }
    return false;
}

/// Whether a codepoint is a **non-spacing mark** (General_Category Mn), which is
/// the only thing HarfBuzz's synthesized glyph classes need to know.
///
/// A placeholder with the same status as `scriptTagOf`: the ranges below are
/// the combining blocks and the per-script combining runs, written out, until
/// a property table packed on the CPU replaces the lot. It is
/// consulted only for a face with **no GlyphClassDef**, which no common UI
/// font is; HarfBuzz's corpus has several, and without this their marks keep
/// their advances and hang off nothing.
bool isNonSpacingMark(uint codepoint)
{
    return (codepoint >= 0x0300u && codepoint <= 0x036Fu)
        || (codepoint >= 0x0483u && codepoint <= 0x0487u)
        || (codepoint >= 0x0591u && codepoint <= 0x05BDu)
        || codepoint == 0x05BFu
        || (codepoint >= 0x05C1u && codepoint <= 0x05C2u)
        || (codepoint >= 0x05C4u && codepoint <= 0x05C5u)
        || codepoint == 0x05C7u
        || (codepoint >= 0x0610u && codepoint <= 0x061Au)
        || (codepoint >= 0x064Bu && codepoint <= 0x065Fu)
        || codepoint == 0x0670u
        || (codepoint >= 0x06D6u && codepoint <= 0x06DCu)
        || (codepoint >= 0x06DFu && codepoint <= 0x06E4u)
        || (codepoint >= 0x06E7u && codepoint <= 0x06E8u)
        || (codepoint >= 0x06EAu && codepoint <= 0x06EDu)
        || codepoint == 0x0711u
        || (codepoint >= 0x0730u && codepoint <= 0x074Au)
        || (codepoint >= 0x07A6u && codepoint <= 0x07B0u)
        || (codepoint >= 0x0816u && codepoint <= 0x0819u)
        || (codepoint >= 0x081Bu && codepoint <= 0x0823u)
        || (codepoint >= 0x0825u && codepoint <= 0x0827u)
        || (codepoint >= 0x0829u && codepoint <= 0x082Du)
        || (codepoint >= 0x0951u && codepoint <= 0x0957u)
        || (codepoint >= 0x1AB0u && codepoint <= 0x1AFFu)
        || (codepoint >= 0x1DC0u && codepoint <= 0x1DFFu)
        || (codepoint >= 0x20D0u && codepoint <= 0x20DCu)
        || codepoint == 0x20E1u
        || (codepoint >= 0x20E5u && codepoint <= 0x20F0u)
        || (codepoint >= 0xFE20u && codepoint <= 0xFE2Fu);
}

/// Whether a codepoint is **Extended_Pictographic**, the emoji property
/// `form_clusters` needs for one rule and one rule only: a ZWJ binds the
/// pictograph after it into the grapheme cluster before it.
///
/// A placeholder with the same status as `scriptTagOf` and `isNonSpacingMark`:
/// the ranges are the property's, written out, until a property table packed
/// on the CPU replaces the lot.
bool isExtendedPictographic(uint c)
{
    if (c < 0x2000u)
    {
        return c == 0x00A9u || c == 0x00AEu;
    }
    if (c < 0x10000u)
    {
        return c == 0x203Cu || c == 0x2049u || c == 0x2122u || c == 0x2139u
            || (c >= 0x2194u && c <= 0x2199u)
            || (c >= 0x21A9u && c <= 0x21AAu)
            || (c >= 0x231Au && c <= 0x231Bu)
            || c == 0x2328u || c == 0x2388u || c == 0x23CFu
            || (c >= 0x23E9u && c <= 0x23F3u)
            || (c >= 0x23F8u && c <= 0x23FAu)
            || c == 0x24C2u
            || (c >= 0x25AAu && c <= 0x25ABu)
            || c == 0x25B6u || c == 0x25C0u
            || (c >= 0x25FBu && c <= 0x25FEu)
            || (c >= 0x2600u && c <= 0x2605u)
            || (c >= 0x2607u && c <= 0x2612u)
            || (c >= 0x2614u && c <= 0x2685u)
            || (c >= 0x2690u && c <= 0x2705u)
            || (c >= 0x2708u && c <= 0x2712u)
            || c == 0x2714u || c == 0x2716u || c == 0x271Du || c == 0x2721u || c == 0x2728u
            || (c >= 0x2733u && c <= 0x2734u)
            || c == 0x2744u || c == 0x2747u || c == 0x274Cu || c == 0x274Eu
            || (c >= 0x2753u && c <= 0x2755u)
            || c == 0x2757u
            || (c >= 0x2763u && c <= 0x2767u)
            || (c >= 0x2795u && c <= 0x2797u)
            || c == 0x27A1u || c == 0x27B0u || c == 0x27BFu
            || (c >= 0x2934u && c <= 0x2935u)
            || (c >= 0x2B05u && c <= 0x2B07u)
            || (c >= 0x2B1Bu && c <= 0x2B1Cu)
            || c == 0x2B50u || c == 0x2B55u
            || c == 0x3030u || c == 0x303Du || c == 0x3297u || c == 0x3299u;
    }
    return (c >= 0x1F000u && c <= 0x1F0FFu)
        || (c >= 0x1F10Du && c <= 0x1F10Fu)
        || c == 0x1F12Fu
        || (c >= 0x1F16Cu && c <= 0x1F171u)
        || (c >= 0x1F17Eu && c <= 0x1F17Fu)
        || c == 0x1F18Eu
        || (c >= 0x1F191u && c <= 0x1F19Au)
        || (c >= 0x1F1ADu && c <= 0x1F1E5u)
        || (c >= 0x1F201u && c <= 0x1F20Fu)
        || c == 0x1F21Au || c == 0x1F22Fu
        || (c >= 0x1F232u && c <= 0x1F23Au)
        || (c >= 0x1F23Cu && c <= 0x1F23Fu)
        || (c >= 0x1F249u && c <= 0x1F3FAu)
        || (c >= 0x1F400u && c <= 0x1F53Du)
        || (c >= 0x1F546u && c <= 0x1F64Fu)
        || (c >= 0x1F680u && c <= 0x1F6FFu)
        || (c >= 0x1F774u && c <= 0x1F77Fu)
        || (c >= 0x1F7D5u && c <= 0x1F7FFu)
        || (c >= 0x1F80Cu && c <= 0x1F80Fu)
        || (c >= 0x1F848u && c <= 0x1F84Fu)
        || (c >= 0x1F85Au && c <= 0x1F85Fu)
        || (c >= 0x1F888u && c <= 0x1F88Fu)
        || (c >= 0x1F8AEu && c <= 0x1F8FFu)
        || (c >= 0x1F90Cu && c <= 0x1F93Au)
        || (c >= 0x1F93Cu && c <= 0x1F945u)
        || (c >= 0x1F947u && c <= 0x1FAFFu)
        || (c >= 0x1FC00u && c <= 0x1FFFDu);
}

/// Whether a codepoint is a **combining mark** (General_Category Mn, Mc or Me),
/// which is what `form_clusters` treats as a continuation of the grapheme
/// before it.
///
/// `isNonSpacingMark` is the Mn half and is a different question — it is what
/// HarfBuzz synthesizes a *glyph class* from for a face with no GlyphClassDef.
/// Both become one General_Category table in the end.
bool isCombiningMark(uint c)
{
    return isNonSpacingMark(c)
        // Me: the enclosing marks.
        || (c >= 0x0488u && c <= 0x0489u)
        || c == 0x1ABEu
        || (c >= 0x20DDu && c <= 0x20E0u)
        || (c >= 0x20E2u && c <= 0x20E4u)
        || (c >= 0xA670u && c <= 0xA672u)
        // Mn ranges the synthesis list has no use for and this one does.
        || (c >= 0x0591u && c <= 0x05BDu)
        || (c >= 0x07EBu && c <= 0x07F3u)
        || c == 0x07FDu
        || (c >= 0x135Du && c <= 0x135Fu)
        || (c >= 0x1CD0u && c <= 0x1CE8u)
        || (c >= 0x2CEFu && c <= 0x2CF1u)
        || (c >= 0x302Au && c <= 0x302Fu)
        || c == 0xA66Fu
        || (c >= 0xA674u && c <= 0xA67Du)
        || (c >= 0xA69Eu && c <= 0xA69Fu)
        || (c >= 0xFE00u && c <= 0xFE0Fu)
        || (c >= 0x101FDu && c <= 0x101FDu)
        || (c >= 0x1D165u && c <= 0x1D169u)
        || (c >= 0x1D16Du && c <= 0x1D172u)
        || (c >= 0x1D17Bu && c <= 0x1D182u)
        || (c >= 0x1D185u && c <= 0x1D18Bu)
        || (c >= 0x1D1AAu && c <= 0x1D1ADu)
        || (c >= 0x1E8D0u && c <= 0x1E8D6u);
}

/// HarfBuzz's *hidden* set: CGJ, the Mongolian free variation selectors and the
/// tag characters, which GPOS ignores and GSUB does not.
bool isHidden(uint codepoint)
{
    return codepoint == 0x034Fu
        || (codepoint >= 0x180Bu && codepoint <= 0x180Du)
        || codepoint == 0x180Fu
        || (codepoint >= 0xE0020u && codepoint <= 0xE007Fu);
}

/// Decode one UTF-8 sequence at `offset`, within `end`.
///
/// `consumed` is how many bytes to skip, always at least 1 — an ill-formed
/// sequence yields U+FFFD and consumes one byte, so the loop above always
/// advances whatever the bytes say.
uint textWord(ShapeTables t, uint at)
{
    if (t.source == 1u)
    {
        return t.utf8_alt0[at];
    }
    if (t.source == 2u)
    {
        return t.utf8_alt1[at];
    }
    return t.utf8[at];
}

uint decodeUtf8(ShapeTables t, uint offset, uint end, out uint consumed, out bool ok)
{
    consumed = 1u;
    ok = true;
    let b0 = (textWord(t, offset >> 2u) >> ((offset & 3u) * 8u)) & 0xFFu;
    if (b0 < 0x80u)
    {
        return b0;
    }

    uint needed = 0u;
    uint value = 0u;
    if ((b0 & 0xE0u) == 0xC0u)
    {
        needed = 1u;
        value = b0 & 0x1Fu;
    }
    else if ((b0 & 0xF0u) == 0xE0u)
    {
        needed = 2u;
        value = b0 & 0x0Fu;
    }
    else if ((b0 & 0xF8u) == 0xF0u)
    {
        needed = 3u;
        value = b0 & 0x07u;
    }
    else
    {
        ok = false;
        return 0xFFFDu;
    }

    if (offset + needed >= end)
    {
        ok = false;
        return 0xFFFDu;
    }
    for (uint i = 1u; i <= needed; ++i)
    {
        let at = offset + i;
        let byte = (textWord(t, at >> 2u) >> ((at & 3u) * 8u)) & 0xFFu;
        if ((byte & 0xC0u) != 0x80u)
        {
            ok = false;
            return 0xFFFDu;
        }
        value = (value << 6u) | (byte & 0x3Fu);
    }
    consumed = needed + 1u;
    if (value > 0x10FFFFu || (value >= 0xD800u && value <= 0xDFFFu))
    {
        ok = false;
        return 0xFFFDu;
    }
    return value;
}

// ---------------------------------------------------------------------------
// The final positioning passes
// ---------------------------------------------------------------------------

/// `propagate_attachment_offsets` for one glyph: fold the chain it hangs off
/// into its own offsets. Recursive in HarfBuzz; a walk and an unwind here.
void propagateAttachment(uint start)
{
    uint nodes[MAX_ATTACH];
    uint parents[MAX_ATTACH];
    uint types[MAX_ATTACH];
    uint depth = 0u;
    uint at = start;
    while (depth < MAX_ATTACH)
    {
        let word = g_attach[at];
        let chain = attachChain(word);
        if (chain == 0)
        {
            break;
        }
        // Clearing the chain is the memoisation: a glyph is folded once.
        g_attach[at] = packAttach(attachType(word), 0);
        let next = int(at) + chain;
        if (next < 0 || uint(next) >= g_len)
        {
            break;
        }
        nodes[depth] = at;
        parents[depth] = uint(next);
        types[depth] = attachType(word);
        depth += 1u;
        at = uint(next);
    }
    while (depth > 0u)
    {
        depth -= 1u;
        let node = nodes[depth];
        let parent = parents[depth];
        if ((types[depth] & ATTACH_CURSIVE) != 0u)
        {
            g_yoff[node] += g_yoff[parent];
        }
        else if ((types[depth] & ATTACH_MARK) != 0u)
        {
            g_xoff[node] += g_xoff[parent];
            g_yoff[node] += g_yoff[parent];
            // A mark carries back over the advances of everything between it
            // and what it is attached to.
            for (uint k = parent; k < node; ++k)
            {
                g_xoff[node] -= g_xadv[k];
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Which face draws a codepoint
// ---------------------------------------------------------------------------

/// Which face in the directory draws a codepoint: the first whose `cmap` has
/// it, so the directory's order is the fallback order. `local` comes back as that face's own
/// glyph id.
///
/// A codepoint no face has answers face 0 and glyph 0, which is `.notdef`, and
/// the run's `SHAPE_RUN_NOTDEF` is what tells the host to open a face that
/// covers it.
uint faceFor(OtArena a, uint face_count, uint codepoint, out uint local)
{
    // One op a face tried, because `otGlyphFor` walks that face's whole
    // `cmap` subtable list and then a binary search: sixteen faces per
    // codepoint is the loop an all-0xFF arena made expensive.
    //
    // **The charge is not the loop's exit**, and that is load-bearing. With
    // this header written as `i < face_count && charge(1u)`, RADV compiled the
    // loop so that it ran **no iteration at all**: `faceFor` fell straight through to its
    // `local = 0u`, and the first codepoint of every run came back `.notdef`.
    // The SPIR-V is identical to
    // the one `shape/examples/conform.rs` dispatches, which is right on the
    // same GPU, and lavapipe draws it correctly; what does not survive is a
    // loop whose exit is a budget the loop itself spends, inlined this deep.
    //
    // Nothing is lost by taking it out. `face_count` is `otHeader`'s, and
    // `otHeader` has already capped it at `MAX_OT_FACES`, so this loop is
    // bounded by a **constant** whatever the arena says — the budget never had
    // to stop it. The op still goes on the tab, because the tab is what bounds
    // the rest of the run.
    for (uint i = 0u; i < face_count; ++i)
    {
        charge(1u);
        let face = otFaceAt(a, i);
        let gid = otGlyphFor(a, face, codepoint);
        if (gid != 0u && gid < face.glyph_count)
        {
            local = gid;
            return i;
        }
    }
    local = 0u;
    return 0u;
}

// ---------------------------------------------------------------------------
// The stages
//
// A string is shaped in four dispatches, each its own entry point and
// pipeline, in this order over the whole batch:
//
//   1. `shapeDecode`     UTF-8, the face segments, `cmap`, GDEF classes;
//   2. `shapeSubstitute` GSUB stage 0 (`rvrn`) and stage 1, per segment;
//   3. `shapePosition`   advances and GPOS, per segment;
//   4. `shapeFinish`     mark advances, default ignorables, attachment
//                        chains, 26.6 output, the run record.
//
// What one stage leaves for the next is in `ShapeTables.scratch[slot]`: the
// glyph arrays of every segment back to back, the segment table, and the
// budget, flags and depth the string has spent so far. Each stage loads one
// segment into the `g_*` arrays, works on it there, and stores it back, so
// every walker above sees exactly one segment in `g_*`, as it would in one
// pass.
//
// The two GSUB stages share a pipeline. Splitting them would compile every
// GSUB walker twice and shorten neither pipeline.
// ---------------------------------------------------------------------------

/// `ShapeScratch.segment[s]`: the segment's glyph count, its face, and whether
/// its lookups were stopped (HarfBuzz's `buffer->successful`).
static const uint SEG_LEN_MASK = 0xFFFFu;
static const uint SEG_FACE_SHIFT = 16u;
static const uint SEG_FACE_MASK = 0xFFu;
static const uint SEG_STOPPED = 0x01000000u;

/// One slot of the buffer as the scratch keeps it: one record per glyph,
/// written and read whole.
ShapeScratchGlyph scratchGlyph(uint i)
{
    ShapeScratchGlyph g;
    g.gid = g_gid[i];
    g.cluster = g_cluster[i];
    g.props = g_props[i];
    g.lig = g_lig[i];
    g.xadv = g_xadv[i];
    g.xoff = g_xoff[i];
    g.yoff = g_yoff[i];
    g.attach = g_attach[i];
    return g;
}

void fromScratchGlyph(uint i, ShapeScratchGlyph g)
{
    g_gid[i] = g.gid;
    g_cluster[i] = g.cluster;
    g_props[i] = g.props;
    g_lig[i] = g.lig;
    g_xadv[i] = g.xadv;
    g_xoff[i] = g.xoff;
    g_yoff[i] = g.yoff;
    g_attach[i] = g.attach;
}

uint segmentWord(uint len, uint face, bool ok)
{
    return (len & SEG_LEN_MASK) | ((face & SEG_FACE_MASK) << SEG_FACE_SHIFT)
        | (ok ? 0u : SEG_STOPPED);
}

uint segmentFace(uint word)
{
    return (word >> SEG_FACE_SHIFT) & SEG_FACE_MASK;
}

/// The state every stage starts from: a fresh clock, an empty cache and
/// nesting stack, and the run's forced features out of the dispatch's pool.
/// The budget, flags and depth are a fresh string's; `resumeStage` replaces
/// them with what the earlier stages left.
void beginStage(ShapeParams params, ShapeRequest req)
{
    g_len = 0u;
    g_cursor = 0u;
    g_lookup_props = 0u;
    g_rules_input = 0u;
    g_rules_context = 0u;
    g_ops = MAX_OPS;
    // The wall-clock deadline: one read at the stage's entry, and `charge`
    // re-reads every SHAPE_CLOCK_EVERY ops. Each stage has the whole
    // deadline, because the time between two dispatches is not the run's.
    g_clock_start = getRealtimeClockLow();
    g_deadline = params.deadline_ticks;
    g_clock_step = 0u;
    g_flags = 0u;
    g_depth_seen = 0u;
    g_serial = 0u;
    g_ok = true;
    fr_depth = 0u;
    fr_pushed = false;
    sb_lookups = 0u;
    sb_lookup = 0xFFFFFFFFu;
    sb_count = 0xFFFFFFFFu;

    // The run's forced features, out of the dispatch's pool. A word of zero is
    // an empty slot, because no OpenType tag is zero.
    uf_count = 0u;
    let first = min(req.feature_start, MAX_SHAPE_FEATURES);
    let want = min(req.feature_count, MAX_SHAPE_FEATURES - first);
    for (uint i = 0u; i < want; ++i)
    {
        let at = first + i;
        let word = params.features[at >> 2u][at & 3u];
        if (word != 0u)
        {
            uf_word[uf_count] = word;
            uf_count += 1u;
        }
    }
}

/// What the earlier stages left: the budget is the string's, not the stage's.
void resumeStage(ShapeTables t, uint slot)
{
    g_ops = t.scratch[slot].ops;
    g_flags = t.scratch[slot].flags;
    g_depth_seen = t.scratch[slot].depth;
}

void endStage(ShapeTables t, uint slot)
{
    t.scratch[slot].ops = g_ops;
    t.scratch[slot].flags = g_flags;
    t.scratch[slot].depth = g_depth_seen;
}

/// The GSUB stages one segment runs: stage 0 (`rvrn`), then stage 1.
///
/// A `static` rather than a literal 2 **on purpose**: with a constant trip
/// count Slang unrolls the loop in `shapeSubstitute` and inlines the whole
/// GSUB dispatch — every substitution walker in this file — once per stage.
static uint g_gsub_stages = 2u;

/// Stage 1: decode, cut into faces, map through `cmap`, classify, and run
/// GSUB over each segment as it closes.
///
/// Each segment is built in the buffer from slot 0, substituted with its own
/// face's GSUB, and appended to the scratch's glyph arrays behind the segments
/// before it, with its entry in the segment table.
public void shapeSubstitute(ShapeTables t, ShapeParams params, ShapeRequest req, uint slot)
{
    beginStage(params, req);
    let a = otOpen(t.ot);
    let header = otHeader(a);
    let face_count = header.face_count;
    let end = req.byte_start + req.byte_len;

    uint segments = 0u;
    uint written = 0u;
    uint offset = req.byte_start;
    uint segment_face = 0u;
    uint segment_script = 0u;
    bool have_segment = false;
    bool finished = face_count == 0u;
    bool full = false;

    // `hb_form_clusters`, folded into the decode.
    //
    // HarfBuzz runs it once over the whole buffer before any lookup: every
    // maximal group of one glyph and the **continuations** after it becomes one
    // cluster, so a base and its marks, a ZWJ emoji sequence, a regional-
    // indicator pair and a flag's tag characters all carry the cluster of the
    // codepoint they hang off. Since a group's clusters are increasing, the
    // minimum `merge_clusters` takes is the group's first — so a continuation
    // taking the previous glyph's cluster at push time is the same answer for
    // one compare instead of a pass.
    //
    // The three tests it needs — General_Category Mn/Mc/Me,
    // Extended_Pictographic, and the emoji-modifier and regional-indicator
    // ranges — are written out below as `scriptTagOf` is, until a property
    // table replaces them.
    //
    // It is **per segment**, where HarfBuzz has no segments: a grapheme whose
    // base and mark come from different faces starts a new group here. No
    // common font does that for a script this shaper handles.
    bool prev_continuation = false;
    bool prev_regional = false;
    bool zwj_open = false;

    while (!finished)
    {
        uint codepoint = 0u;
        uint cluster = 0u;
        uint local = 0u;
        uint face_index = 0u;
        bool have_codepoint = false;

        if (offset < end && !full)
        {
            uint consumed;
            bool ok;
            cluster = offset - req.byte_start;
            codepoint = decodeUtf8(t, offset, end, consumed, ok);
            offset += consumed;
            if (!ok)
            {
                g_flags |= SHAPE_RUN_BAD_UTF8;
            }
            have_codepoint = true;
            if (isDefaultIgnorable(codepoint))
            {
                // A joiner belongs to the run around it, not to a face of its
                // own: it takes the current segment's face, or the primary's.
                face_index = have_segment ? segment_face : 0u;
                local = otGlyphFor(a, otFaceAt(a, face_index), codepoint);
            }
            else
            {
                face_index = faceFor(a, face_count, codepoint, local);
            }
        }
        else
        {
            finished = true;
        }

        if ((!have_codepoint || (have_segment && face_index != segment_face))
            && have_segment && g_len > 0u)
        {
            // The segment closes and is substituted.
            let face = otFaceAt(a, segment_face);
            let table = face.gsub_off;
            g_cursor = 0u;
            // One call site for the lookup pass, which is what keeps the GSUB
            // dispatch from being inlined once per stage.
            for (uint stage = 0u; stage < g_gsub_stages; ++stage)
            {
                if (table == 0u || !g_ok)
                {
                    continue;
                }
                collectStage(a, table, segment_script, stage, false);
                let lookups = co_count;
                for (uint i = 0u; i < lookups && g_ok; ++i)
                {
                    applyLookupPass(a, face, table, co_index[i], false,
                                    co_rules_input[i], co_rules_context[i]);
                }
            }
            // Every segment together fits `MAX_RUN_GLYPHS`, the most one run
            // can write; what does not fit is dropped from the end.
            uint keep = 0u;
            for (uint i = 0u; i < g_len; ++i)
            {
                if (written + keep >= MAX_RUN_GLYPHS)
                {
                    g_flags |= SHAPE_RUN_TRUNCATED;
                    break;
                }
                let at = written + keep;
                if (at < MAX_RUN_GLYPHS)
                {
                    t.scratch[slot].glyphs[at & 255u] = scratchGlyph(i & 255u);
                }
                keep += 1u;
            }
            t.scratch[slot].segment[segments] = segmentWord(keep, segment_face, g_ok);
            t.scratch[slot].script[segments] = segment_script;
            segments += 1u;
            written += keep;
            g_len = 0u;
            g_cursor = 0u;
            g_ok = true;
        }

        if (!have_codepoint)
        {
            break;
        }
        if (g_len >= MAX_RUN_GLYPHS)
        {
            // The segment has more codepoints than a run has slots. What is
            // decoded so far is shaped; the rest is not.
            g_flags |= SHAPE_RUN_TRUNCATED;
            full = true;
            continue;
        }
        if (!have_segment || face_index != segment_face)
        {
            if (have_segment)
            {
                g_flags |= SHAPE_RUN_MULTI_FACE;
            }
            segment_face = face_index;
            segment_script = 0u;
            have_segment = true;
        }
        if (segment_script == 0u)
        {
            segment_script = scriptTagOf(codepoint);
        }
        // `form_clusters`' one question, in HarfBuzz's own order of tests. An
        // ASCII codepoint, a letter and a space separator are never
        // continuations, which is the early `continue` in its loop.
        bool continuation = false;
        bool regional = codepoint >= 0x1F1E6u && codepoint <= 0x1F1FFu;
        if (codepoint >= 0x80u)
        {
            if (isCombiningMark(codepoint))
            {
                continuation = true;
            }
            else if (codepoint >= 0x1F3FBu && codepoint <= 0x1F3FFu)
            {
                // Emoji_Modifier: the skin tones.
                continuation = true;
            }
            else if (regional)
            {
                // A regional indicator continues the one before it, but only
                // in pairs — the second of a pair starts the next flag.
                continuation = prev_regional && !prev_continuation;
            }
            else if (codepoint == 0x200Du)
            {
                continuation = true;
            }
            else if ((codepoint >= 0xFF9Eu && codepoint <= 0xFF9Fu)
                     || (codepoint >= 0xE0020u && codepoint <= 0xE007Fu))
            {
                // Other_Grapheme_Extend that is not a mark: the halfwidth
                // katakana sound marks and the tag characters a flag sequence
                // is spelled with. ZWNJ is deliberately **not** here — keeping
                // it separate gives more granular clusters, says HarfBuzz.
                continuation = true;
            }
            else if (zwj_open && isExtendedPictographic(codepoint))
            {
                continuation = true;
            }
        }
        zwj_open = codepoint == 0x200Du;
        prev_regional = regional;

        let slot_at = g_len;
        g_gid[slot_at] = local;
        // A segment's first glyph starts a cluster of its own.
        g_cluster[slot_at] = (continuation && slot_at > 0u) ? g_cluster[slot_at - 1u] : cluster;
        prev_continuation = continuation;
        let segment = otFaceAt(a, face_index);
        uint props;
        if (gdefGlyphClasses(a, segment) != 0u)
        {
            props = glyphProps(a, segment, local);
        }
        else
        {
            // HarfBuzz's `hb_synthesize_glyph_classes`: a face with no
            // GlyphClassDef gets its classes from Unicode instead — every
            // non-spacing mark is a mark, everything else is a base. A
            // default-ignorable is never a mark, whatever its category, because
            // a lookup flag that ignores marks would then skip a joiner
            // (Uniscribe does not, and Mongolian fonts without GDEF rely on it).
            props = isNonSpacingMark(codepoint) && !isDefaultIgnorable(codepoint)
                        ? PROPS_MARK
                        : PROPS_BASE;
        }
        if (isDefaultIgnorable(codepoint))
        {
            props |= PROPS_IGNORABLE;
            if (codepoint == 0x200Cu)
            {
                props |= PROPS_ZWNJ;
            }
            if (codepoint == 0x200Du)
            {
                props |= PROPS_ZWJ;
            }
            if (isHidden(codepoint))
            {
                props |= PROPS_HIDDEN;
            }
        }
        g_props[slot_at] = props;
        g_lig[slot_at] = 0u;
        g_xadv[slot_at] = 0;
        g_xoff[slot_at] = 0;
        g_yoff[slot_at] = 0;
        g_attach[slot_at] = 0u;
        g_len = slot_at + 1u;
    }

    t.scratch[slot].glyph_count = written;
    t.scratch[slot].segment_count = segments;
    endStage(t, slot);
}

/// Stage 3: the advances every glyph starts with, then GPOS, segment by
/// segment. GPOS moves glyphs and never adds or removes one.
public void shapePosition(ShapeTables t, ShapeParams params, ShapeRequest req, uint slot)
{
    beginStage(params, req);
    resumeStage(t, slot);
    let a = otOpen(t.ot);
    let count = min(t.scratch[slot].segment_count, MAX_RUN_GLYPHS);
    let total = min(t.scratch[slot].glyph_count, MAX_RUN_GLYPHS);
    uint base = 0u;
    for (uint s = 0u; s < count; ++s)
    {
        let word = t.scratch[slot].segment[s];
        let len = min(word & SEG_LEN_MASK, total - base);
        let face = otFaceAt(a, segmentFace(word));
        let script_tag = t.scratch[slot].script[s];
        for (uint i = 0u; i < len; ++i)
        {
            fromScratchGlyph(i, t.scratch[slot].glyphs[base + i]);
            g_xadv[i] = int(otAdvance(a, face, g_gid[i]));
            g_xoff[i] = 0;
            g_yoff[i] = 0;
            g_attach[i] = 0u;
        }
        g_len = len;
        g_cursor = 0u;
        g_ok = (word & SEG_STOPPED) == 0u;
        let table = face.gpos_off;
        if (table != 0u && g_ok)
        {
            collectStage(a, table, script_tag, 1u, true);
            let lookups = co_count;
            for (uint i = 0u; i < lookups && g_ok; ++i)
            {
                applyLookupPass(a, face, table, co_index[i], true,
                                co_rules_input[i], co_rules_context[i]);
            }
        }
        for (uint i = 0u; i < len; ++i)
        {
            t.scratch[slot].glyphs[base + i] = scratchGlyph(i);
        }
        base += len;
    }
    endStage(t, slot);
}

/// The passes HarfBuzz runs after the lookups, then the segment written out in
/// 26.6 at the request's size. Returns the segment's advance in 26.6;
/// `written` moves on by the glyphs it emitted.
int finishSegment(
    OtArena a,
    OtFace face,
    ShapeRequest req,
    int pen_26_6,
    inout uint written,
    uint cap,
    RWStructuredBuffer<ShapedGlyph> out_glyphs,
    uint glyph_capacity)
{
    // In HarfBuzz's order: mark widths zeroed by GDEF class, then default
    // ignorables zeroed, then the attachment chains folded in.
    for (uint i = 0u; i < g_len; ++i)
    {
        if ((g_props[i] & PROPS_MARK) != 0u)
        {
            g_xadv[i] = 0;
        }
        if ((g_props[i] & PROPS_IGNORABLE) != 0u && (g_props[i] & PROPS_SUBSTITUTED) == 0u)
        {
            g_xadv[i] = 0;
        }
    }
    for (uint i = 0u; i < g_len; ++i)
    {
        propagateAttachment(i);
    }

    let space = otGlyphFor(a, face, 0x20u);
    let scale = req.px_size / float(max(face.upem, 1u)) * 64.0;
    int pen = 0;
    for (uint i = 0u; i < g_len; ++i)
    {
        uint glyph = g_gid[i];
        uint flags = 0u;
        let props = g_props[i];
        let hidden = (props & PROPS_IGNORABLE) != 0u && (props & PROPS_SUBSTITUTED) == 0u;
        if (hidden)
        {
            // `hide_default_ignorables`: the id becomes the space, and the
            // advance was zeroed just above.
            glyph = space;
            flags |= SHAPE_GLYPH_IGNORABLE;
        }
        if (glyph == NOTDEF_GLYPH && !hidden)
        {
            flags |= SHAPE_GLYPH_NOTDEF;
            g_flags |= SHAPE_RUN_NOTDEF;
        }
        else if (!otFilled(a, face, glyph))
        {
            // A reserved range whose curves have not arrived: it draws blank,
            // and the run asks for cover.
            flags |= SHAPE_GLYPH_UNFILLED;
            g_flags |= SHAPE_RUN_NOTDEF;
        }
        if ((props & PROPS_SUBSTITUTED) != 0u)
        {
            flags |= SHAPE_GLYPH_SUBSTITUTED;
        }
        if ((props & PROPS_LIGATED) != 0u)
        {
            flags |= SHAPE_GLYPH_LIGATURE;
        }
        if ((props & PROPS_MARK) != 0u)
        {
            flags |= SHAPE_GLYPH_MARK;
        }
        if (g_xoff[i] != 0 || g_yoff[i] != 0)
        {
            flags |= SHAPE_GLYPH_POSITIONED;
        }

        if (written >= cap)
        {
            g_flags |= SHAPE_RUN_TRUNCATED;
            break;
        }
        let slot = req.out_start + written;
        if (slot < glyph_capacity)
        {
            ShapedGlyph shaped;
            shaped.gid = face.glyph_base + glyph;
            shaped.x = pen_26_6 + roundAway(float(pen + g_xoff[i]) * scale);
            shaped.y = roundAway(float(g_yoff[i]) * scale);
            shaped.cluster = g_cluster[i];
            shaped.flags = flags;
            shaped._rsvd0 = 0u;
            shaped._rsvd1 = 0u;
            shaped._rsvd2 = 0u;
            out_glyphs[slot] = shaped;
        }
        else
        {
            g_flags |= SHAPE_RUN_TRUNCATED;
        }
        written += 1u;
        pen += g_xadv[i];
    }
    return roundAway(float(pen) * scale);
}

/// Stage 4: the final passes and the output. Returns the run record; the
/// glyphs go to `out_glyphs[req.out_start ..]`.
///
/// A segment boundary is where the pen crosses from one face's design units to
/// the next face's, so each segment is scaled by its own upem.
public ShapeRun shapeFinish(
    ShapeTables t,
    ShapeParams params,
    ShapeRequest req,
    uint slot,
    RWStructuredBuffer<ShapedGlyph> out_glyphs,
    uint glyph_capacity)
{
    beginStage(params, req);
    resumeStage(t, slot);
    let a = otOpen(t.ot);
    let count = min(t.scratch[slot].segment_count, MAX_RUN_GLYPHS);
    let total = min(t.scratch[slot].glyph_count, MAX_RUN_GLYPHS);
    let cap = min(req.out_cap, MAX_RUN_GLYPHS);

    // The pen, in 26.6 at the request's size, and the slot the next glyph goes
    // to. Both cross segment boundaries; everything else is a segment's own.
    int pen_26_6 = 0;
    uint written = 0u;
    uint base = 0u;
    for (uint s = 0u; s < count; ++s)
    {
        let word = t.scratch[slot].segment[s];
        let len = min(word & SEG_LEN_MASK, total - base);
        for (uint i = 0u; i < len; ++i)
        {
            fromScratchGlyph(i, t.scratch[slot].glyphs[base + i]);
        }
        g_len = len;
        base += len;
        if (len == 0u)
        {
            continue;
        }
        pen_26_6 += finishSegment(a, otFaceAt(a, segmentFace(word)), req, pen_26_6, written,
                                  cap, out_glyphs, glyph_capacity);
    }

    ShapeRun run;
    run.glyph_count = written;
    run.advance = pen_26_6;
    run.flags = g_flags;
    run.depth = g_depth_seen;
    return run;
}
