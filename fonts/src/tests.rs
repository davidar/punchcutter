//! What a load and a cover promise a consumer: whole buffers to upload, a
//! merged glyph space that fallback faces join by their whole range, and
//! updates a GPU copy can follow by tails and named records alone.
//!
//! Every test is skipped rather than failed on a machine with no font: a build
//! container without fontconfig is not a regression.

use super::*;

use crate::fallback::Chain;

/// The font this machine's fontconfig picks, or `None`.
fn loaded() -> Option<Loaded> {
    let (family, path) = pick_family(COMPOSED_CHARS)?;
    match Loaded::load_path(&family, &path) {
        Ok(loaded) => Some(loaded),
        Err(why) => {
            eprintln!("skipping: {why}");
            None
        }
    }
}

#[test]
fn every_buffer_a_consumer_uploads_has_bytes() {
    let Some(loaded) = loaded() else { return };
    for (i, bytes) in loaded.curve_bytes().iter().enumerate() {
        // Every one of the four is non-empty for a real font.
        assert!(!bytes.is_empty(), "curve buffer {i} is empty");
        assert_eq!(
            bytes.len() % 4,
            0,
            "curve buffer {i} is not a whole number of words"
        );
    }
    assert_eq!(
        loaded.curve_bytes().map(<[u8]>::len),
        loaded.curve_lengths()
    );
    // The arena always carries at least a `cmap` and an `hmtx` behind its
    // directory.
    let arena = loaded.arena_bytes();
    assert!(arena.len() > punchcutter_shape::OT_TABLES_START as usize);
    assert_eq!(arena.len() % 4, 0);
    let font = loaded.shape_font();
    assert_eq!(font.upem, loaded.units_per_em);
    assert_eq!(
        font.glyph_count,
        loaded.curves.glyphs.len() as u32,
        "the shaper's glyph bound is the record table's length"
    );
    assert!(loaded.cap_height > 0.0 && loaded.cap_height < 1.0);
}

#[test]
fn the_cache_lives_under_punchcutter() {
    let dir = cache_dir();
    assert!(
        dir.ends_with("punchcutter"),
        "{} is not the cache directory",
        dir.display()
    );
    let key = cache_key(&dir, b"a font, notionally");
    assert!(key.starts_with(&dir));
    assert!(
        key.to_string_lossy()
            .ends_with(&format!("-18-{}.curves", punchcutter_text::CACHE_VERSION)),
        "the key carries the length and the layout version: {}",
        key.display()
    );
    // The same bytes name the same file, and different bytes do not.
    assert_eq!(key, cache_key(&dir, b"a font, notionally"));
    assert_ne!(key, cache_key(&dir, b"a font, notionallY"));
}

/// The cover path end to end: Latin the primary already has, one CJK
/// codepoint and one emoji it does not. The covered codepoints land in a
/// fallback face's range, past the primary's ids, which is what "one merged
/// glyph space" means.
#[test]
fn a_cover_opens_a_face_and_fills_what_the_primary_lacks() {
    let Some(loaded) = loaded() else { return };
    let Some(chain) = test_chain() else {
        eprintln!("skipping: no CJK or emoji face on this machine");
        return;
    };
    let font_glyphs = loaded.font_glyphs;
    let mut merged = Merged::with_chain(loaded, chain);
    let before = merged.loaded.curve_lengths();

    let cover = merged.cover("Hi 日 \u{1f600}");
    assert!(
        cover.glyphs >= 2,
        "two codepoints were missing and {} glyphs were filled",
        cover.glyphs
    );
    assert!(!cover.faces.is_empty());
    let after = merged.loaded.curve_lengths();
    for i in 0..4 {
        assert!(after[i] >= before[i], "curve buffer {i} shrank");
    }

    let arena = &merged.loaded.arena;
    assert!(
        arena.faces().len() >= 2,
        "a cover opens at least one fallback face"
    );
    for face in 1..arena.faces().len() {
        let info = arena.faces()[face];
        assert!(
            info.glyph_base >= font_glyphs,
            "face {face} overlaps the primary's ids"
        );
        assert!(
            !info.all_filled,
            "a face opened by a cover fills glyph by glyph"
        );
        assert!(
            merged.loaded.curves.glyphs.len() as u32 >= info.glyph_base + info.glyph_count,
            "face {face}'s reserved range has no records"
        );
    }
    assert_eq!(
        merged.loaded.shape_font().glyph_count,
        merged.loaded.curves.glyphs.len() as u32,
        "the shaper's bound follows the merged record table"
    );
    // Every codepoint that was covered has a filled record inside some face's
    // reserved range.
    let filled: usize = (1..arena.faces().len())
        .map(|face| {
            let info = arena.faces()[face];
            (0..info.glyph_count)
                .filter(|gid| arena.is_filled(face, *gid))
                .count()
        })
        .sum();
    assert!(filled >= 1, "nothing was filled");
}

