// OpenType's own binary format, walked on the GPU.
//
// The engine's font reaches the GPU as the bytes the file holds — `cmap`,
// `hmtx`, `GDEF`, `GSUB`, `GPOS` sliced out and concatenated by
// `punchcutter-shape` into one word buffer with a directory in front (`shape/src/font.rs`, `shape/src/abi.rs`
// for the header) — and this module is the reader. Nothing is flattened, nothing
// is prepared, and a lookup type the shaper gains is an edit *here* rather than
// an edit here and a second one in Rust with a cache format between them.
//
// `shape.slang` is the shaper; this file is only the format. The split is the
// obvious one: everything below reads bytes and answers
// questions about a table, and knows nothing about a string, a glyph run or a
// feature.
//
// ---------------------------------------------------------------------------
// BYTES, OFFSETS AND BOUNDS
//
// Everything is big-endian, because OpenType is, and every offset in the spec
// is **relative to the table or subtable that states it**. The walkers here
// carry *absolute* byte offsets into the arena and add: a caller that has a
// subtable at `sub` and reads a coverage offset from it passes
// `sub + be16(a, sub + 2)`.
//
// Every read is bounded by `OtArena.bytes`, which is the header's own
// `arena_bytes` clamped to the buffer the driver actually bound. A read past it
// answers zero. That is what makes a malformed or half-arrived table draw
// wrong and **never** stall: there is no loop below whose trip count depends on
// a byte that could be anything.
//
// ---------------------------------------------------------------------------
// WHAT IS NOT HERE
//
// Device and VariationIndex tables (a `deltaFormat` of 1..3 is a hinting delta
// this renderer has no use for; 0x8000 is a variation index, which waits for
// variable fonts), the item variation store, `avar`/`gvar`/`HVAR`,
// `COLR`/`CPAL` (colour fonts, not yet), `MATH`, `morx` and Graphite, and
// every `cmap` subtable format but 0, 4, 6, 12 and 13 (docs/roadmap.md). Each
// raises `SHAPE_RUN_UNSUPPORTED` at the call site rather than a silent zero.

module ot;

// The arena's header, its face directory and the capacities around them are
// **generated** from `shape/src/abi.rs`: `OtHeader`, `OtFace`, `MAX_OT_FACES`,
// `OT_TABLES_START` and `OT_FACE_ALL_FILLED` are one declaration rendered for
// both languages, not a hand-mirrored copy whose offsets a comment keeps in
// step. Re-exported, so a file importing this one still sees
// them.
__exported import shape_abi;

/// `cmap` subtable records this module will score before it stops looking.
///
/// The one cap in this file, and it is a **time** bound rather than a
/// correctness one: see `cmapSubtable`. Every other loop below is a binary
/// search, whose trip count is the logarithm of a count and is at most
/// thirty-two steps whatever the bytes say.
public static const uint MAX_CMAP_SUBTABLES = 16u;

// ---------------------------------------------------------------------------
// The arena, and the header the CPU wrote
// ---------------------------------------------------------------------------

/// The buffer and the one bound every read is checked against.
///
/// `bytes` is the header's `arena_bytes` clamped to the words the driver bound,
/// so a header that has arrived before its tables (the pipe writes the front
/// block and the tail as separate runs) cannot send a walk off the end.
public struct OtArena
{
    public StructuredBuffer<uint> words;
    public uint bytes;
}

/// Open the arena: read the header, clamp it, and answer a handle.
public OtArena otOpen(StructuredBuffer<uint> words)
{
    OtArena a;
    a.words = words;
    uint count;
    uint stride;
    words.GetDimensions(count, stride);
    let capacity = count * 4u;
    // The header itself is read through a temporary bound of its own size, so
    // the clamp cannot depend on a word past the buffer.
    a.bytes = min(capacity, OT_TABLES_START);
    let stated = be32(a, 8u);
    a.bytes = min(capacity, stated);
    return a;
}

/// The header, as a struct.
public OtHeader otHeader(OtArena a)
{
    OtHeader h;
    h.version = be32(a, 0u);
    h.face_count = min(be32(a, 4u), MAX_OT_FACES);
    h.arena_bytes = a.bytes;
    h._rsvd0 = 0u;
    return h;
}

