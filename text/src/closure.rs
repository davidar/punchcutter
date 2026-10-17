//! A face's `GSUB` closure: every glyph a set of glyphs can substitute to.
//!
//! A cover fills a fallback face's glyphs one codepoint at a time, and the
//! shaping arena holds the face's `GSUB` verbatim, so the GPU shaper substitutes
//! with it: two regional indicators ligate to a flag, an Arabic letter takes
//! its initial or final form, a `ccmp` decomposes. None of those outputs is
//! any codepoint's glyph, so none of them was filled, and a reserved record
//! nothing filled draws nothing. The closure is the list a cover has to fill
//! as well: the glyphs reachable from the filled set through the face's own
//! lookups, computed the way HarfBuzz's subsetter computes one
//! (`hb_ot_layout_lookups_substitute_closure`).
//!
//! # The walk
//!
//! The roots are the lookups of the features in [`CLOSURE_FEATURES`] under
//! every script and language system the face declares, plus every required
//! feature. Each root is applied to the whole set, adding its outputs:
//!
//! * Single (1), Multiple (2), Alternate (3): every output of a covered glyph.
//! * Ligature (4): the ligature, when every component is in the set.
//! * Context (5) and Chain Context (6), all three formats each: when every
//!   position of a rule — glyph, class or coverage, backtrack and lookahead
//!   included — has a glyph in the set, the rule's nested lookups are applied
//!   to the whole set. That is an over-approximation HarfBuzz narrows by
//!   position; the price here is a few glyphs filled that a string may never
//!   reach, which is cheaper than being wrong.
//! * Extension (7) is unwrapped by the parser.
//! * Reverse Chain Single (8): the substitutes, when its context intersects.
//!
//! Rounds repeat until one adds nothing. A lookup applied at a given set size
//! is not applied again until the set has grown, which is HarfBuzz's own
//! termination argument (`done_lookups_glyph_count`) and bounds a context
//! lookup that names itself.
//!
//! # Bounds
//!
//! A font is somebody else's file. Every loop here is charged to one step
//! budget, the rounds and the nesting depth are capped, and so is the number
//! of glyphs one closure may add; hitting any of them ends the walk with what
//! it has and says which ([`Closure::bound`]). The caller reports it; nothing
//! here prints.

use std::collections::HashMap;

use anyhow::{Context, Result};
use ttf_parser::GlyphId;
use ttf_parser::gsub::SubstitutionSubtable;
use ttf_parser::opentype_layout::{
    ChainedContextLookup, ClassDefinition, ContextLookup, Coverage, LayoutTable, LookupIndex,
    SequenceLookupRecord,
};

/// The features whose lookups a closure starts from.
///
/// HarfBuzz's default shaper list for horizontal text (the sixteen
/// `slang/modules/shape.slang` enables, plus `rvrn`), and the complex shapers'
/// features — Arabic joining, Hangul jamo, the Indic and USE syllable
/// features — because the face's tables go into the arena whole, and a cover
/// filled for the default shaper should still be whole when a complex shaper
/// enables those features. `frac`/`numr`/`dnom` are in HarfBuzz's default list
/// and here, for the same reason.
///
/// Left out on purpose: the vertical features (`vert`, `vrt2`, `valt`; the
/// shaper lays out horizontal runs only), and every feature a user turns on —
/// `aalt`, `salt`, the `ssNN` and `cvNN` sets, `swsh`, `smcp`, numerals. `aalt`
/// alone reaches every alternate in a face, which for a CJK face is thousands
/// of glyphs a default-features string will never draw.
pub const CLOSURE_FEATURES: [[u8; 4]; 50] = [
    // The default shaper, horizontal, LTR and RTL.
    *b"rvrn", *b"ltra", *b"ltrm", *b"rtla", *b"rtlm", *b"abvm", *b"blwm", *b"ccmp", *b"locl",
    *b"mark", *b"mkmk", *b"rlig", *b"calt", *b"clig", *b"curs", *b"dist", *b"kern", *b"liga",
    *b"rclt", *b"frac", *b"numr", *b"dnom", // Arabic, Syriac, N'Ko and friends.
    *b"isol", *b"init", *b"medi", *b"fina", *b"med2", *b"fin2", *b"fin3", *b"stch", *b"mset",
    // Hangul.
    *b"ljmo", *b"vjmo", *b"tjmo",
    // Indic, Khmer, Myanmar and the Universal Shaping Engine.
    *b"nukt", *b"akhn", *b"rphf", *b"rkrf", *b"pref", *b"blwf", *b"abvf", *b"half", *b"pstf",
    *b"vatu", *b"cjct", *b"pres", *b"abvs", *b"blws", *b"psts", *b"haln",
];

