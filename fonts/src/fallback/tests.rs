//! The chain, and the cache file behind it. Every test that needs a real font
//! is skipped rather than failed on a machine without one, the way the rest of
//! this crate's tests are.

use super::*;
use punchcutter_shape::Arena;

/// A face fontconfig can name, or `None` — the tests' stand-in for "a machine
/// with fonts".
fn a_face(pattern: &str) -> Option<(String, PathBuf, u32)> {
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
    let index = fields
        .next()
        .and_then(|word| word.trim().parse().ok())
        .unwrap_or(0);
    path.is_file().then_some((family, path, index))
}

#[test]
fn the_chain_is_fontconfigs_order_without_the_primary() {
    let Some((_, primary, _)) = a_face("sans-serif") else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    let chain = Chain::discover(&primary);
    if chain.is_empty() {
        eprintln!("skipping: this machine has one font");
        return;
    }
    assert!(chain.len() <= MAX_CHAIN);
    // The primary is not in its own fallback chain — asking it again for a
    // codepoint it just failed to have is the one certain waste.
    assert!(
        !chain.faces.iter().any(|face| face.path == primary),
        "the primary is in its own chain"
    );
    // And no face is listed twice, however many families point at one file.
    for (i, face) in chain.faces.iter().enumerate() {
        assert!(
            !chain.faces[..i]
                .iter()
                .any(|seen| seen.path == face.path && seen.index == face.index),
            "{} appears twice",
            face.path.display()
        );
    }
}

/// The chain's whole job, on this machine's own fonts: a codepoint the Latin
/// primary lacks comes back drawn, from a face the log can name, appended
/// to the buffers that were handed in.
#[test]
fn a_codepoint_the_primary_lacks_comes_back_drawn() {
    let Some((_, primary, _)) = a_face("sans-serif") else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    let Some((_, cjk, index)) = a_face(":lang=ja") else {
        eprintln!("skipping: no Japanese face on this machine");
        return;
    };
    let data = std::fs::read(&cjk).expect("the file fontconfig named");
    if punchcutter_text::font::codepoint_curves(&data, index, '日')
        .ok()
        .flatten()
        .is_none()
    {
        eprintln!("skipping: the :lang=ja face has no 日");
        return;
    }
    let _ = primary;

    let mut chain = Chain::of([("Test CJK".to_owned(), cjk, index)]);
    let mut curves = CurveSet::default();
    // Two glyphs in the set already, so an append lands on non-zero bases.
    let mut builder = Builder::resume(std::mem::take(&mut curves));
    builder.push(&[], 0.5, false);
    builder.push(&[], 0.5, false);
    curves = builder.finish();
    let before = curves.bytes();

    let mut arena = Arena::new();
    arena.set_glyph_count(curves.glyphs.len() as u32);
    let covered = chain
        .cover(&mut curves, &mut arena, '日')
        .expect("a face with the glyph");
    // The face took its whole glyph range from the watermark, so the merged id
    // is `glyph_base + face gid` and the records reach past it.
    assert_eq!(arena.faces().len(), 1, "the face was opened into the arena");
    let info = arena.faces()[0];
    assert_eq!(info.glyph_base, 2, "reserved after what was there");
    assert!(covered.gid >= info.glyph_base);
    assert!(covered.gid < info.glyph_base + info.glyph_count);
    assert_eq!(
        curves.glyphs.len() as u32,
        info.glyph_base + info.glyph_count
    );
    assert!(
        arena.is_filled(0, covered.gid - info.glyph_base),
        "the bit was set"
    );
    assert_eq!(covered.family, "Test CJK");
    assert!(
        covered.bytes > 0 && curves.bytes() > before,
        "nothing was appended"
    );
    assert!(!covered.warm, "the first build cannot be a cache hit");
    let glyph = curves.glyphs[covered.gid as usize];
    assert!(
        glyph.hband_count > 0 && glyph.vband_count > 0,
        "no bands: no ink"
    );
    // Em units, like every other glyph in the merged space.
    assert!(
        glyph.bounds[2] < 4.0 && glyph.bounds[3] < 4.0,
        "{:?} is not em units",
        glyph.bounds
    );
}