/// Face `index` of the directory. An index past the count answers a record of
/// zeros, which every walker below reads as "this face has no tables".
public OtFace otFaceAt(OtArena a, uint index)
{
    OtFace f;
    if (index >= MAX_OT_FACES)
    {
        f.glyph_base = 0u; f.glyph_count = 0u; f.upem = 0u; f.num_h_metrics = 0u;
        f.cmap_off = 0u; f.cmap_len = 0u; f.hmtx_off = 0u; f.hmtx_len = 0u;
        f.gdef_off = 0u; f.gdef_len = 0u; f.gsub_off = 0u; f.gsub_len = 0u;
        f.gpos_off = 0u; f.gpos_len = 0u; f.filled_off = 0u; f.flags = 0u;
        return f;
    }
    // The directory is little-endian, because Rust wrote it; only the table
    // bytes behind it are OpenType's.
    let at = (16u + index * 64u) >> 2u;
    f.glyph_base = a.words[at + 0u];
    f.glyph_count = a.words[at + 1u];
    f.upem = a.words[at + 2u];
    f.num_h_metrics = a.words[at + 3u];
    f.cmap_off = a.words[at + 4u];
    f.cmap_len = a.words[at + 5u];
    f.hmtx_off = a.words[at + 6u];
    f.hmtx_len = a.words[at + 7u];
    f.gdef_off = a.words[at + 8u];
    f.gdef_len = a.words[at + 9u];
    f.gsub_off = a.words[at + 10u];
    f.gsub_len = a.words[at + 11u];
    f.gpos_off = a.words[at + 12u];
    f.gpos_len = a.words[at + 13u];
    f.filled_off = a.words[at + 14u];
    f.flags = a.words[at + 15u];
    return f;
}

/// Whether a face's glyph has its curves yet — the bitmap a cover fills in.
/// A face the CPU marked `OT_FACE_ALL_FILLED` answers true without a read.
public bool otFilled(OtArena a, OtFace f, uint local_gid)
{
    if ((f.flags & OT_FACE_ALL_FILLED) != 0u)
    {
        return true;
    }
    if (f.filled_off == 0u || local_gid >= f.glyph_count)
    {
        return false;
    }
    let at = f.filled_off + (local_gid >> 5u) * 4u;
    if (at + 4u > a.bytes)
    {
        return false;
    }
    // The bitmap is Rust's too, so little-endian words rather than be32.
    return (a.words[at >> 2u] & (1u << (local_gid & 31u))) != 0u;
}

// ---------------------------------------------------------------------------
// Big-endian primitives — the only two the walkers use
// ---------------------------------------------------------------------------

/// One byte, or zero past the arena.
///
/// Written **branchless** on purpose. Every `be16` is two of these and every
/// `be32` is four, and Slang inlines the lot — a function taking an `OtArena`
/// takes a descriptor, and SPIR-V has no way to pass one, so there is no
/// version of this module in which these are calls. At three thousand copies a compile, an `if` here would be three thousand extra basic blocks; the clamp and the select are two instructions and no branch.
public uint otByte(OtArena a, uint off)
{
    let safe = min(off, a.bytes - 1u);
    let byte = (a.words[safe >> 2u] >> ((safe & 3u) * 8u)) & 0xFFu;
    return off < a.bytes ? byte : 0u;
}

/// A big-endian `uint16`.
public uint be16(OtArena a, uint off)
{
    return (otByte(a, off) << 8u) | otByte(a, off + 1u);
}

/// A big-endian `int16`, sign-extended.
public int be16s(OtArena a, uint off)
{
    let value = be16(a, off);
    return int(value) - int((value >> 15u) * 65536u);
}

/// A big-endian `uint32`.
public uint be32(OtArena a, uint off)
{
    return (be16(a, off) << 16u) | be16(a, off + 2u);
}

/// A four-character tag as the `uint` a caller compares against
/// `otTag('k','e','r','n')`.
public uint otTag(uint a0, uint a1, uint a2, uint a3)
{
    return (a0 << 24u) | (a1 << 16u) | (a2 << 8u) | a3;
}

// ---------------------------------------------------------------------------
// Coverage and ClassDef (the spec's chapter 2)
// ---------------------------------------------------------------------------

