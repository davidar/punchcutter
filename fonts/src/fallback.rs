//! The fallback font, as a lazily merged virtual font.
//!
//! A codepoint the primary face lacks is `.notdef` — a box on screen, which is
//! what a CJK or emoji window title looks like without this module. A consumer
//! hands [`crate::Merged::cover`] the bytes of a run its shaper flagged
//! `.notdef`; this module is what happens next: the codepoints the merged
//! `cmap` cannot answer for, a chain of faces to ask, and one glyph filled per
//! codepoint that anything can draw.
//!
//! # One glyph space, and a face's whole range in it
//!
//! One merged glyph id per covered codepoint would kern with nothing and
//! substitute to nothing — right for a stray symbol, wrong for a word —
//! because the face's own `GSUB` and `GPOS` index *its* glyph ids and nothing
//! would relate them to the merged space.
//!
//! So the space relates them. A face that draws anything is
//! **opened**: it takes the whole of its glyph range in the merged space at
//! `glyph_base`, its five tables go into the arena verbatim, and a merged id is
//! `glyph_base + face gid` — one addition, and the face's layout works. The
//! records inside the range are blanks until a cover fills one, and the arena's
//! per-face bitmap says which are filled, so the shaper can flag a glyph whose
//! curves have not arrived and the consumer covers again.
//!
//! The cost is the record table: Droid Sans Fallback is ~50k glyphs, so opening
//! it reserves ~50k blank records, 64 bytes each on the GPU (about 3.2 MB) plus
//! the CPU index, once per process and only when CJK text first appears.
//!
//! Curves are normalised by **each face's own** upem, which
//! [`punchcutter_text::font::codepoint_curves`] already does. The advance is not
//! rescaled here: `hmtx` is in the arena and the shader walks the face's
//! own, in the face's own design units, and scales the segment by the face's
//! own upem.
//!
//! # The closure
//!
//! Filling a face per codepoint alone would leave every **substitution output** blank: the
//! flag two regional indicators ligate to in Noto Emoji, the joining forms of
//! Noto Sans Arabic, any `ccmp` or `liga` output. The shaper reaches them from
//! the face's own `GSUB` and finds a reserved record nothing filled, and the
//! tofu becomes a gap. So after a cover has filled its codepoints, every face
//! that grew is closed: [`punchcutter_text::closure::gsub_closure`] walks the face's
//! lookups from everything this process has filled in it, and each glyph it
//! reaches is filled by id ([`punchcutter_text::font::glyph_curves`]) through the same
//! per-face cache. The walk is bounded ([`punchcutter_text::closure::Limits::COVER`])
//! and a bound it hits is one log line.
//!
//! # The chain
//!
//! `fc-match -s sans-serif` once at startup — the list, not a call per
//! codepoint — with the primary itself dropped out of it. Faces are opened as
//! they are first needed and probed in order; the first that has the codepoint
//! **and ink for it** wins. A face that names the codepoint and draws it with a
//! colour table (COLRv1, CBDT) has no outline, so the walk goes past it: that
//! is how Noto Color Emoji is skipped and Noto Emoji's monochrome outlines are
//! taken. Colour emoji is not drawn yet, and the log says so.
//!
//! A codepoint no face can draw is remembered, so it is asked once per process.
//!
//! # The cache
//!
//! Per face, beside the primary's curve cache under `$XDG_CACHE_HOME/punchcutter`
//! and keyed by the face's path and mtime: the glyphs appended from it, as the
//! records [`punchcutter_text::Builder::push_from`] rebases. What it saves is the band
//! search — about 48 µs a glyph, and nearly all of a cold font load — so a
//! process restarted with the same CJK text on screen pays a file read
//! instead.

use std::io;
use std::path::{Path, PathBuf};

use punchcutter_shape::Arena;
use punchcutter_text::{Builder, CurveSet};

use crate::cache_dir;

/// The first eight bytes of a fallback cache file. The primary's cache is
/// `punchcutter_text`'s own `PNCHCRVS`; this one carries a glyph-id list in front of
/// it, so it is a different file with a different magic.
pub const FALLBACK_MAGIC: [u8; 8] = *b"PNCHFBGL";

/// This file's own layout version, beside `punchcutter_text::CACHE_VERSION` (which
/// covers the records themselves and rides in the file name).
pub const FALLBACK_VERSION: u32 = 1;

/// Faces the chain will hold. `fc-match -s` answers with everything installed —
/// 67 on a typical desktop install — and a cover that has walked past sixty of
/// them for one codepoint has already lost; the bound is here so the list cannot become
/// unbounded work per cover on a machine with a font collection.
pub const MAX_CHAIN: usize = 96;