/// A codepoint nothing can draw is one log line and then silence: the chain
/// remembers it, so a title full of them is one walk and not one per frame.
#[test]
fn a_codepoint_no_face_has_is_remembered() {
    let Some((family, path, index)) = a_face("sans-serif") else {
        eprintln!("skipping: fontconfig named no sans-serif");
        return;
    };
    let mut chain = Chain::of([(family, path, index)]);
    let mut curves = CurveSet::default();
    let mut arena = Arena::new();
    // A private-use codepoint, which no text font claims.
    assert!(chain.cover(&mut curves, &mut arena, '\u{f8ff}').is_none());
    assert_eq!(chain.hopeless(), &['\u{f8ff}' as u32]);
    assert!(chain.cover(&mut curves, &mut arena, '\u{f8ff}').is_none());
    assert_eq!(chain.hopeless().len(), 1, "asked once");
    assert!(curves.glyphs.is_empty(), "nothing was appended for it");
    assert!(arena.faces().is_empty(), "no face was opened for it");
}

/// A face that is not a face is one log line and then skipped for good, rather
/// than an error that stops the cover: the chain is other people's files.
#[test]
fn a_file_that_is_not_a_face_is_skipped() {
    let scratch = std::env::temp_dir().join("punchcutter-fonts-not-a-face.ttf");
    std::fs::write(&scratch, b"this is not a font").expect("a writable temp dir");
    let mut chain = Chain::of([("Nonsense".to_owned(), scratch.clone(), 0)]);
    let mut curves = CurveSet::default();
    let mut arena = Arena::new();
    assert!(chain.cover(&mut curves, &mut arena, 'A').is_none());
    assert!(chain.cover(&mut curves, &mut arena, 'B').is_none());
    let _ = std::fs::remove_file(&scratch);
}

/// The cache file round-trips the records and the glyph-id list, and anything
/// that is not one of ours is a miss rather than an error.
#[test]
fn the_cache_file_round_trips_and_refuses_anything_else() {
    let mut builder = Builder::new();
    builder.push(&[], 0.5, false);
    builder.push(&[], 0.25, false);
    let set = builder.finish();
    let gids = vec![7u16, 4001];

    let bytes = encode(&gids, &set);
    let (back_gids, back_set) = decode(&bytes).expect("what this file just wrote");
    assert_eq!(back_gids, gids);
    assert_eq!(back_set.glyphs, set.glyphs);
    assert_eq!(back_set.index, set.index);
    assert_eq!(back_set.points, set.points);

    assert!(decode(b"").is_none());
    assert!(
        decode(b"PNCHCRVS and then some bytes").is_none(),
        "the primary's own cache"
    );
    let mut wrong_version = bytes.clone();
    wrong_version[8] = 0xff;
    assert!(decode(&wrong_version).is_none());
    assert!(
        decode(&bytes[..bytes.len() - 4]).is_none(),
        "a truncated file"
    );
}

/// **Colour emoji is not drawn yet.** A face that claims the codepoint and draws it
/// with a colour table has no outline for it, so the chain walks past rather
/// than appending a blank — which is what leaves a colour-only emoji a
/// `.notdef`.
///
/// On a typical Fedora install this does not fire from the sans-serif chain at
/// all (fontconfig keeps Noto Color Emoji out of it and the monochrome Noto
/// Emoji is in), so the face is named here directly.
#[test]
fn a_colour_only_face_is_walked_past() {
    let Some((family, path, index)) = a_face("emoji") else {
        eprintln!("skipping: no emoji face on this machine");
        return;
    };
    let data = std::fs::read(&path).expect("the file fontconfig named");
    let drawn = punchcutter_text::font::codepoint_curves(&data, index, '\u{1f600}')
        .ok()
        .flatten();
    let Some(glyph) = drawn else {
        eprintln!("skipping: this emoji face has no U+1F600 at all");
        return;
    };
    if glyph.has_ink() {
        eprintln!(
            "skipping: {family} is a monochrome emoji face, which the fallback chain does append"
        );
        return;
    }
    let mut chain = Chain::of([(family, path, index)]);
    let mut curves = CurveSet::default();
    let mut arena = Arena::new();
    assert!(
        chain.cover(&mut curves, &mut arena, '\u{1f600}').is_none(),
        "a colour glyph was appended"
    );
    assert!(curves.glyphs.is_empty());
    assert_eq!(
        chain.hopeless(),
        &['\u{1f600}' as u32],
        "and it is asked once"
    );
}