/// The coverage index of `gid` in the Coverage table at `cov`, or −1.
///
/// The index matters as much as the membership: a format 1 SingleSubst adds a
/// delta to the glyph, but a format 2 one and every PairPos, MarkBasePos and
/// LigatureSubst index their own arrays *by the coverage index*.
public int coverageIndex(OtArena a, uint cov, uint gid)
{
    if (cov == 0u)
    {
        return -1;
    }
    let format = be16(a, cov);
    if (format == 1u)
    {
        let count = be16(a, cov + 2u);
        uint lo = 0u;
        uint hi = count;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be16(a, cov + 4u + mid * 2u) < gid)
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo < count && be16(a, cov + 4u + lo * 2u) == gid)
        {
            return int(lo);
        }
        return -1;
    }
    if (format == 2u)
    {
        let count = be16(a, cov + 2u);
        uint lo = 0u;
        uint hi = count;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be16(a, cov + 4u + mid * 6u + 2u) < gid)
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo >= count)
        {
            return -1;
        }
        let record = cov + 4u + lo * 6u;
        let start = be16(a, record);
        if (gid < start)
        {
            return -1;
        }
        return int(be16(a, record + 4u) + (gid - start));
    }
    return -1;
}

/// Whether `gid` is covered at all — the common question, and one that need not
/// pay for the index.
public bool covered(OtArena a, uint cov, uint gid)
{
    return coverageIndex(a, cov, gid) >= 0;
}

/// The **first and last glyph ids** a Coverage table holds, in three reads.
///
/// A coverage table is sorted in both formats, so its extent is its first and
/// last entry, and a glyph outside it is not covered — which is the whole point:
/// a caller can reject a subtable in three reads rather than the twenty-odd a
/// binary search costs. An empty table answers `lo > hi`, which rejects
/// everything.
///
/// This is HarfBuzz's per-lookup digest in the cheapest form that needs no
/// memory: HarfBuzz keeps a Bloom filter of every covered glyph, built once per
/// face and cached; a shader has nowhere to cache one, and the extent gets
/// most of the benefit for three reads and no state.
public void coverageBounds(OtArena a, uint cov, out uint lo, out uint hi)
{
    lo = 1u;
    hi = 0u;
    if (cov == 0u)
    {
        return;
    }
    let format = be16(a, cov);
    let count = be16(a, cov + 2u);
    if (count == 0u)
    {
        return;
    }
    if (format == 1u)
    {
        lo = be16(a, cov + 4u);
        hi = be16(a, cov + 4u + (count - 1u) * 2u);
        return;
    }
    if (format == 2u)
    {
        lo = be16(a, cov + 4u);
        hi = be16(a, cov + 4u + (count - 1u) * 6u + 2u);
        return;
    }
    // A format this module does not walk covers nothing, and the caller's
    // `coverageIndex` says so too.
}

/// The Coverage table that decides whether a subtable can apply **at the
/// cursor** — the one every subtable of every type checks first.
///
/// It is at `sub + 2` for all of them but the format 3 contextuals, whose first
/// input coverage sits behind the backtrack array. `type` is the resolved
/// lookup type (past any Extension) and `gpos` says which table it came from,
/// because 5 and 6 mean different things on the two sides.
public uint primaryCoverage(OtArena a, uint sub, uint type, bool gpos)
{
    if (sub == 0u)
    {
        return 0u;
    }
    let context = gpos ? type == 7u : type == 5u;
    let chain = gpos ? type == 8u : type == 6u;
    if (!context && !chain)
    {
        return sub + be16(a, sub + 2u);
    }
    let format = be16(a, sub);
    if (format == 1u || format == 2u)
    {
        return sub + be16(a, sub + 2u);
    }
    if (format != 3u)
    {
        return 0u;
    }
    if (context)
    {
        // SequenceContextFormat3: glyphCount, lookupCount, coverageOffsets[].
        return sub + be16(a, sub + 6u);
    }
    // ChainedSequenceContextFormat3: the backtrack array comes first.
    let back_count = be16(a, sub + 2u);
    let input_at = 4u + back_count * 2u + 2u;
    return sub + be16(a, sub + input_at);
}

