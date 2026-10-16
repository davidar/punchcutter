//! punchcutter-fonts — a font, found and loaded into the engine's buffers.
//!
//! Everything the GPU needs from a font, built on the CPU: the font's Bézier
//! curves and band tables (`punchcutter-text`, cached on disk), and the
//! **arena** of verbatim OpenType tables (`punchcutter-shape`). The two share
//! one glyph-id space, the *merged* space, which starts as the primary face's
//! own ids and grows as fallback faces are opened into it.
//!
//! The API is two calls:
//!
//! - **"Give me a font"**: [`Loaded::load`] asks fontconfig for a UI font
//!   (or takes `$PUNCHCUTTER_FONT`), and returns its curve buffers and its arena.
//!   [`Loaded::load_path`] does the same for a file already chosen.
//! - **"Here is a string; cover what the primary lacks"**: [`Merged::cover`]
//!   sends every codepoint the primary cannot draw down the [`fallback`] chain,
//!   and the **first face that draws one is opened into the arena**: it takes
//!   its whole glyph range in the merged space, so its own `GSUB` and `GPOS`
//!   index the merged ids by one addition and a Hebrew word gets its fallback
//!   face's own kerning. The records in that range are blanks until a glyph is
//!   covered, and a per-face bitmap in the arena says which are filled, so the
//!   GPU shaper can flag a glyph whose curves have not arrived.
//!
//! When to call [`Merged::cover`] is the consumer's decision; the usual trigger
//! is a run the GPU shaper returned with `SHAPE_RUN_NOTDEF`. After a cover the
//! consumer brings its GPU copies up to date: each curve buffer grew by a tail,
//! the records [`punchcutter_text::CurveSet::take_rewritten`] names were
//! rewritten in place, and the arena's front block, fill bitmaps
//! ([`punchcutter_shape::Arena::bitmap_range`]) and tail changed.
//!
//! # The cache
//!
//! One, for the curves only. The OpenType tables are not cached: slicing five
//! tables out of a directory costs 60 µs and there is nothing worth keeping.
//! The curve cache lives under `$XDG_CACHE_HOME/punchcutter/`, keyed on the
//! FNV-1a of the font file plus its length and the layout version, so a new
//! font and a changed layout both miss. State, not config: a missing or
//! unreadable cache is a rebuild and one log line.
//!
//! # Reporting
//!
//! Nothing here prints. What a load or a cover did, and what it could not do,
//! goes to [`tracing`] at `info` or `warn`; a consumer that installs a
//! subscriber sees it.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use punchcutter_core::plain::bytes_of_slice;
use punchcutter_shape::{Arena, ShapeFont};
use punchcutter_text::CurveSet;

use crate::fallback::{Chain, Closed};

pub mod fallback;

/// The one knob: a font file, or a fontconfig pattern, to use instead of
/// [`pick_family`]'s search. Read by [`Loaded::load`] and nowhere else.
pub const FONT_ENV: &str = "PUNCHCUTTER_FONT";

/// UI families to try, best first, asked of fontconfig. Adwaita Sans and
/// Cantarell are GNOME's; Noto and DejaVu are the safety net, and `sans-serif`
/// below them is whatever the machine has.
pub const UI_FAMILIES: &[&str] = &[
    "Adwaita Sans",
    "Cantarell",
    "Inter",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
];

/// Characters a user interface commonly composes into runs of its own: the
/// down and up arrows of a transfer rate, the degree sign, the en dash that
/// stands for no value, and the ellipsis a cut label ends in.
///
/// A string a consumer shapes whole can be covered by a fallback face when the
/// primary lacks a glyph; a run a shader composes on the GPU, out of glyphs it
/// looked up itself, cannot, so the primary face has to carry these. Noto Sans
/// has no arrows, and a UI shaped with it would show two boxes in a network
/// line. [`Loaded::load`] asks for these; [`Loaded::load_requiring`] takes a
/// consumer's own list.
pub const COMPOSED_CHARS: &[char] = &['\u{2193}', '\u{2191}', '\u{00B0}', '\u{2013}', '\u{2026}'];

// ---------------------------------------------------------------------------
// The font
// ---------------------------------------------------------------------------