/// What closing one face added, after a cover.
pub struct Closed {
    /// The family, for a log line.
    pub family: String,
    /// Glyphs the closure reached that were not filled yet, and were filled.
    pub glyphs: u32,
    /// Bytes the four curve buffers grew by.
    pub bytes: usize,
    /// Whether every one of those glyphs came out of the cache file.
    pub warm: bool,
    /// The walk's own numbers: lookups in the face, lookups the features
    /// rooted, rounds, steps, and the bound it hit if it hit one.
    pub lookups: u16,
    pub roots: usize,
    pub rounds: u32,
    pub steps: u64,
    pub bound: Option<punchcutter_text::closure::Bound>,
    /// Wall time of the walk and the fill together.
    pub micros: u128,
}

/// What one face contributed for one codepoint.
pub struct Covered {
    /// The family, for a log line.
    pub family: String,
    /// The merged glyph id the caller filled: `glyph_base + face gid`.
    pub gid: u32,
    /// Bytes the four curve buffers grew by.
    pub bytes: usize,
    /// Whether the records came out of the cache file rather than the band
    /// search.
    pub warm: bool,
}

/// One face of the chain: what fontconfig said, and what has been read.
struct Face {
    family: String,
    path: PathBuf,
    /// The face index within a collection, as fontconfig reports it.
    index: u32,
    /// The file, once something has needed it. Dropped again after a cover in
    /// which this face contributed nothing — a chain walk for a CJK codepoint
    /// touches every Latin face before it, and holding all of them resident
    /// would be tens of megabytes for nothing.
    data: Option<Vec<u8>>,
    /// A file that would not parse: logged once, then skipped for good.
    broken: bool,
    /// This face's index in the arena's directory, once it has been opened.
    /// `None` until the face draws its first codepoint — a face that is walked
    /// past and contributes nothing costs the arena nothing.
    opened: Option<usize>,
    /// Whether anything has ever been appended from this face.
    contributed: bool,
    /// Every glyph id **in this face** filled so far, codepoints and closure
    /// alike: the seeds of the next closure.
    filled: Vec<u16>,
    /// Whether a codepoint was filled since the face was last closed.
    grown: bool,
    /// The glyphs appended from it, and where they came from.
    cache: FaceCache,
}

/// The glyphs one face has contributed, as records — the cache file's contents
/// in memory, whether or not the file existed.
#[derive(Default)]
struct FaceCache {
    /// The cache file, or `None` before the face has been opened (the name
    /// carries the file's mtime, which is not known until then).
    file: Option<PathBuf>,
    /// Glyph ids **in that face**, parallel to `set.glyphs`.
    gids: Vec<u16>,
    set: CurveSet,
    /// Whether `set` has something the file does not.
    dirty: bool,
}

/// The chain, its faces' caches, and what has already been answered.
pub struct Chain {
    faces: Vec<Face>,
    /// Codepoints no face in the chain can draw: asked once per process.
    hopeless: Vec<u32>,
}