/// The class of `gid` in the ClassDef table at `cd`. **Class 0 is the answer for
/// a glyph the table does not list**, which is the spec's own rule and is why
/// this returns a class rather than a maybe.
public uint classDef(OtArena a, uint cd, uint gid)
{
    if (cd == 0u)
    {
        return 0u;
    }
    let format = be16(a, cd);
    if (format == 1u)
    {
        let start = be16(a, cd + 2u);
        let count = be16(a, cd + 4u);
        if (gid < start || gid >= start + count)
        {
            return 0u;
        }
        return be16(a, cd + 6u + (gid - start) * 2u);
    }
    if (format == 2u)
    {
        let count = be16(a, cd + 2u);
        uint lo = 0u;
        uint hi = count;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be16(a, cd + 4u + mid * 6u + 2u) < gid)
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo >= count)
        {
            return 0u;
        }
        let record = cd + 4u + lo * 6u;
        if (gid < be16(a, record))
        {
            return 0u;
        }
        return be16(a, record + 4u);
    }
    return 0u;
}

// ---------------------------------------------------------------------------
// cmap
// ---------------------------------------------------------------------------

/// Formats this module walks, as the priority a subtable gets. A face's best
/// subtable is the highest-scoring one; a tie goes to the first in the table,
/// which is the order the font states.
///
/// The order is **harfrust's own**, which is not read-fonts' `best_subtable`
/// and not ttf-parser's: the Windows Symbol encoding (3, 0) comes *first*, and
/// only then the full-repertoire (3, 10) and (0, 6)/(0, 4), the BMP (3, 1) and
/// (0, 3), the older Unicode (0, 2)/(0, 1)/(0, 0) and finally Macintosh (1, 0).
/// A face that has a symbol subtable at all means it, and its codepoints are
/// reached through the `0xF000 | c` retry in `otGlyphFor`.
uint cmapScore(uint platform, uint encoding, uint format)
{
    if (format != 0u && format != 4u && format != 6u && format != 12u && format != 13u)
    {
        return 0u;
    }
    if (platform == 3u && encoding == 0u)
    {
        return 10u;
    }
    if (platform == 3u && encoding == 10u)
    {
        return 9u;
    }
    if (platform == 0u && encoding == 6u)
    {
        return 8u;
    }
    if (platform == 0u && encoding == 4u)
    {
        return 7u;
    }
    if (platform == 3u && encoding == 1u)
    {
        return 6u;
    }
    if (platform == 0u && encoding == 3u)
    {
        return 5u;
    }
    if (platform == 0u && encoding == 2u)
    {
        return 4u;
    }
    if (platform == 0u && encoding == 1u)
    {
        return 3u;
    }
    if (platform == 0u && encoding == 0u)
    {
        return 2u;
    }
    if (platform == 1u && encoding == 0u)
    {
        return 1u;
    }
    return 0u;
}

/// The subtable a face's `cmap` should be read through, as an absolute byte
/// offset, plus whether it is the (3, 0) symbol encoding.
///
/// Returns 0 for a face whose `cmap` holds nothing this module walks — which is
/// reported by the caller (`SHAPE_RUN_UNSUPPORTED`), not a silent notdef.
public uint cmapSubtable(OtArena a, OtFace f, out bool symbol)
{
    symbol = false;
    if (f.cmap_off == 0u)
    {
        return 0u;
    }
    // **Capped**. This is the one linear loop in this
    // file, and its trip count is a `u16` out of the arena — so an arena that
    // is not an arena says 65535 here, and `faceFor` runs it sixteen times per
    // codepoint. That is the loop that took an all-0xFF arena to 1.39 s a
    // dispatch on lavapipe and hung RADV's gfx ring. A real font's `cmap` has
    // three or four subtables; sixteen is far past any of them and is a
    // bound no bytes can lift.
    let count = be16(a, f.cmap_off + 2u);
    uint best = 0u;
    uint best_score = 0u;
    // The bound is the **constant**, with the stated count as an early break:
    // this file has no budget to charge, so the only honest bound here is one
    // no byte of the arena can move. The lint in `reload/src/tests.rs` reads
    // the header and would fail on the count.
    for (uint i = 0u; i < MAX_CMAP_SUBTABLES; ++i)
    {
        if (i >= count)
        {
            break;
        }
        let record = f.cmap_off + 4u + i * 8u;
        let platform = be16(a, record);
        let encoding = be16(a, record + 2u);
        let sub = f.cmap_off + be32(a, record + 4u);
        let score = cmapScore(platform, encoding, be16(a, sub));
        if (score > best_score)
        {
            best_score = score;
            best = sub;
            symbol = platform == 3u && encoding == 0u;
        }
    }
    return best;
}

