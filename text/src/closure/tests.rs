//! The closure on this machine's own fonts. Every test is skipped with a line
//! on stderr when its face is not installed, so a build container or a minimal
//! test guest stays green.

use super::*;

/// The file fontconfig lists under exactly this family, or `None`.
///
/// `fc-list`, not `fc-match`: asked for "Noto Emoji", `fc-match` answers with
/// Noto Color Emoji, which is the face a cover walks past.
pub(crate) fn family_file(family: &str) -> Option<(Vec<u8>, u32)> {
    let output = std::process::Command::new("fc-list")
        .args([family, "--format=%{family}\t%{file}\t%{index}\n"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    for line in lines {
        let mut fields = line.split('\t');
        let names = fields.next()?;
        if !names.split(',').any(|name| name.trim() == family) {
            continue;
        }
        let path = fields.next()?.trim();
        let index = fields
            .next()
            .and_then(|word| word.trim().parse().ok())
            .unwrap_or(0u32);
        if let Ok(data) = std::fs::read(path) {
            return Some((data, index & 0xffff));
        }
    }
    None
}

fn gid_of(data: &[u8], index: u32, ch: char) -> Option<u16> {
    ttf_parser::Face::parse(data, index)
        .ok()?
        .glyph_index(ch)
        .map(|gid| gid.0)
}

/// Two regional indicators ligate to a flag in Noto Emoji, and the flag is in
/// the closure of the two — the case the closure exists for: without it the
/// flag's record is never filled and draws as a gap.
#[test]
fn the_regional_indicator_pair_reaches_its_flag() {
    let Some((data, index)) = family_file("Noto Emoji") else {
        eprintln!("skipping: Noto Emoji is not installed");
        return;
    };
    let (Some(a), Some(u)) = (
        gid_of(&data, index, '\u{1F1E6}'),
        gid_of(&data, index, '\u{1F1FA}'),
    ) else {
        eprintln!("skipping: this Noto Emoji has no regional indicators");
        return;
    };
    let closure = gsub_closure(&data, index, &[a, u], Limits::COVER).expect("a face");
    eprintln!(
        "Noto Emoji 🇦🇺: +{} glyphs {:?}, {}/{} lookups rooted, {} rounds, {} steps",
        closure.added.len(),
        closure.added,
        closure.roots,
        closure.lookups,
        closure.rounds,
        closure.steps
    );
    assert_eq!(closure.bound, None, "a two-glyph seed hit a bound");
    assert!(
        !closure.added.is_empty(),
        "the flag ligature was not reached"
    );
    let face = ttf_parser::Face::parse(&data, index).expect("parsed");
    if face.number_of_glyphs() == 1895 {
        // In the 1,895-glyph build of the face, `flagAustralia` is face gid 1626, the
        // glyph hb-shape and the GPU shaper both ligate the pair to.
        assert!(
            closure.added.contains(&1626),
            "flagAustralia (1626) is not in {:?}",
            closure.added
        );
    } else {
        eprintln!(
            "note: Noto Emoji has {} glyphs, not the 1895 probed; 1626 unchecked",
            face.number_of_glyphs()
        );
    }
    // Neither seed is reported as added.
    assert!(!closure.added.contains(&a) && !closure.added.contains(&u));
}

/// The Single and Multiple substitution outputs for `gid` under the lookups of
/// the feature `tag` — the test's own reading of the table, independent of the
/// walk. Noto Sans Arabic writes its joining forms as Multiple substitutions
/// (one glyph to a sequence of one), which is why both are read.
fn joining_forms(data: &[u8], index: u32, tag: &[u8; 4], gid: u16) -> Vec<u16> {
    use ttf_parser::gsub::SingleSubstitution;
    let face = ttf_parser::Face::parse(data, index).expect("parsed");
    let gsub = face.tables().gsub.expect("an Arabic face has GSUB");
    let mut out = Vec::new();
    for feature in gsub.features {
        if feature.tag.to_bytes() != *tag {
            continue;
        }
        for lookup in feature.lookup_indices {
            let Some(lookup) = gsub.lookups.get(lookup) else {
                continue;
            };
            for sub in 0..lookup.subtables.len() {
                let found = match lookup.subtables.get::<SubstitutionSubtable>(sub) {
                    Some(SubstitutionSubtable::Single(SingleSubstitution::Format1 {
                        coverage,
                        delta,
                    })) => coverage
                        .contains(GlyphId(gid))
                        .then(|| gid.wrapping_add(delta as u16)),
                    Some(SubstitutionSubtable::Single(SingleSubstitution::Format2 {
                        coverage,
                        substitutes,
                    })) => coverage
                        .get(GlyphId(gid))
                        .and_then(|at| substitutes.get(at))
                        .map(|g| g.0),
                    Some(SubstitutionSubtable::Multiple(table)) => {
                        if let Some(seq) = table
                            .coverage
                            .get(GlyphId(gid))
                            .and_then(|at| table.sequences.get(at))
                        {
                            out.extend(seq.substitutes.into_iter().map(|g| g.0));
                        }
                        None
                    }
                    _ => None,
                };
                out.extend(found);
            }
        }
    }
    out
}

/// An Arabic word's letters close over their initial, medial and final forms,
/// which the shaper will reach once it joins Arabic and a cover filled today
/// must already hold.
#[test]
fn an_arabic_word_reaches_its_joining_forms() {
    let Some((data, index)) = family_file("Noto Sans Arabic") else {
        eprintln!("skipping: Noto Sans Arabic is not installed");
        return;
    };
    let word = "مرحبا بالعالم";
    let seeds: Vec<u16> = word
        .chars()
        .filter_map(|ch| gid_of(&data, index, ch))
        .collect();
    let closure = gsub_closure(&data, index, &seeds, Limits::COVER).expect("a face");
    let mut want: Vec<u16> = Vec::new();
    for seed in &seeds {
        for tag in [b"init", b"medi", b"fina"] {
            want.extend(joining_forms(&data, index, tag, *seed));
        }
    }
    want.sort_unstable();
    want.dedup();
    eprintln!(
        "Noto Sans Arabic {word:?}: {} seeds, +{} glyphs, {} joining forms expected, {}/{} lookups rooted, {} rounds, {} steps",
        seeds.len(),
        closure.added.len(),
        want.len(),
        closure.roots,
        closure.lookups,
        closure.rounds,
        closure.steps
    );
    assert_eq!(closure.bound, None);
    assert!(
        want.len() >= 6,
        "the test's own reading found only {} forms",
        want.len()
    );
    for form in &want {
        assert!(
            closure.added.contains(form) || seeds.contains(form),
            "joining form {form} is not in the closure"
        );
    }
}

/// A face without `GSUB` closes to its seeds.
#[test]
fn a_face_without_gsub_adds_nothing() {
    let output = std::process::Command::new("fc-list")
        .args(["--format=%{file}\n"])
        .output();
    let Ok(output) = output else {
        eprintln!("skipping: no fontconfig");
        return;
    };
    let mut files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    files.sort_unstable();
    for file in files {
        let Ok(data) = std::fs::read(&file) else {
            continue;
        };
        let Ok(face) = ttf_parser::Face::parse(&data, 0) else {
            continue;
        };
        if face.tables().gsub.is_some() {
            continue;
        }
        let Some(a) = face.glyph_index('A') else {
            continue;
        };
        let closure = gsub_closure(&data, 0, &[a.0], Limits::COVER).expect("a face");
        assert!(
            closure.added.is_empty(),
            "{file} has no GSUB and added {:?}",
            closure.added
        );
        assert_eq!(closure.lookups, 0);
        return;
    }
    eprintln!("skipping: every installed face has a GSUB");
}

/// A bound ends the walk with what it has and names itself, and nothing past
/// the face's glyph count gets in.
#[test]
fn a_bound_ends_the_walk_and_says_which() {
    let Some((data, index)) = family_file("Noto Sans Arabic") else {
        eprintln!("skipping: Noto Sans Arabic is not installed");
        return;
    };
    let seeds: Vec<u16> = "مرحبا"
        .chars()
        .filter_map(|ch| gid_of(&data, index, ch))
        .collect();
    let one = Limits {
        added: 1,
        ..Limits::COVER
    };
    let closure = gsub_closure(&data, index, &seeds, one).expect("a face");
    assert_eq!(closure.bound, Some(Bound::Added));
    assert_eq!(closure.added.len(), 1);

    let broke = Limits {
        steps: 10,
        ..Limits::COVER
    };
    let closure = gsub_closure(&data, index, &seeds, broke).expect("a face");
    assert_eq!(closure.bound, Some(Bound::Steps));

    let glyphs = ttf_parser::Face::parse(&data, index)
        .expect("parsed")
        .number_of_glyphs();
    let closure = gsub_closure(&data, index, &[glyphs, u16::MAX], Limits::COVER).expect("a face");
    assert!(
        closure.added.is_empty(),
        "seeds past the face are not glyphs"
    );
    assert!(gsub_closure(b"not a font", 0, &[1], Limits::COVER).is_err());
}