/// A font: its four curve buffers, its arena, and the facts a consumer sizes
/// text by.
pub struct Loaded {
    /// The family fontconfig gave us, for a log line or a label.
    pub family: String,
    /// The file it came from.
    pub path: PathBuf,
    /// The curve buffers: the font's glyphs, then whatever is merged after
    /// them.
    pub curves: CurveSet,
    /// `head.unitsPerEm`.
    pub units_per_em: u32,
    /// Glyphs in the primary face. Anything at or above this was merged later.
    pub font_glyphs: u32,
    /// `OS/2.sCapHeight` in em units: what a consumer snaps its pixel sizes
    /// by, since Slug does no hinting.
    pub cap_height: f32,
    /// The arena: the primary's five tables verbatim, plus every fallback face
    /// a cover has opened.
    pub arena: Arena,
    /// The primary's file, kept so a cover can ask whether it already draws a
    /// codepoint. That is a face probe — the same question
    /// [`punchcutter_text::font::codepoint_curves`] asks of every face in the
    /// chain — and not a table this side interprets.
    pub data: Vec<u8>,
    /// Whether the curve buffers came out of the cache file.
    pub warm: bool,
    /// How long the load took, ms — cold or warm, whichever this was.
    pub load_ms: u128,
}

impl Loaded {
    /// Find a font and load it: `$PUNCHCUTTER_FONT` if it names one, else the
    /// first of [`UI_FAMILIES`] fontconfig actually has with every one of
    /// [`COMPOSED_CHARS`].
    pub fn load() -> Result<Loaded, String> {
        Loaded::load_requiring(COMPOSED_CHARS)
    }

    /// [`Self::load`], with the consumer's own list of characters the primary
    /// face must carry.
    pub fn load_requiring(required: &[char]) -> Result<Loaded, String> {
        let (family, path) = match std::env::var_os(FONT_ENV) {
            Some(value) if !value.is_empty() => {
                let asked = PathBuf::from(&value);
                if asked.is_file() {
                    let family = asked
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| asked.display().to_string());
                    tracing::info!("${FONT_ENV} names {}", asked.display());
                    (family, asked)
                } else {
                    let pattern = value.to_string_lossy().into_owned();
                    let found = fc_match(&pattern).ok_or_else(|| {
                        format!("${FONT_ENV}={pattern} is neither a file nor a font fontconfig has")
                    })?;
                    tracing::info!("${FONT_ENV}={pattern} matched {}", found.0);
                    found
                }
            }
            _ => pick_family(required).ok_or_else(|| {
                "fontconfig named no usable sans-serif font; there is nothing to shape with"
                    .to_owned()
            })?,
        };
        Loaded::load_path(&family, &path)
    }

    /// [`Self::load`] for a font already chosen.
    pub fn load_path(family: &str, path: &Path) -> Result<Loaded, String> {
        let started = std::time::Instant::now();
        let data = std::fs::read(path)
            .map_err(|err| format!("could not read the font at {}: {err}", path.display()))?;
        let dir = cache_dir();
        let mut arena = Arena::of(&data, 0)
            .map_err(|err| format!("{} has no usable tables: {err}", path.display()))?;

        let key = cache_key(&dir, &data);
        let (cached, warm) = match read_cache(&key) {
            Some(cached) => (cached, true),
            None => {
                let font = punchcutter_text::Font::parse(&data, 0).map_err(|err| {
                    format!("could not build curves for {}: {err:#}", path.display())
                })?;
                let cached = punchcutter_text::Cached {
                    curves: font.curves,
                    units_per_em: font.units_per_em,
                    glyph_count: font.glyph_count,
                    cap_height: font.cap_height,
                };
                write_cache(&key, &cached);
                (cached, false)
            }
        };

        // The merged glyph space starts as the curve records: the primary's
        // glyphs, and after them whatever faces arrive. The arena's watermark
        // follows it, so a face opened later takes a base past them.
        arena.set_glyph_count(cached.curves.glyphs.len() as u32);
        Ok(Loaded {
            family: family.to_owned(),
            path: path.to_path_buf(),
            units_per_em: cached.units_per_em as u32,
            font_glyphs: cached.glyph_count,
            cap_height: cached.cap_height,
            curves: cached.curves,
            arena,
            data,
            warm,
            load_ms: started.elapsed().as_millis(),
        })
    }

    /// `"warm"` or `"cold"`: whether the curve buffers came out of the cache.
    pub fn start(&self) -> &'static str {
        if self.warm { "warm" } else { "cold" }
    }

    /// The four curve buffers as the bytes the GPU holds, in the binding order
    /// `slang/text.slang` declares: points, bands, band curves, glyphs.
    pub fn curve_bytes(&self) -> [&[u8]; 4] {
        let set = &self.curves;
        [
            bytes_of_slice(&set.points),
            bytes_of_slice(&set.bands),
            bytes_of_slice(&set.band_curves),
            bytes_of_slice(&set.glyphs),
        ]
    }

    /// The arena as the bytes the GPU holds: `ShapeTables::ot` in
    /// `slang/shape.slang`.
    pub fn arena_bytes(&self) -> &[u8] {
        self.arena.bytes()
    }

    /// The merged font's scalars, the `font` field of `ShapeParams`.
    pub fn shape_font(&self) -> ShapeFont {
        self.arena.font()
    }

    /// The four curve buffers' live lengths in bytes, in binding order — what
    /// a merge is a delta against.
    pub fn curve_lengths(&self) -> [usize; 4] {
        self.curves.buffer_bytes()
    }

    /// Whether the **primary** face draws a codepoint — the question a cover
    /// asks before it walks the chain.
    pub fn primary_draws(&self, ch: char) -> bool {
        matches!(
            punchcutter_text::font::codepoint_curves(&self.data, 0, ch),
            Ok(Some(found)) if found.gid != 0
        )
    }
}