/// How far one closure may go. The caller picks the numbers; [`Limits::COVER`]
/// is what a cover uses.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Fixpoint rounds over the root lookups.
    pub rounds: u32,
    /// Glyphs the closure may add beyond the seeds.
    pub added: usize,
    /// Set probes, rule checks and outputs, all charged to one counter.
    pub steps: u64,
    /// Context lookups nested inside context lookups.
    pub depth: u32,
}

impl Limits {
    /// A cover's budget. Noto Sans Arabic closes a word in a handful of rounds
    /// and well under a million steps; these leave an order of magnitude over
    /// that and still end a hostile table in milliseconds.
    pub const COVER: Limits = Limits {
        rounds: 16,
        added: 1024,
        steps: 8_000_000,
        depth: 16,
    };
}

/// Which bound ended a walk early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Rounds,
    Added,
    Steps,
    Depth,
}

impl Bound {
    pub fn name(self) -> &'static str {
        match self {
            Bound::Rounds => "rounds",
            Bound::Added => "glyphs added",
            Bound::Steps => "steps",
            Bound::Depth => "nesting depth",
        }
    }
}

/// What a closure found.
#[derive(Debug, Default)]
pub struct Closure {
    /// Glyphs reached that were not among the seeds, in the order found.
    pub added: Vec<u16>,
    /// Lookups in the face's `GSUB`, and how many the features rooted.
    pub lookups: u16,
    pub roots: usize,
    /// Rounds run, the last of which added nothing unless a bound ended it.
    pub rounds: u32,
    pub steps: u64,
    /// The bound that ended the walk, or `None` for a fixpoint.
    pub bound: Option<Bound>,
}

/// The glyphs `seeds` reach through `data`'s `GSUB`, within `limits`.
///
/// A face with no `GSUB` (or one this parser refuses) closes to its seeds:
/// `added` is empty and nothing is an error but a face that will not parse.
/// Seeds past the face's glyph count are ignored, and so is any output past it.
pub fn gsub_closure(data: &[u8], index: u32, seeds: &[u16], limits: Limits) -> Result<Closure> {
    let face = ttf_parser::Face::parse(data, index & 0xffff).context("the face would not parse")?;
    let glyphs = face.number_of_glyphs();
    let Some(gsub) = face.tables().gsub else {
        return Ok(Closure::default());
    };
    let roots = roots(&gsub);
    let mut walk = Walk {
        table: gsub,
        member: vec![false; usize::from(glyphs)],
        list: Vec::new(),
        seeds: None,
        done: HashMap::new(),
        steps: 0,
        limits,
        bound: None,
    };
    for gid in seeds {
        walk.add(*gid);
    }
    walk.seeds = Some(walk.list.len());

    let mut rounds = 0;
    loop {
        if rounds >= limits.rounds {
            walk.bound.get_or_insert(Bound::Rounds);
            break;
        }
        rounds += 1;
        let before = walk.list.len();
        for lookup in &roots {
            walk.apply(*lookup, 0);
            if walk.bound.is_some() {
                break;
            }
        }
        if walk.bound.is_some() || walk.list.len() == before {
            break;
        }
    }
    Ok(Closure {
        added: walk.list[walk.seeds.unwrap_or(0)..].to_vec(),
        lookups: gsub.lookups.len(),
        roots: roots.len(),
        rounds,
        steps: walk.steps,
        bound: walk.bound,
    })
}

