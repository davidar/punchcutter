//! A source lint over `slang/modules/shape.slang` and `slang/modules/ot.slang`: a rule about
//! the shader that no compiler checks and that a future edit would quietly
//! break.

/// A name assignment's right-hand side, or `None` for a line that assigns
/// nothing — which includes every `<=`, `>=`, `==` and `!=` in a condition.
fn assigned<'a>(code: &'a str) -> Option<(&'a str, &'a str)> {
    let bytes = code.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        if *byte != b'=' {
            continue;
        }
        if bytes.get(at + 1) == Some(&b'=') {
            continue;
        }
        if at > 0
            && matches!(
                bytes[at - 1],
                b'<' | b'>' | b'!' | b'=' | b'+' | b'-' | b'*' | b'/' | b'&' | b'|' | b'^'
            )
        {
            continue;
        }
        return Some((&code[..at], &code[at + 1..]));
    }
    None
}

/// **No loop in the shaper takes its trip count from the arena.**
///
/// A font's counts are untrusted. A loop whose bound is a `u16` read straight
/// out of a damaged or hostile arena can run for tens of thousands of trips,
/// nested loops multiply that, and the budget does not bite because a bare
/// loop is not an operation: a dispatch that should take milliseconds takes a
/// hundred, and an arena of all `0xFF` can hang a GPU's graphics ring.
///
/// So the rule: a `for` or `while` may take its bound from a constant, from the
/// glyph buffer, or from `otCount`/`otTake` — which cap it *and* charge it to
/// the run's budget — and from nothing else. It fails on a raw read in a loop's
/// own header, and on a header naming a local that a raw read assigned
/// **directly** in the same function.
///
/// What it deliberately does not chase: a bound assigned from a bound, which is
/// every binary search in `ot.slang` (`hi = count`). Those are log-bounded by
/// construction — thirty-two steps whatever the bytes say — and a rule that
/// failed on them would have to be turned off rather than obeyed.
///
/// The `garbage` example measures the same property at run time, on damaged
/// arenas; this is the check that needs no GPU.
#[test]
fn no_loop_in_the_shaper_is_bounded_by_the_arena() {
    // The reads that answer with a byte of the arena. `otCount` and `otTake`
    // are the sanctioned wrappers and are *not* here.
    const RAW: [&str; 4] = ["be16(", "be32(", "otByte(", "otWord("];
    let word = |text: &str, name: &str| {
        text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|w| w == name)
    };

    let mut complaints: Vec<String> = Vec::new();
    for (file, text) in [
        ("shape.slang", punchcutter_slang::modules::SHAPE),
        ("ot.slang", punchcutter_slang::modules::OT),
    ] {
        // Taint is per function, and a function is what starts in column 0.
        let mut tainted: Vec<String> = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !code.is_empty() && !code.starts_with(char::is_whitespace) {
                tainted.clear();
            }
            if let Some((left, right)) = assigned(code) {
                if RAW.iter().any(|read| right.contains(read))
                    && !right.contains("otCount(")
                    && !right.contains("otTake(")
                    && let Some(name) = left.split_whitespace().last()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.is_empty()
                {
                    tainted.push(name.to_owned());
                }
            }

            let trimmed = code.trim_start();
            if !(trimmed.starts_with("for (") || trimmed.starts_with("while (")) {
                continue;
            }
            if let Some(read) = RAW.iter().find(|read| code.contains(**read)) {
                complaints.push(format!(
                    "{file}:{}: a loop header reads {read} — bound it with otCount/otTake\n    {}",
                    number + 1,
                    trimmed
                ));
                continue;
            }
            if let Some(name) = tainted.iter().find(|name| word(code, name)) {
                complaints.push(format!(
                    "{file}:{}: the bound `{name}` came from a raw arena read — assign it \
                     through otCount/otTake\n    {}",
                    number + 1,
                    trimmed
                ));
            }
        }
    }
    assert!(
        complaints.is_empty(),
        "a loop in the shaper takes its trip count from the arena: a font's count is \
         untrusted, and a loop it bounds can hang the GPU's graphics ring:\n{}",
        complaints.join("\n")
    );
}