// ---------------------------------------------------------------------------
// The merged font: the primary, and the fallback faces that cover what it lacks
// ---------------------------------------------------------------------------

/// A [`Loaded`] font and the fallback chain behind it.
pub struct Merged {
    pub loaded: Loaded,
    /// The fallback chain and its per-face caches (see [`fallback`]).
    fallback: Chain,
    /// Codepoints already decided about: in the primary, newly filled, or
    /// nothing in the chain can draw them. A repeated string costs one pass
    /// over its characters and no font is opened twice.
    seen: HashSet<u32>,
}

/// What one face contributed to one [`Merged::cover`].
#[derive(Clone, Debug, PartialEq)]
pub struct CoveredFace {
    pub family: String,
    /// Codepoints this face drew.
    pub codepoints: u32,
    /// Bytes the four curve buffers grew by for them.
    pub bytes: usize,
    /// Whether every one of those glyphs came out of the face's cache file.
    pub warm: bool,
}

/// What one [`Merged::cover`] did.
#[derive(Default)]
pub struct Cover {
    /// The faces that drew codepoints, in the order first used.
    pub faces: Vec<CoveredFace>,
    /// The `GSUB` closure of every face that grew: the ligatures, joining forms
    /// and `ccmp` outputs its substitutions reach, filled in the same cover.
    pub closed: Vec<Closed>,
    /// Glyph records filled, codepoints and closures together. Zero means the
    /// GPU copies need nothing.
    pub glyphs: u32,
}

impl Merged {
    /// `loaded`, with the chain fontconfig gives (`fc-match -s sans-serif`,
    /// the primary dropped out of it).
    pub fn new(loaded: Loaded) -> Merged {
        let chain = Chain::discover(&loaded.path);
        Merged::with_chain(loaded, chain)
    }

    /// [`Self::new`] with the chain handed in — what the tests use, so a test
    /// names the faces it means instead of whatever the machine has installed.
    pub fn with_chain(loaded: Loaded, fallback: Chain) -> Merged {
        Merged {
            loaded,
            fallback,
            seen: HashSet::new(),
        }
    }

    /// The chain, for its families and its refusals.
    pub fn chain(&self) -> &Chain {
        &self.fallback
    }

    /// Cover `text`: every codepoint the merged font cannot draw goes down the
    /// chain. The first face that draws one is **opened into the arena** —
    /// taking its whole glyph range in the merged space — and the glyph is
    /// filled into the blank record its face id names. Then every face that
    /// grew is closed under its own `GSUB` (see [`fallback`], *The closure*),
    /// so the glyphs its substitutions reach are filled before the shaper
    /// reaches them.
    ///
    /// A string with nothing missing changes nothing and comes back with
    /// `glyphs` zero.
    pub fn cover(&mut self, text: &str) -> Cover {
        let mut cover = Cover::default();
        for ch in text.chars() {
            let codepoint = ch as u32;
            if !self.seen.insert(codepoint) {
                continue;
            }
            // A codepoint the primary already draws needs nothing, and neither
            // does one an earlier cover already filled.
            if self.loaded.primary_draws(ch) {
                continue;
            }
            let Some(covered) =
                self.fallback
                    .cover(&mut self.loaded.curves, &mut self.loaded.arena, ch)
            else {
                continue;
            };
            cover.glyphs += 1;
            match cover
                .faces
                .iter_mut()
                .find(|face| face.family == covered.family)
            {
                Some(face) => {
                    face.codepoints += 1;
                    face.bytes += covered.bytes;
                    face.warm &= covered.warm;
                }
                None => cover.faces.push(CoveredFace {
                    family: covered.family,
                    codepoints: 1,
                    bytes: covered.bytes,
                    warm: covered.warm,
                }),
            }
        }
        for closed in self
            .fallback
            .close(&mut self.loaded.curves, &mut self.loaded.arena)
        {
            tracing::info!(
                "cover {text:?}: closure of {} +{} glyphs, {} KiB{} ({}/{} lookups rooted, {} rounds, {} steps, {:.1} ms{})",
                closed.family,
                closed.glyphs,
                closed.bytes.div_ceil(1024),
                if closed.warm && closed.glyphs > 0 {
                    ", cached"
                } else {
                    ""
                },
                closed.roots,
                closed.lookups,
                closed.rounds,
                closed.steps,
                closed.micros as f64 / 1000.0,
                match closed.bound {
                    Some(bound) => format!(", stopped at the {} bound", bound.name()),
                    None => String::new(),
                },
            );
            cover.glyphs += closed.glyphs;
            cover.closed.push(closed);
        }
        self.fallback.settle();

        for face in &cover.faces {
            tracing::info!(
                "cover {text:?}: {} codepoints from {} ({} KiB{})",
                face.codepoints,
                face.family,
                face.bytes.div_ceil(1024),
                if face.warm { ", cached" } else { "" }
            );
        }
        cover
    }
}