impl Chain {
    /// `fc-match -s sans-serif`, with `primary` dropped out of it.
    ///
    /// One subprocess at startup. The order is fontconfig's own, which is the
    /// order the desktop's other toolkits fall back in, so a title that draws
    /// in GTK draws here with the same faces.
    pub fn discover(primary: &Path) -> Chain {
        let mut faces = Vec::new();
        for (family, path, index) in fc_sorted() {
            if path == primary || faces.len() >= MAX_CHAIN {
                continue;
            }
            if faces
                .iter()
                .any(|face: &Face| face.path == path && face.index == index)
            {
                continue;
            }
            faces.push(Face {
                family,
                path,
                index,
                data: None,
                broken: false,
                opened: None,
                contributed: false,
                filled: Vec::new(),
                grown: false,
                cache: FaceCache::default(),
            });
        }
        tracing::info!(
            "the fallback chain is {} faces: {}",
            faces.len(),
            faces
                .iter()
                .take(6)
                .map(|face| face.family.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Chain {
            faces,
            hopeless: Vec::new(),
        }
    }

    /// A chain of exactly these faces — what the tests use, and what a machine
    /// with no fontconfig would get if anything ever wanted one.
    pub fn of(faces: impl IntoIterator<Item = (String, PathBuf, u32)>) -> Chain {
        Chain {
            faces: faces
                .into_iter()
                .map(|(family, path, index)| Face {
                    family,
                    path,
                    index,
                    data: None,
                    broken: false,
                    opened: None,
                    contributed: false,
                    filled: Vec::new(),
                    grown: false,
                    cache: FaceCache::default(),
                })
                .collect(),
            hopeless: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.faces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// The families, in order — for a log line and for the tests.
    pub fn families(&self) -> impl Iterator<Item = &str> {
        self.faces.iter().map(|face| face.family.as_str())
    }

    /// Fill the first face's drawing of `ch` into `curves`, opening that face
    /// into `arena` if this is the first thing it has drawn. `None` when
    /// nothing in the chain can draw it.
    ///
    /// The caller owns the merged buffers and the arena and passes them in,
    /// because the glyph id this returns is only meaningful against both.
    pub fn cover(&mut self, curves: &mut CurveSet, arena: &mut Arena, ch: char) -> Option<Covered> {
        let codepoint = ch as u32;
        if self.hopeless.contains(&codepoint) {
            return None;
        }
        for index in 0..self.faces.len() {
            match self.faces[index].cover(curves, arena, ch) {
                Some(covered) => return Some(covered),
                None => continue,
            }
        }
        tracing::info!("no face has U+{codepoint:04X}");
        self.hopeless.push(codepoint);
        None
    }

    /// After a cover's codepoints: fill the `GSUB` closure of every face that
    /// grew, so the glyphs its substitutions reach are drawn when the shaper
    /// reaches them (see *The closure* above). One entry per face closed; a face the
    /// closure added nothing to is still reported, with `glyphs` zero, so the
    /// log can say the walk ran.
    ///
    /// Must run before [`Self::settle`], which lets go of the files.
    pub fn close(&mut self, curves: &mut CurveSet, arena: &mut Arena) -> Vec<Closed> {
        let mut out = Vec::new();
        for face in &mut self.faces {
            if let Some(closed) = face.close(curves, arena) {
                out.push(closed);
            }
        }
        out
    }

    /// After a cover: write what was appended, and let go of the files that
    /// contributed nothing. Errors are log lines — a cache is a courtesy.
    pub fn settle(&mut self) {
        for face in &mut self.faces {
            face.cache.write();
            if !face.contributed {
                face.data = None;
            }
        }
    }

    /// Codepoints the whole chain refused, for a log line and the tests.
    pub fn hopeless(&self) -> &[u32] {
        &self.hopeless
    }
}

impl Face {
    /// This face's answer for one codepoint, filled into `curves`.
    fn cover(&mut self, curves: &mut CurveSet, arena: &mut Arena, ch: char) -> Option<Covered> {
        if self.broken {
            return None;
        }
        if self.data.is_none() {
            let data = match std::fs::read(&self.path) {
                Ok(data) => data,
                Err(err) => {
                    tracing::warn!("{} would not open: {err}", self.path.display());
                    self.broken = true;
                    return None;
                }
            };
            self.cache.open(&self.path, data.len());
            self.data = Some(data);
        }
        let data = self.data.as_ref().expect("just read");

        let found = match punchcutter_text::font::codepoint_curves(data, self.index, ch) {
            Ok(found) => found?,
            Err(err) => {
                tracing::warn!(
                    "{} is not a face this build reads: {err:#}",
                    self.path.display()
                );
                self.broken = true;
                self.data = None;
                return None;
            }
        };
        if !found.has_ink() {
            // The face names the codepoint and draws it with something that is
            // not an outline — a COLRv1 layer tree or an embedded bitmap. Colour
            // emoji is **not drawn yet**: Noto Color Emoji stays a `.notdef`,
            // and the walk goes on to whatever monochrome face is behind it.
            tracing::info!(
                "U+{:04X} in {} has no outline (a colour or bitmap face); walking on",
                ch as u32,
                self.family
            );
            return None;
        }

        // Open the face the first time it draws anything: its whole glyph
        // range is reserved in the merged space, and its five tables go into
        // the arena. The curve records are brought up to the same watermark, so
        // that `glyph_base + face gid` is the record's own index.
        if self.opened.is_none() {
            arena.set_glyph_count(curves.glyphs.len() as u32);
            let data = self.data.as_ref().expect("just read");
            let face = match arena.add_face(data, self.index & 0xffff, false) {
                Ok(face) => face,
                Err(err) => {
                    tracing::warn!("{} will not go in the arena: {err}", self.family);
                    self.broken = true;
                    self.data = None;
                    return None;
                }
            };
            let info = arena.faces()[face];
            let mut builder = Builder::resume(std::mem::take(curves));
            builder.reserve(info.glyph_base + info.glyph_count - builder.len());
            *curves = builder.finish();
            tracing::info!(
                "opened {} into the arena: {} glyphs from {}, {} — {} KiB",
                self.family,
                info.glyph_count,
                info.glyph_base,
                info.table_sizes(),
                info.bytes().div_ceil(1024),
            );
            self.opened = Some(face);
        }
        let before = bytes_of(curves);
        let local = found.gid;
        let (gid, warm) = self.fill(curves, arena, local, || Some(found))?;
        self.grown = true;
        Some(Covered {
            family: self.family.clone(),
            gid,
            bytes: bytes_of(curves).saturating_sub(before),
            warm,
        })
    }

    /// Fill this face's glyph `local` into its reserved record, from the cache
    /// or from `make`. The face must be open. Returns the merged id and whether
    /// the records were cached.
    fn fill(
        &mut self,
        curves: &mut CurveSet,
        arena: &mut Arena,
        local: u16,
        make: impl FnOnce() -> Option<punchcutter_text::font::FaceGlyph>,
    ) -> Option<(u32, bool)> {
        let face = self.opened?;
        let gid = arena.faces()[face].glyph_base + u32::from(local);
        let (slot, warm) = self.cache.slot_for(local, make)?;
        let mut builder = Builder::resume(std::mem::take(curves));
        builder.fill_from(gid, &self.cache.set, slot);
        *curves = builder.finish();
        arena.mark_filled(face, u32::from(local));
        arena.set_glyph_count(curves.glyphs.len() as u32);
        if !self.filled.contains(&local) {
            self.filled.push(local);
        }
        self.contributed = true;
        Some((gid, warm))
    }

    /// The closure of everything filled in this face, filled — or `None` for a
    /// face nothing new was filled in since the last time.
    fn close(&mut self, curves: &mut CurveSet, arena: &mut Arena) -> Option<Closed> {
        if !self.grown {
            return None;
        }
        self.grown = false;
        let face = self.opened?;
        let started = std::time::Instant::now();
        let data = self.data.take()?;
        let closure = punchcutter_text::closure::gsub_closure(
            &data,
            self.index,
            &self.filled,
            punchcutter_text::closure::Limits::COVER,
        );
        let closure = match closure {
            Ok(closure) => closure,
            Err(err) => {
                tracing::warn!("{}: no GSUB closure: {err:#}", self.family);
                self.data = Some(data);
                return None;
            }
        };
        let before = bytes_of(curves);
        let mut glyphs = 0u32;
        let mut warm = true;
        for local in &closure.added {
            if arena.is_filled(face, u32::from(*local)) {
                continue;
            }
            let index = self.index;
            let make = || match punchcutter_text::font::glyph_curves(&data, index, *local) {
                Ok(found) => found,
                Err(err) => {
                    tracing::warn!("glyph {local}: {err:#}");
                    None
                }
            };
            if let Some((_, cached)) = self.fill(curves, arena, *local, make) {
                glyphs += 1;
                warm &= cached;
            }
        }
        self.data = Some(data);
        if let Some(bound) = closure.bound {
            tracing::warn!(
                "{}: the GSUB closure stopped at its bound on {} ({} glyphs added, {} rounds, {} steps); \
                 a substitution past it may draw nothing",
                self.family,
                bound.name(),
                closure.added.len(),
                closure.rounds,
                closure.steps
            );
        }
        Some(Closed {
            family: self.family.clone(),
            glyphs,
            bytes: bytes_of(curves).saturating_sub(before),
            warm,
            lookups: closure.lookups,
            roots: closure.roots,
            rounds: closure.rounds,
            steps: closure.steps,
            bound: closure.bound,
            micros: started.elapsed().as_micros(),
        })
    }
}

impl FaceCache {
    /// Name the cache file and read it, once the face's own file is open.
    ///
    /// Keyed by path and mtime: a font replaced in place
    /// misses, and so does a layout change, because `punchcutter_text::CACHE_VERSION`
    /// is in the name too.
    fn open(&mut self, path: &Path, len: usize) {
        if self.file.is_some() {
            // Opened before, and `write` has already put whatever was appended
            // into the file: re-reading it would only replace these records
            // with themselves.
            return;
        }
        let stamp = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs())
            .unwrap_or(0);
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in path.as_os_str().as_encoded_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let file = cache_dir().join(format!(
            "fallback-{hash:016x}-{len}-{stamp}-{}-{FALLBACK_VERSION}.curves",
            punchcutter_text::CACHE_VERSION
        ));
        if let Ok(bytes) = std::fs::read(&file)
            && let Some((gids, set)) = decode(&bytes)
        {
            self.gids = gids;
            self.set = set;
        }
        self.file = Some(file);
    }

    /// The slot holding this face's glyph `gid`, building it from `make` if
    /// the cache does not have it — so a cached glyph is never outlined. The
    /// bool is whether it was already there; `None` when it was not and `make`
    /// had nothing.
    fn slot_for(
        &mut self,
        gid: u16,
        make: impl FnOnce() -> Option<punchcutter_text::font::FaceGlyph>,
    ) -> Option<(u32, bool)> {
        if let Some(slot) = self.gids.iter().position(|known| *known == gid) {
            return Some((slot as u32, true));
        }
        let found = make()?;
        let mut builder = Builder::resume(std::mem::take(&mut self.set));
        let slot = builder.push(&found.contours, found.advance, false);
        self.set = builder.finish();
        self.gids.push(gid);
        self.dirty = true;
        Some((slot, false))
    }

    /// Write the file through a temporary in the same directory, so a killed
    /// process leaves no half-written cache. Failure is one log line.
    fn write(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(file) = self.file.clone() else {
            return;
        };
        let bytes = encode(&self.gids, &self.set);
        let write = || -> io::Result<()> {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let temp = file.with_extension("curves.new");
            std::fs::write(&temp, &bytes)?;
            std::fs::rename(&temp, &file)
        };
        if let Err(err) = write() {
            tracing::warn!("could not cache {}: {err}", file.display());
        }
    }
}

/// The four curve buffers' size in bytes — what an append is measured by.
fn bytes_of(set: &CurveSet) -> usize {
    set.bytes()
}

/// A cache file: the magic, this file's version, the glyph-id list, then the
/// records as `punchcutter_text::Cached` writes them.
pub fn encode(gids: &[u16], set: &CurveSet) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&FALLBACK_MAGIC);
    out.extend_from_slice(&FALLBACK_VERSION.to_le_bytes());
    out.extend_from_slice(&(gids.len() as u32).to_le_bytes());
    for gid in gids {
        out.extend_from_slice(&u32::from(*gid).to_le_bytes());
    }
    let cached = punchcutter_text::Cached {
        curves: CurveSet {
            points: set.points.clone(),
            bands: set.bands.clone(),
            band_curves: set.band_curves.clone(),
            glyphs: set.glyphs.clone(),
            index: set.index.clone(),
            rewritten: Vec::new(),
        },
        // Neither scalar means anything here — the curves are em-normalised
        // already and a fallback face's cap height is nobody's business — so
        // they are written as what they are: the count, and zero.
        units_per_em: 0.0,
        glyph_count: gids.len() as u32,
        cap_height: 0.0,
    };
    out.extend_from_slice(&cached.to_bytes());
    out
}

/// The inverse, or `None` for every way a file can fail to be one of ours.
pub fn decode(bytes: &[u8]) -> Option<(Vec<u16>, CurveSet)> {
    let head = 16usize;
    if bytes.len() < head || bytes[..8] != FALLBACK_MAGIC {
        return None;
    }
    let word = |at: usize| -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    if word(8) != FALLBACK_VERSION {
        return None;
    }
    let count = word(12) as usize;
    let list_end = head.checked_add(count.checked_mul(4)?)?;
    if bytes.len() < list_end {
        return None;
    }
    let gids: Vec<u16> = (0..count).map(|i| word(head + i * 4) as u16).collect();
    let cached = punchcutter_text::Cached::from_bytes(&bytes[list_end..])?;
    if cached.curves.glyphs.len() != count || cached.curves.index.len() != count {
        return None;
    }
    Some((gids, cached.curves))
}

/// `fc-match -s sans-serif`: every face fontconfig would fall back through, in
/// its order.
///
/// I/O and a subprocess, which is what the CPU side is for.
/// An empty answer is not an error: a machine with no fontconfig has no chain
/// and every missing codepoint stays a box.
fn fc_sorted() -> Vec<(String, PathBuf, u32)> {
    let output = std::process::Command::new("fc-match")
        .args([
            "-s",
            "--format=%{family}\t%{file}\t%{index}\n",
            "sans-serif",
        ])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) => return Vec::new(),
        Err(err) => {
            tracing::warn!("fc-match -s: {err}; there is no fallback chain");
            return Vec::new();
        }
    };
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut faces = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(family), Some(file)) = (fields.next(), fields.next()) else {
            continue;
        };
        let index: u32 = fields
            .next()
            .and_then(|word| word.trim().parse().ok())
            .unwrap_or(0);
        let file = file.trim();
        if file.is_empty() {
            continue;
        }
        // A family list comes back comma-separated ("Noto Sans,Noto Sans Bold").
        let family = family.split(',').next().unwrap_or(family).trim();
        faces.push((family.to_owned(), PathBuf::from(file), index));
    }
    faces
}

#[cfg(test)]
mod tests;