/// One codepoint through one `cmap` subtable, or 0.
public uint cmapLookup(OtArena a, uint sub, uint codepoint)
{
    if (sub == 0u)
    {
        return 0u;
    }
    let format = be16(a, sub);
    if (format == 0u)
    {
        // Byte encoding: 256 bytes of glyph index at offset 6.
        if (codepoint > 0xFFu)
        {
            return 0u;
        }
        return otByte(a, sub + 6u + codepoint);
    }
    if (format == 4u)
    {
        if (codepoint > 0xFFFFu)
        {
            return 0u;
        }
        let seg_x2 = be16(a, sub + 6u);
        let segments = seg_x2 >> 1u;
        if (segments == 0u)
        {
            return 0u;
        }
        let end_codes = sub + 14u;
        let start_codes = end_codes + seg_x2 + 2u;
        let deltas = start_codes + seg_x2;
        let range_offsets = deltas + seg_x2;

        // The first segment whose endCode is at or above the codepoint. The
        // array is sorted and the last entry is 0xFFFF, so this always lands.
        uint lo = 0u;
        uint hi = segments;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be16(a, end_codes + mid * 2u) < codepoint)
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo >= segments)
        {
            return 0u;
        }
        let start = be16(a, start_codes + lo * 2u);
        if (codepoint < start)
        {
            return 0u;
        }
        let range_offset = be16(a, range_offsets + lo * 2u);
        if (range_offset == 0u)
        {
            return (codepoint + be16(a, deltas + lo * 2u)) & 0xFFFFu;
        }
        // The spec's pointer arithmetic, written out: the offset is from the
        // idRangeOffset word itself.
        let at = range_offsets + lo * 2u + range_offset + (codepoint - start) * 2u;
        let gid = be16(a, at);
        if (gid == 0u)
        {
            return 0u;
        }
        return (gid + be16(a, deltas + lo * 2u)) & 0xFFFFu;
    }
    if (format == 6u)
    {
        let first = be16(a, sub + 6u);
        let count = be16(a, sub + 8u);
        if (codepoint < first || codepoint >= first + count)
        {
            return 0u;
        }
        return be16(a, sub + 10u + (codepoint - first) * 2u);
    }
    if (format == 12u || format == 13u)
    {
        // Format 13 is format 12 with the last field read the other way: every
        // codepoint of a group maps to the *same* glyph rather than to a run of
        // them. Adobe Blank is the font that needs it, and it is the font
        // HarfBuzz's 3825 emoji-cluster cases are shaped with.
        let groups = be32(a, sub + 12u);
        uint lo = 0u;
        uint hi = groups;
        while (lo < hi)
        {
            let mid = lo + (hi - lo) / 2u;
            if (be32(a, sub + 16u + mid * 12u + 4u) < codepoint)
            {
                lo = mid + 1u;
            }
            else
            {
                hi = mid;
            }
        }
        if (lo >= groups)
        {
            return 0u;
        }
        let group = sub + 16u + lo * 12u;
        let start = be32(a, group);
        if (codepoint < start)
        {
            return 0u;
        }
        let value = be32(a, group + 8u);
        return format == 12u ? value + (codepoint - start) : value;
    }
    return 0u;
}

/// A codepoint through a whole face: its best subtable, with the (3, 0) symbol
/// retry at U+F000 that every shaper does.
public uint otGlyphFor(OtArena a, OtFace f, uint codepoint)
{
    bool symbol;
    let sub = cmapSubtable(a, f, symbol);
    if (sub == 0u)
    {
        return 0u;
    }
    let gid = cmapLookup(a, sub, codepoint);
    if (gid != 0u || !symbol || codepoint > 0xFFu)
    {
        return gid;
    }
    return cmapLookup(a, sub, 0xF000u | codepoint);
}