/// A face with no `GSUB` lookups, and a codepoint it draws, from what
/// fontconfig lists — or `None`.
fn a_face_without_gsub() -> Option<((String, PathBuf, u32), char)> {
    let output = std::process::Command::new("fc-list")
        .args(["--format=%{family}\t%{file}\t%{index}\n"])
        .output()
        .ok()?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort_unstable();
    for line in lines {
        let mut fields = line.split('\t');
        let (Some(family), Some(file)) = (fields.next(), fields.next()) else {
            continue;
        };
        let index: u32 = fields
            .next()
            .and_then(|word| word.trim().parse().ok())
            .unwrap_or(0);
        let Ok(data) = std::fs::read(file) else {
            continue;
        };
        let Ok(closure) = punchcutter_text::closure::gsub_closure(
            &data,
            index,
            &[],
            punchcutter_text::closure::Limits::COVER,
        ) else {
            continue;
        };
        if closure.lookups != 0 {
            continue;
        }
        let drawn = punchcutter_text::font::codepoint_curves(&data, index, 'A')
            .ok()
            .flatten();
        if drawn.is_some_and(|glyph| glyph.has_ink()) {
            let family = family.split(',').next().unwrap_or(family).trim().to_owned();
            return Some(((family, PathBuf::from(file), index), 'A'));
        }
    }
    None
}

/// A face without `GSUB` fills exactly its codepoints: the closure runs,
/// reaches nothing, and says so.
#[test]
fn a_face_without_gsub_fills_exactly_its_codepoints() {
    let Some((face, ch)) = a_face_without_gsub() else {
        eprintln!("skipping: every installed face has GSUB lookups");
        return;
    };
    let family = face.0.clone();
    let mut chain = Chain::of([face]);
    let mut curves = CurveSet::default();
    let mut arena = Arena::new();
    chain
        .cover(&mut curves, &mut arena, ch)
        .expect("the face draws it");
    let closed = chain.close(&mut curves, &mut arena);
    assert_eq!(closed.len(), 1, "the face that grew was closed");
    assert_eq!(
        closed[0].glyphs, 0,
        "{family} has no GSUB and the closure filled something"
    );
    assert_eq!(closed[0].lookups, 0);
    let info = arena.faces()[0];
    let filled = (0..info.glyph_count)
        .filter(|gid| arena.is_filled(0, *gid))
        .count();
    assert_eq!(filled, 1, "{family}: exactly the one codepoint");
    // Closing again with nothing new filled is not a walk at all.
    assert!(chain.close(&mut curves, &mut arena).is_empty());
}

/// The closure's glyphs go through the per-face cache like a codepoint's: a
/// second process (a second chain) finds them there and outlines nothing.
#[test]
fn the_closure_is_cached_with_the_codepoints() {
    let Some(face) = crate::tests::family_face("Noto Emoji") else {
        eprintln!("skipping: Noto Emoji is not installed");
        return;
    };
    let run = |face: (String, PathBuf, u32)| {
        let mut chain = Chain::of([face]);
        let mut curves = CurveSet::default();
        let mut arena = Arena::new();
        for ch in ['\u{1F1E6}', '\u{1F1FA}'] {
            chain
                .cover(&mut curves, &mut arena, ch)
                .expect("Noto Emoji draws it");
        }
        let closed = chain.close(&mut curves, &mut arena);
        chain.settle();
        let cache_bytes = chain.faces[0]
            .cache
            .file
            .as_ref()
            .and_then(|file| std::fs::metadata(file).ok())
            .map(|meta| meta.len())
            .unwrap_or(0);
        (closed, cache_bytes)
    };
    let (first, _) = run(face.clone());
    assert_eq!(first.len(), 1);
    assert!(first[0].glyphs >= 1, "the flag ligature was not filled");
    let (second, cache_bytes) = run(face);
    eprintln!(
        "Noto Emoji closure: +{} glyphs, cold {:.2} ms / warm {:.2} ms, cache file {} bytes",
        second[0].glyphs,
        first[0].micros as f64 / 1000.0,
        second[0].micros as f64 / 1000.0,
        cache_bytes
    );
    assert_eq!(second[0].glyphs, first[0].glyphs);
    assert!(second[0].warm, "the closure's glyphs were outlined again");
}