// ---------------------------------------------------------------------------
// fontconfig, and the curve cache
// ---------------------------------------------------------------------------

/// A fontconfig pattern for `family` that also asks for `required`:
/// `fc-match` then answers a *different* family when this one lacks them.
pub fn pattern_requiring(family: &str, required: &[char]) -> String {
    if required.is_empty() {
        return family.to_owned();
    }
    let charset: Vec<String> = required
        .iter()
        .map(|c| format!("{:x}", *c as u32))
        .collect();
    format!("{family}:charset={}", charset.join(" "))
}

/// The first of [`UI_FAMILIES`] fontconfig actually has that carries every
/// one of `required`, else whatever it calls `sans-serif` with them.
///
/// `fc-match` always answers *something*, so a requested family counts as
/// installed only when the family it names back contains what was asked for;
/// an installed family that fails the charset comes back as another family,
/// and one log line says which was passed over and why.
pub fn pick_family(required: &[char]) -> Option<(String, PathBuf)> {
    for candidate in UI_FAMILIES {
        let wanted = candidate.to_ascii_lowercase();
        let Some((installed, _)) = fc_match(candidate) else {
            continue;
        };
        if !installed.to_ascii_lowercase().contains(&wanted) {
            continue;
        }
        if let Some((family, path)) = fc_match(&pattern_requiring(candidate, required))
            && family.to_ascii_lowercase().contains(&wanted)
        {
            return Some((family, path));
        }
        tracing::info!(
            "{installed} is installed but lacks a required glyph ({}); passed over",
            required.iter().collect::<String>()
        );
    }
    let (family, path) = fc_match(&pattern_requiring("sans-serif", required))?;
    tracing::info!(
        "none of {UI_FAMILIES:?} is installed with every required glyph; falling back to {family} ({})",
        path.display()
    );
    Some((family, path))
}

/// One `fc-match`: the family fontconfig chose and the file it lives in.
pub fn fc_match(pattern: &str) -> Option<(String, PathBuf)> {
    let output = std::process::Command::new("fc-match")
        .args(["--format=%{family}\t%{file}", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let answer = String::from_utf8(output.stdout).ok()?;
    let (family, file) = answer.split_once('\t')?;
    let file = file.trim();
    if file.is_empty() {
        return None;
    }
    // A family list comes back comma-separated ("Noto Sans,Noto Sans Regular").
    let family = family.split(',').next().unwrap_or(family).trim();
    Some((family.to_owned(), PathBuf::from(file)))
}

/// `$XDG_CACHE_HOME/punchcutter`, or `~/.cache/punchcutter`.
pub fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("punchcutter")
}

/// The cache file name for a font's bytes: a content hash and the layout
/// version, so a font replaced in place and a layout change both miss.
///
/// FNV-1a over the whole file. Not a cryptographic digest and not meant to be:
/// the file it names is derived data this crate wrote, and the only question
/// asked of the name is whether these are the same bytes as last time.
pub fn cache_key(dir: &Path, font: &[u8]) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in font {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    dir.join(format!(
        "{hash:016x}-{}-{}.curves",
        font.len(),
        punchcutter_text::CACHE_VERSION
    ))
}

/// The cached buffers, or `None` for every way that can fail.
fn read_cache(path: &Path) -> Option<punchcutter_text::Cached> {
    let bytes = std::fs::read(path).ok()?;
    let cached = punchcutter_text::Cached::from_bytes(&bytes);
    if cached.is_none() {
        tracing::info!(
            "{} is not a cache this build reads; rebuilding",
            path.display()
        );
    }
    cached
}

/// Write the cache through a temporary file in the same directory, so a killed
/// process cannot leave a half-written one behind. Failure is a log line.
fn write_cache(path: &Path, cached: &punchcutter_text::Cached) {
    let write = || -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("curves.new");
        std::fs::write(&temp, cached.to_bytes())?;
        std::fs::rename(&temp, path)
    };
    match write() {
        Ok(()) => tracing::info!("cached the curves at {}", path.display()),
        Err(err) => tracing::warn!("could not cache the curves at {}: {err}", path.display()),
    }
}

#[cfg(test)]
mod tests;