// ---------------------------------------------------------------------------
// hmtx
// ---------------------------------------------------------------------------

/// A glyph's advance in the face's design units.
///
/// `hmtx` is `numberOfHMetrics` four-byte records and then a tail of left side
/// bearings: a glyph past the last record carries the last record's advance,
/// which is how a monospace font states one width for ten thousand glyphs.
public uint otAdvance(OtArena a, OtFace f, uint local_gid)
{
    if (f.hmtx_off == 0u || f.num_h_metrics == 0u)
    {
        // harfrust's answer for a face with no `hmtx`: half an em.
        return f.upem / 2u;
    }
    let index = min(local_gid, f.num_h_metrics - 1u);
    return be16(a, f.hmtx_off + index * 4u);
}

// ---------------------------------------------------------------------------
// GDEF
// ---------------------------------------------------------------------------

/// GDEF's glyph classes, as the spec numbers them.
public static const uint GLYPH_CLASS_BASE = 1u;
public static const uint GLYPH_CLASS_LIGATURE = 2u;
public static const uint GLYPH_CLASS_MARK = 3u;
public static const uint GLYPH_CLASS_COMPONENT = 4u;

/// The GlyphClassDef subtable's absolute offset, or 0.
public uint gdefGlyphClasses(OtArena a, OtFace f)
{
    if (f.gdef_off == 0u)
    {
        return 0u;
    }
    let offset = be16(a, f.gdef_off + 4u);
    return offset == 0u ? 0u : f.gdef_off + offset;
}

/// The MarkAttachClassDef subtable's absolute offset, or 0.
public uint gdefMarkAttachClasses(OtArena a, OtFace f)
{
    if (f.gdef_off == 0u)
    {
        return 0u;
    }
    let offset = be16(a, f.gdef_off + 10u);
    return offset == 0u ? 0u : f.gdef_off + offset;
}

/// The coverage table of mark glyph set `index`, or 0 — GDEF 1.2's
/// MarkGlyphSetsDef, which a lookup flag's UseMarkFilteringSet names.
public uint gdefMarkGlyphSet(OtArena a, OtFace f, uint index)
{
    if (f.gdef_off == 0u || be16(a, f.gdef_off + 2u) < 2u)
    {
        return 0u;
    }
    let sets_offset = be16(a, f.gdef_off + 12u);
    if (sets_offset == 0u)
    {
        return 0u;
    }
    let sets = f.gdef_off + sets_offset;
    if (be16(a, sets) != 1u || index >= be16(a, sets + 2u))
    {
        return 0u;
    }
    let coverage = be32(a, sets + 4u + index * 4u);
    return coverage == 0u ? 0u : sets + coverage;
}

// ---------------------------------------------------------------------------
// GSUB and GPOS: the script, feature and lookup lists
// ---------------------------------------------------------------------------

/// The ScriptList's absolute offset for a layout table at `table`.
public uint layoutScripts(OtArena a, uint table)
{
    return table == 0u ? 0u : table + be16(a, table + 4u);
}

/// The FeatureList's.
public uint layoutFeatures(OtArena a, uint table)
{
    return table == 0u ? 0u : table + be16(a, table + 6u);
}

/// The LookupList's.
public uint layoutLookups(OtArena a, uint table)
{
    return table == 0u ? 0u : table + be16(a, table + 8u);
}

/// Lookups in a LookupList.
public uint lookupCount(OtArena a, uint lookups)
{
    return lookups == 0u ? 0u : be16(a, lookups);
}

/// One Lookup table's absolute offset.
public uint lookupAt(OtArena a, uint lookups, uint index)
{
    if (lookups == 0u || index >= be16(a, lookups))
    {
        return 0u;
    }
    return lookups + be16(a, lookups + 2u + index * 2u);
}