/// A string the primary draws entirely changes nothing.
#[test]
fn a_cover_with_nothing_missing_changes_nothing() {
    let Some(loaded) = loaded() else { return };
    let mut merged = Merged::with_chain(loaded, Chain::of([]));
    let before = merged.loaded.curve_lengths();
    let arena_before = merged.loaded.arena_bytes().len();
    let cover = merged.cover("Terminal - bash");
    assert_eq!(cover.glyphs, 0);
    assert!(cover.faces.is_empty() && cover.closed.is_empty());
    assert_eq!(merged.loaded.curve_lengths(), before);
    assert_eq!(merged.loaded.arena_bytes().len(), arena_before);
}

/// A chain of exactly the two faces the cover test needs, or `None` on a
/// machine that has neither.
fn test_chain() -> Option<Chain> {
    let mut faces = Vec::new();
    for (pattern, ch) in [(":lang=ja", '日'), ("emoji", '\u{1f600}')] {
        let output = std::process::Command::new("fc-match")
            .args(["--format=%{family}\t%{file}\t%{index}", pattern])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let answer = String::from_utf8(output.stdout).ok()?;
        let mut fields = answer.split('\t');
        let family = fields.next()?.split(',').next()?.trim().to_owned();
        let path = PathBuf::from(fields.next()?.trim());
        let index: u32 = fields
            .next()
            .and_then(|word| word.trim().parse().ok())
            .unwrap_or(0);
        let data = std::fs::read(&path).ok()?;
        // A face that claims the codepoint and draws it with a colour table is
        // no use to this test — and is exactly what the chain walks past.
        let drawn = punchcutter_text::font::codepoint_curves(&data, index, ch)
            .ok()
            .flatten()
            .is_some_and(|glyph| glyph.has_ink());
        if !drawn {
            return None;
        }
        faces.push((family, path, index));
    }
    Some(Chain::of(faces))
}

// ---------------------------------------------------------------------------
// A cover fills the GSUB closure
// ---------------------------------------------------------------------------