/// The lookups [`CLOSURE_FEATURES`] and the required features name, under
/// every script and language system, sorted and without repeats.
fn roots(table: &LayoutTable<'_>) -> Vec<LookupIndex> {
    let mut features: Vec<u16> = Vec::new();
    for script in table.scripts {
        let languages = script.default_language.into_iter().chain(script.languages);
        for language in languages {
            features.extend(language.required_feature.map(|index| index | 0x8000));
            features.extend(language.feature_indices);
        }
    }
    let mut lookups: Vec<LookupIndex> = Vec::new();
    for word in features {
        let required = word & 0x8000 != 0;
        let Some(feature) = table.features.get(word & 0x7fff) else {
            continue;
        };
        if required || CLOSURE_FEATURES.contains(&feature.tag.to_bytes()) {
            lookups.extend(feature.lookup_indices);
        }
    }
    lookups.sort_unstable();
    lookups.dedup();
    lookups
}

/// The walk's state: the set as a membership vector and an insertion-ordered
/// list, and the budget.
struct Walk<'a> {
    table: LayoutTable<'a>,
    member: Vec<bool>,
    list: Vec<u16>,
    /// How many of `list` are seeds, once seeding is over; the added cap
    /// counts from there.
    seeds: Option<usize>,
    /// Lookup → the set size it was last applied at.
    done: HashMap<LookupIndex, usize>,
    steps: u64,
    limits: Limits,
    bound: Option<Bound>,
}