/// The Script table for `tag`, or 0. Scripts are sorted by tag, so this is a
/// binary search — a font with two hundred scripts costs eight reads.
public uint scriptFor(OtArena a, uint scripts, uint tag)
{
    if (scripts == 0u)
    {
        return 0u;
    }
    let count = be16(a, scripts);
    uint lo = 0u;
    uint hi = count;
    while (lo < hi)
    {
        let mid = lo + (hi - lo) / 2u;
        if (be32(a, scripts + 2u + mid * 6u) < tag)
        {
            lo = mid + 1u;
        }
        else
        {
            hi = mid;
        }
    }
    if (lo >= count || be32(a, scripts + 2u + lo * 6u) != tag)
    {
        return 0u;
    }
    let offset = be16(a, scripts + 2u + lo * 6u + 4u);
    return offset == 0u ? 0u : scripts + offset;
}

/// A Script table's default LangSys, or 0.
public uint defaultLangSys(OtArena a, uint script)
{
    if (script == 0u)
    {
        return 0u;
    }
    let offset = be16(a, script);
    return offset == 0u ? 0u : script + offset;
}

/// A LangSys's required feature index, or 0xFFFF for none.
public uint langSysRequired(OtArena a, uint langsys)
{
    return langsys == 0u ? 0xFFFFu : be16(a, langsys + 2u);
}

/// Feature indices a LangSys lists.
public uint langSysFeatureCount(OtArena a, uint langsys)
{
    return langsys == 0u ? 0u : be16(a, langsys + 4u);
}

public uint langSysFeature(OtArena a, uint langsys, uint i)
{
    return be16(a, langsys + 6u + i * 2u);
}

/// A feature index's tag, out of the FeatureList.
public uint featureTag(OtArena a, uint features, uint index)
{
    if (features == 0u || index >= be16(a, features))
    {
        return 0u;
    }
    return be32(a, features + 2u + index * 6u);
}

/// A feature index's Feature table, absolute.
public uint featureTable(OtArena a, uint features, uint index)
{
    if (features == 0u || index >= be16(a, features))
    {
        return 0u;
    }
    let offset = be16(a, features + 2u + index * 6u + 4u);
    return offset == 0u ? 0u : features + offset;
}

/// Lookup indices a Feature lists.
public uint featureLookupCount(OtArena a, uint feature)
{
    return feature == 0u ? 0u : be16(a, feature + 2u);
}

public uint featureLookup(OtArena a, uint feature, uint i)
{
    return be16(a, feature + 4u + i * 2u);
}

// ---------------------------------------------------------------------------
// One Lookup
// ---------------------------------------------------------------------------

/// Lookup flag bits, as the spec names them.
public static const uint LOOKUP_RIGHT_TO_LEFT = 0x0001u;
public static const uint LOOKUP_IGNORE_BASE_GLYPHS = 0x0002u;
public static const uint LOOKUP_IGNORE_LIGATURES = 0x0004u;
public static const uint LOOKUP_IGNORE_MARKS = 0x0008u;
public static const uint LOOKUP_USE_MARK_FILTERING_SET = 0x0010u;
public static const uint LOOKUP_MARK_ATTACHMENT_TYPE = 0xFF00u;

public uint lookupType(OtArena a, uint lookup)
{
    return lookup == 0u ? 0u : be16(a, lookup);
}

public uint lookupFlag(OtArena a, uint lookup)
{
    return lookup == 0u ? 0u : be16(a, lookup + 2u);
}

public uint subtableCount(OtArena a, uint lookup)
{
    return lookup == 0u ? 0u : be16(a, lookup + 4u);
}

/// One subtable's absolute offset.
public uint subtableAt(OtArena a, uint lookup, uint index)
{
    if (lookup == 0u || index >= be16(a, lookup + 4u))
    {
        return 0u;
    }
    return lookup + be16(a, lookup + 6u + index * 2u);
}

/// The mark filtering set index, meaningful only when the flag says so. It sits
/// **after** the subtable offsets, which is why it needs the count to find.
public uint markFilteringSet(OtArena a, uint lookup)
{
    if (lookup == 0u || (be16(a, lookup + 2u) & LOOKUP_USE_MARK_FILTERING_SET) == 0u)
    {
        return 0u;
    }
    return be16(a, lookup + 6u + be16(a, lookup + 4u) * 2u);
}

