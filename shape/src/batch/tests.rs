use super::*;

#[test]
fn a_batch_lays_its_strings_out_end_to_end() {
    let mut batch = Batch::new();
    assert_eq!(batch.push("ab", 16.0), 0);
    assert_eq!(batch.push("—", 28.0), 1);
    let requests = batch.requests();
    assert_eq!(requests[0].byte_start, 0);
    assert_eq!(requests[0].byte_len, 2);
    assert_eq!(requests[0].out_start, 0);
    // Four slots a byte plus eight: a MultipleSubst *adds* glyphs, which is
    // what one slot per byte would miss.
    assert_eq!(requests[0].out_cap, 16);
    assert_eq!(requests[1].byte_start, 2);
    assert_eq!(requests[1].byte_len, 3);
    assert_eq!(requests[1].out_start, 16);
    assert_eq!(requests[1].out_cap, 20);
    assert_eq!(batch.glyph_capacity(), 36);
    assert_eq!(batch.workgroups(), 1);
}

#[test]
fn the_words_are_the_bytes_little_endian() {
    let mut batch = Batch::new();
    batch.push("abcde", 16.0);
    assert_eq!(
        batch.utf8_words(),
        vec![
            u32::from_le_bytes(*b"abcd"),
            u32::from_le_bytes([b'e', 0, 0, 0]),
        ]
    );
}

#[test]
fn an_empty_batch_still_asks_for_one_word_and_one_slot() {
    let batch = Batch::new();
    assert_eq!(batch.utf8_words(), vec![0]);
    assert_eq!(batch.glyph_capacity(), 1);
    assert!(batch.is_empty());
}

#[test]
fn a_very_long_string_is_capped_at_the_run_limit() {
    let mut batch = Batch::new();
    let long = "x".repeat(MAX_RUN_GLYPHS as usize + 64);
    batch.push(&long, 16.0);
    assert_eq!(batch.requests()[0].out_cap, MAX_RUN_GLYPHS);
}

/// The feature pool is the dispatch's and a request names a slice of it, so runs forcing the same features share one slice: a corpus file whose every case forces `test` costs one word.
#[test]
fn runs_that_force_the_same_features_share_one_slice() {
    use crate::abi::{MAX_SHAPE_FEATURES, SHAPE_FEATURE_ON};
    use crate::batch::Feature;

    let test = [Feature::on(b"test")];
    let mut batch = Batch::new();
    batch.push("plain", 16.0);
    batch.push_with_features("a", 16.0, &test).expect("fits");
    batch.push_with_features("b", 16.0, &test).expect("fits");
    batch
        .push_with_features("c", 16.0, &[Feature::off(b"liga"), Feature::on(b"ss01")])
        .expect("fits");

    let want = [(0, 0), (0, 1), (0, 1), (1, 2)];
    for (index, (start, count)) in want.into_iter().enumerate() {
        let req = batch.requests()[index];
        assert_eq!(
            (req.feature_start, req.feature_count),
            (start, count),
            "run {index}"
        );
    }

    // The words as the shader reads them: a packed tag, and one bit for which
    // way. `Batch::params` needs an arena, so the pool is read through the
    // requests' own ranges above and the words are checked here.
    let on = u32::from_be_bytes(*b"test") | SHAPE_FEATURE_ON;
    let off = u32::from_be_bytes(*b"liga");
    assert_eq!(Feature::on(b"test").word(), on);
    assert_eq!(Feature::off(b"liga").word(), off);

    // And the pool is bounded: a run that will not fit is `None`, never a run
    // shaped with the wrong features.
    let mut full = Batch::new();
    let many: Vec<Feature> = (0..MAX_SHAPE_FEATURES)
        .map(|i| Feature::on(&[b'a', b'a', b'a', b'a' + i as u8]))
        .collect();
    assert!(full.push_with_features("x", 16.0, &many).is_some());
    assert!(
        full.push_with_features("y", 16.0, &[Feature::on(b"zzzz")])
            .is_none()
    );
    // A run forcing nothing always fits, however full the pool is.
    assert!(full.push_with_features("z", 16.0, &[]).is_some());
}