impl Walk<'_> {
    /// One step of work; `false` once the budget is spent.
    fn charge(&mut self, steps: u64) -> bool {
        self.steps += steps;
        if self.steps > self.limits.steps {
            self.bound.get_or_insert(Bound::Steps);
            return false;
        }
        true
    }

    fn has(&self, gid: u16) -> bool {
        self.member.get(usize::from(gid)).copied().unwrap_or(false)
    }

    fn add(&mut self, gid: u16) {
        let Some(slot) = self.member.get_mut(usize::from(gid)) else {
            return;
        };
        if *slot {
            return;
        }
        if let Some(seeds) = self.seeds
            && self.list.len() - seeds >= self.limits.added
        {
            self.bound.get_or_insert(Bound::Added);
            return;
        }
        *slot = true;
        self.list.push(gid);
    }

    /// Every glyph in the set, at the time of the call.
    fn snapshot(&self) -> Vec<u16> {
        self.list.clone()
    }

    fn apply(&mut self, lookup: LookupIndex, depth: u32) {
        if self.bound.is_some() {
            return;
        }
        if depth > self.limits.depth {
            self.bound.get_or_insert(Bound::Depth);
            return;
        }
        if self.done.get(&lookup) == Some(&self.list.len()) {
            return;
        }
        self.done.insert(lookup, self.list.len());
        let Some(table) = self.table.lookups.get(lookup) else {
            return;
        };
        for sub in 0..table.subtables.len() {
            if !self.charge(1) {
                return;
            }
            let Some(subtable) = table.subtables.get::<SubstitutionSubtable>(sub) else {
                continue;
            };
            self.subtable(subtable, depth);
            if self.bound.is_some() {
                return;
            }
        }
    }

    fn subtable(&mut self, subtable: SubstitutionSubtable<'_>, depth: u32) {
        use ttf_parser::gsub::SingleSubstitution;
        let set = self.snapshot();
        if !self.charge(set.len() as u64) {
            return;
        }
        match subtable {
            SubstitutionSubtable::Single(SingleSubstitution::Format1 { coverage, delta }) => {
                for gid in set {
                    if coverage.contains(GlyphId(gid)) {
                        self.add(gid.wrapping_add(delta as u16));
                    }
                }
            }
            SubstitutionSubtable::Single(SingleSubstitution::Format2 {
                coverage,
                substitutes,
            }) => {
                for gid in set {
                    if let Some(out) = coverage
                        .get(GlyphId(gid))
                        .and_then(|at| substitutes.get(at))
                    {
                        self.add(out.0);
                    }
                }
            }
            SubstitutionSubtable::Multiple(table) => {
                for gid in set {
                    let Some(seq) = table
                        .coverage
                        .get(GlyphId(gid))
                        .and_then(|at| table.sequences.get(at))
                    else {
                        continue;
                    };
                    if !self.charge(u64::from(seq.substitutes.len())) {
                        return;
                    }
                    for out in seq.substitutes {
                        self.add(out.0);
                    }
                }
            }
            SubstitutionSubtable::Alternate(table) => {
                for gid in set {
                    let Some(alts) = table
                        .coverage
                        .get(GlyphId(gid))
                        .and_then(|at| table.alternate_sets.get(at))
                    else {
                        continue;
                    };
                    if !self.charge(u64::from(alts.alternates.len())) {
                        return;
                    }
                    for out in alts.alternates {
                        self.add(out.0);
                    }
                }
            }
            SubstitutionSubtable::Ligature(table) => {
                for gid in set {
                    let Some(ligatures) = table
                        .coverage
                        .get(GlyphId(gid))
                        .and_then(|at| table.ligature_sets.get(at))
                    else {
                        continue;
                    };
                    if !self.charge(u64::from(ligatures.len())) {
                        return;
                    }
                    for ligature in ligatures {
                        if !self.charge(u64::from(ligature.components.len())) {
                            return;
                        }
                        if ligature.components.into_iter().all(|c| self.has(c.0)) {
                            self.add(ligature.glyph.0);
                        }
                    }
                }
            }
            SubstitutionSubtable::Context(table) => self.context(table, &set, depth),
            SubstitutionSubtable::ChainContext(table) => self.chain(table, &set, depth),
            SubstitutionSubtable::ReverseChainSingle(table) => {
                let context = table
                    .backtrack_coverages
                    .into_iter()
                    .chain(table.lookahead_coverages)
                    .collect::<Vec<_>>();
                if !context
                    .iter()
                    .all(|coverage| self.coverage_meets(*coverage, &set))
                {
                    return;
                }
                for gid in set {
                    if let Some(out) = table
                        .coverage
                        .get(GlyphId(gid))
                        .and_then(|at| table.substitutes.get(at))
                    {
                        self.add(out.0);
                    }
                }
            }
        }
    }

    /// Context (5): the rules whose every input position meets the set.
    fn context(&mut self, table: ContextLookup<'_>, set: &[u16], depth: u32) {
        match table {
            ContextLookup::Format1 { coverage, sets } => {
                for gid in set {
                    let Some(rules) = coverage.get(GlyphId(*gid)).and_then(|at| sets.get(at))
                    else {
                        continue;
                    };
                    for rule in rules {
                        if !self.charge(u64::from(rule.input.len()) + 1) {
                            return;
                        }
                        if rule.input.into_iter().all(|g| self.has(g)) {
                            self.nested(rule.lookups, depth);
                        }
                    }
                }
            }
            ContextLookup::Format2 {
                coverage,
                classes,
                sets,
            } => {
                for class in self.first_classes(coverage, classes, set) {
                    let Some(rules) = sets.get(class) else {
                        continue;
                    };
                    for rule in rules {
                        let input: Vec<u16> = rule.input.into_iter().collect();
                        if input.iter().all(|c| self.class_meets(classes, *c, set)) {
                            self.nested(rule.lookups, depth);
                        }
                        if self.bound.is_some() {
                            return;
                        }
                    }
                }
            }
            ContextLookup::Format3 {
                coverage,
                coverages,
                lookups,
            } => {
                if self.coverage_meets(coverage, set)
                    && coverages.into_iter().all(|c| self.coverage_meets(c, set))
                {
                    self.nested(lookups, depth);
                }
            }
        }
    }

    /// Chain Context (6): as [`Self::context`], with the backtrack and the
    /// lookahead held to the same test.
    fn chain(&mut self, table: ChainedContextLookup<'_>, set: &[u16], depth: u32) {
        match table {
            ChainedContextLookup::Format1 { coverage, sets } => {
                for gid in set {
                    let Some(rules) = coverage.get(GlyphId(*gid)).and_then(|at| sets.get(at))
                    else {
                        continue;
                    };
                    for rule in rules {
                        let len = rule.backtrack.len() + rule.input.len() + rule.lookahead.len();
                        if !self.charge(u64::from(len) + 1) {
                            return;
                        }
                        let context = rule
                            .backtrack
                            .into_iter()
                            .chain(rule.input)
                            .chain(rule.lookahead);
                        if context.into_iter().all(|g| self.has(g)) {
                            self.nested(rule.lookups, depth);
                        }
                    }
                }
            }
            ChainedContextLookup::Format2 {
                coverage,
                backtrack_classes,
                input_classes,
                lookahead_classes,
                sets,
            } => {
                for class in self.first_classes(coverage, input_classes, set) {
                    let Some(rules) = sets.get(class) else {
                        continue;
                    };
                    for rule in rules {
                        let meets = rule
                            .backtrack
                            .into_iter()
                            .all(|c| self.class_meets(backtrack_classes, c, set))
                            && rule
                                .input
                                .into_iter()
                                .all(|c| self.class_meets(input_classes, c, set))
                            && rule
                                .lookahead
                                .into_iter()
                                .all(|c| self.class_meets(lookahead_classes, c, set));
                        if meets {
                            self.nested(rule.lookups, depth);
                        }
                        if self.bound.is_some() {
                            return;
                        }
                    }
                }
            }
            ChainedContextLookup::Format3 {
                coverage,
                backtrack_coverages,
                input_coverages,
                lookahead_coverages,
                lookups,
            } => {
                let meets = self.coverage_meets(coverage, set)
                    && backtrack_coverages
                        .into_iter()
                        .chain(input_coverages)
                        .chain(lookahead_coverages)
                        .all(|c| self.coverage_meets(c, set));
                if meets {
                    self.nested(lookups, depth);
                }
            }
        }
    }

    /// A rule matched: its lookups, applied to the whole set one level down.
    fn nested(&mut self, records: ttf_parser::LazyArray16<'_, SequenceLookupRecord>, depth: u32) {
        for record in records {
            self.apply(record.lookup_list_index, depth + 1);
            if self.bound.is_some() {
                return;
            }
        }
    }

    /// The input classes of the covered glyphs in the set: which rule sets of a
    /// class-based context can start at all.
    fn first_classes(
        &mut self,
        coverage: Coverage<'_>,
        classes: ClassDefinition<'_>,
        set: &[u16],
    ) -> Vec<u16> {
        let mut out: Vec<u16> = set
            .iter()
            .filter(|gid| coverage.contains(GlyphId(**gid)))
            .map(|gid| classes.get(GlyphId(*gid)))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether some glyph in the set is in `class`.
    fn class_meets(&mut self, classes: ClassDefinition<'_>, class: u16, set: &[u16]) -> bool {
        if !self.charge(set.len() as u64) {
            return false;
        }
        set.iter().any(|gid| classes.get(GlyphId(*gid)) == class)
    }

    /// Whether some glyph in the set is covered.
    fn coverage_meets(&mut self, coverage: Coverage<'_>, set: &[u16]) -> bool {
        if !self.charge(set.len() as u64) {
            return false;
        }
        set.iter().any(|gid| coverage.contains(GlyphId(*gid)))
    }
}

#[cfg(test)]
mod tests;