/// Follow an Extension subtable (GSUB type 7, GPOS type 9) to the real one.
///
/// `type` comes back as the extended lookup type, so a caller dispatches on the
/// answer and never on 7 or 9. A format other than 1 is not a thing the spec
/// has, and answers zero.
public uint extensionTarget(OtArena a, uint subtable, out uint type)
{
    type = 0u;
    if (subtable == 0u || be16(a, subtable) != 1u)
    {
        return 0u;
    }
    type = be16(a, subtable + 2u);
    return subtable + be32(a, subtable + 4u);
}

// ---------------------------------------------------------------------------
// ValueRecords and Anchors
// ---------------------------------------------------------------------------

/// ValueFormat bits.
public static const uint VALUE_X_PLACEMENT = 0x0001u;
public static const uint VALUE_Y_PLACEMENT = 0x0002u;
public static const uint VALUE_X_ADVANCE = 0x0004u;
public static const uint VALUE_Y_ADVANCE = 0x0008u;
public static const uint VALUE_X_PLACEMENT_DEVICE = 0x0010u;
public static const uint VALUE_Y_PLACEMENT_DEVICE = 0x0020u;
public static const uint VALUE_X_ADVANCE_DEVICE = 0x0040u;
public static const uint VALUE_Y_ADVANCE_DEVICE = 0x0080u;

/// Bytes one ValueRecord of this format occupies: two per set bit, and the
/// spec's own reason a PairPos row can be walked without reading it.
public uint valueRecordSize(uint format)
{
    return uint(countbits(format & 0xFFu)) * 2u;
}

/// What a ValueRecord says, in design units.
public struct OtValue
{
    public int x_placement;
    public int y_placement;
    public int x_advance;
    public int y_advance;
}

/// Read one ValueRecord at `at` with `format`.
///
/// The four Device offsets take space in the record whether or not anything
/// uses them, and nothing here uses them: a `deltaFormat` of 1..3 is a hinting
/// delta this renderer has no use for, and 0x8000 is a VariationIndex, which
/// waits for variable fonts. **harfrust does the same** — it enters the device block only when the
/// instance has non-default coordinates — so this is agreement rather than a
/// gap, and it raises no flag.
public OtValue readValue(OtArena a, uint at, uint format)
{
    OtValue v;
    v.x_placement = 0;
    v.y_placement = 0;
    v.x_advance = 0;
    v.y_advance = 0;
    uint cursor = at;
    if ((format & VALUE_X_PLACEMENT) != 0u)
    {
        v.x_placement = be16s(a, cursor);
        cursor += 2u;
    }
    if ((format & VALUE_Y_PLACEMENT) != 0u)
    {
        v.y_placement = be16s(a, cursor);
        cursor += 2u;
    }
    if ((format & VALUE_X_ADVANCE) != 0u)
    {
        v.x_advance = be16s(a, cursor);
        cursor += 2u;
    }
    if ((format & VALUE_Y_ADVANCE) != 0u)
    {
        v.y_advance = be16s(a, cursor);
        cursor += 2u;
    }
    return v;
}

/// An anchor point in design units.
public struct OtAnchor
{
    public int x;
    public int y;
    public bool valid;
    /// Format 2 (a contour point): the coordinates below are the anchor's own, and the outline refinement was dropped.
    public bool approximate;
}

/// Read an Anchor table at `at`.
///
/// Format 1 is two coordinates. Format 2 adds a contour point whose *outline*
/// position should override them — the outline is not at hand in a shaping
/// pass, so the coordinates stand and the anchor says it is **approximate**,
/// which is the one anchor case that raises `SHAPE_RUN_UNSUPPORTED`. Format 3 adds Device
/// tables, dropped silently for the reason `readValue` drops them: at a default
/// instance harfrust never reads them either.
public OtAnchor readAnchor(OtArena a, uint at)
{
    OtAnchor anchor;
    anchor.x = 0;
    anchor.y = 0;
    anchor.valid = false;
    anchor.approximate = false;
    if (at == 0u)
    {
        return anchor;
    }
    let format = be16(a, at);
    if (format < 1u || format > 3u)
    {
        return anchor;
    }
    anchor.x = be16s(a, at + 2u);
    anchor.y = be16s(a, at + 4u);
    anchor.valid = true;
    if (format == 2u)
    {
        anchor.approximate = true;
    }
    return anchor;
}