/// The face fontconfig lists under exactly this family, as a one-face chain
/// entry. `fc-list`, because `fc-match "Noto Emoji"` answers Noto Color Emoji.
pub(crate) fn family_face(family: &str) -> Option<(String, PathBuf, u32)> {
    let output = std::process::Command::new("fc-list")
        .args([family, "--format=%{family}\t%{file}\t%{index}\n"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    for line in lines {
        let mut fields = line.split('\t');
        if !fields.next()?.split(',').any(|name| name.trim() == family) {
            continue;
        }
        let path = PathBuf::from(fields.next()?.trim());
        let index = fields
            .next()
            .and_then(|word| word.trim().parse().ok())
            .unwrap_or(0);
        if path.is_file() {
            return Some((family.to_owned(), path, index));
        }
    }
    None
}

/// Filled records in arena face `face`, as face gids.
fn filled_in(merged: &Merged, face: usize) -> Vec<u32> {
    let arena = &merged.loaded.arena;
    (0..arena.faces()[face].glyph_count)
        .filter(|gid| arena.is_filled(face, *gid))
        .collect()
}

/// `🇦🇺 Australia` asks Noto Emoji for two regional indicators; the GPU shaper
/// ligates them to `flagAustralia` (face gid 1626 in the 1,895-glyph build of
/// the face), and that record must be filled and carry bands — filled per
/// codepoint only, it is a reserved blank and the flag draws as a gap.
#[test]
fn a_flag_cover_fills_the_ligature_the_shaper_reaches() {
    let Some(loaded) = loaded() else { return };
    let Some(face) = family_face("Noto Emoji") else {
        eprintln!("skipping: Noto Emoji is not installed");
        return;
    };
    let mut merged = Merged::with_chain(loaded, Chain::of([face]));
    let ot_before = merged.loaded.arena_bytes().len();
    let curves_before: usize = merged.loaded.curve_lengths().iter().sum();

    let started = std::time::Instant::now();
    let cover = merged.cover("🇦🇺 Australia");
    let took = started.elapsed();

    let arena = &merged.loaded.arena;
    let emoji = arena.faces().len() - 1;
    assert!(emoji >= 1, "Noto Emoji was not opened");
    let info = arena.faces()[emoji];
    let filled = filled_in(&merged, emoji);
    eprintln!(
        "🇦🇺 Australia: {} glyphs filled in Noto Emoji ({:?}) of {} reserved, curves +{} KiB, arena +{} KiB, {:.1} ms",
        filled.len(),
        filled,
        info.glyph_count,
        (merged.loaded.curve_lengths().iter().sum::<usize>() - curves_before).div_ceil(1024),
        (arena.bytes().len() - ot_before).div_ceil(1024),
        took.as_secs_f64() * 1000.0,
    );
    assert!(
        filled.len() > 2,
        "only the two regional indicators were filled: {filled:?}"
    );
    assert_eq!(cover.closed.len(), 1, "the face that grew was closed");
    if info.glyph_count == 1895 {
        assert!(
            filled.contains(&1626),
            "flagAustralia (1626) is not filled: {filled:?}"
        );
        let record = merged.loaded.curves.glyphs[(info.glyph_base + 1626) as usize];
        assert!(
            record.hband_count > 0 && record.vband_count > 0,
            "the flag's record has no bands"
        );
    }
}

/// An Arabic word fills its letters and everything their substitutions reach —
/// the joining forms an Arabic shaper selects, filled because the face's `GSUB`
/// is in the arena and the closure walks it.
#[test]
fn an_arabic_cover_fills_the_forms_its_letters_substitute_to() {
    let Some(loaded) = loaded() else { return };
    let Some(face) = family_face("Noto Sans Arabic") else {
        eprintln!("skipping: Noto Sans Arabic is not installed");
        return;
    };
    let data = std::fs::read(&face.1).expect("the file fontconfig named");
    let index = face.2;
    let mut merged = Merged::with_chain(loaded, Chain::of([face]));
    let curves_before: usize = merged.loaded.curve_lengths().iter().sum();

    let word = "مرحبا بالعالم";
    let started = std::time::Instant::now();
    merged.cover(word);
    let took = started.elapsed();

    let letters: Vec<u16> = word
        .chars()
        .filter(|ch| !merged.loaded.primary_draws(*ch))
        .filter_map(|ch| {
            punchcutter_text::font::codepoint_curves(&data, index, ch)
                .ok()
                .flatten()
        })
        .map(|glyph| glyph.gid)
        .collect();
    if letters.is_empty() {
        // DejaVu Sans, for one, carries Arabic itself: nothing reaches the
        // fallback face, and there is no cover to check.
        eprintln!(
            "skipping: {} draws {word:?} itself; the cover test needs a primary without Arabic",
            merged.loaded.family
        );
        return;
    }
    let arabic = merged.loaded.arena.faces().len() - 1;
    let filled = filled_in(&merged, arabic);
    let closure = punchcutter_text::closure::gsub_closure(
        &data,
        index,
        &letters,
        punchcutter_text::closure::Limits::COVER,
    )
    .expect("a face");
    eprintln!(
        "{word:?}: {} codepoint glyphs, {} filled after the closure, curves +{} KiB, {:.1} ms",
        {
            let mut distinct = letters.clone();
            distinct.sort_unstable();
            distinct.dedup();
            distinct.len()
        },
        filled.len(),
        (merged.loaded.curve_lengths().iter().sum::<usize>() - curves_before).div_ceil(1024),
        took.as_secs_f64() * 1000.0,
    );
    assert!(
        !closure.added.is_empty(),
        "Noto Sans Arabic's letters substitute to nothing?"
    );
    for gid in closure.added.iter().chain(&letters) {
        assert!(
            filled.contains(&u32::from(*gid)),
            "face gid {gid} is reachable and not filled"
        );
    }
}

// ---------------------------------------------------------------------------
// A GPU copy follows a cover by tails and named records
// ---------------------------------------------------------------------------

/// The first flag cover opens Noto Emoji — reserving its whole range of blank
/// records — and fills them inside the new tail. A second flag fills records
/// **below** what a GPU copy already holds, which no tail carries: unless the
/// copy rewrites the records `take_rewritten` names, the arena's bitmap says
/// filled, the copy's record is the blank, and 🇯🇵 draws as nothing while 🇦🇺
/// draws.
#[test]
fn a_copy_kept_by_tails_and_rewritten_records_stays_whole() {
    let Some(loaded) = loaded() else { return };
    let Some(face) = family_face("Noto Emoji") else {
        eprintln!("skipping: Noto Emoji is not installed");
        return;
    };
    let mut merged = Merged::with_chain(loaded, Chain::of([face]));
    let stride = size_of::<punchcutter_text::Glyph>();

    // The consumer's copy of the record table, kept the documented way: the
    // tail past what it holds, then every record named as rewritten.
    let sync = |copy: &mut Vec<u8>, merged: &mut Merged| -> Vec<u32> {
        let whole = merged.loaded.curve_bytes()[3].to_vec();
        let held = copy.len();
        copy.extend_from_slice(&whole[held..]);
        let rewritten = merged.loaded.curves.take_rewritten();
        for &index in &rewritten {
            let at = index as usize * stride;
            if at < held {
                copy[at..at + stride].copy_from_slice(&whole[at..at + stride]);
            }
        }
        rewritten
            .into_iter()
            .filter(|&index| (index as usize * stride) < held)
            .collect()
    };

    let mut copy: Vec<u8> = Vec::new();
    sync(&mut copy, &mut merged);
    merged.cover("🇦🇺 Australia");
    sync(&mut copy, &mut merged);
    let watermark = copy.len();

    merged.cover("🇯🇵 Japan");
    let below = sync(&mut copy, &mut merged);
    eprintln!(
        "🇯🇵 Japan after 🇦🇺 Australia: records rewritten below the copy's tail at {watermark}: {below:?}"
    );
    assert!(
        !below.is_empty(),
        "no record below the first cover's tail was rewritten"
    );
    assert!(
        copy == merged.loaded.curve_bytes()[3],
        "the copy's record table differs from the merged one"
    );

    let arena = &merged.loaded.arena;
    let emoji = arena.faces().len() - 1;
    let info = arena.faces()[emoji];
    if info.glyph_count == 1895 {
        // flagJapan, gid 1734 in the 1,895-glyph build of Noto Emoji.
        let record = info.glyph_base + 1734;
        assert!(
            arena.is_filled(emoji, 1734),
            "flagJapan (1734) is not filled"
        );
        assert!(
            below.contains(&record),
            "flagJapan's record was not rewritten in place"
        );
        let glyph = merged.loaded.curves.glyphs[record as usize];
        assert!(
            glyph.hband_count > 0 && glyph.vband_count > 0,
            "flagJapan's record is the blank"
        );
    } else {
        eprintln!(
            "Noto Emoji has {} glyphs, not the 1895 of the build probed; flagJapan's gid is unchecked",
            info.glyph_count
        );
    }

    // Nothing is done twice: a third cover of the same flags fills nothing.
    assert_eq!(merged.cover("🇯🇵 🇦🇺").glyphs, 0);
}

#[test]
fn the_required_charset_is_asked_of_fontconfig_in_hex() {
    assert_eq!(
        pattern_requiring("Noto Sans", COMPOSED_CHARS),
        "Noto Sans:charset=2193 2191 b0 2013 2026"
    );
    assert_eq!(pattern_requiring("Noto Sans", &[]), "Noto Sans");
}
