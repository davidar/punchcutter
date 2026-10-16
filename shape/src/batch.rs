//! The per-dispatch half: strings in, slots out.
//!
//! A [`Batch`] is a pile of strings to shape in one dispatch — a clock, a row
//! of labels, a menu's items — concatenated as UTF-8 and indexed by one
//! [`ShapeRequest`] each. One thread shapes one string, so a batch of forty
//! strings is one dispatch of forty threads, which is the shape of the work a
//! user interface has.
//!
//! Nothing here looks at the font. The batch is bytes and offsets; the shader
//! does the rest.

use crate::abi::{
    MAX_RUN_GLYPHS, MAX_SHAPE_FEATURES, SHAPE_ABI_VERSION, SHAPE_FEATURE_ON, ShapeParams,
    ShapeRequest,
};
use crate::font::Arena;

/// One feature a run forces on or off, global over the whole run.
///
/// No ranges and no values above one: HarfBuzz's `hb_feature_t` has a start, an
/// end and a value, and every one of the 276 cases in aots (the Annotated OpenType Specification tests, part of the corpus `examples/conform.rs` runs) sets `--features="test"`, which is a tag and nothing else.
/// A language's `locl` and a complex script's masks are what would need the
/// rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The four-byte OpenType tag, as it is spelled.
    pub tag: [u8; 4],
    /// Force it on, or force it off.
    pub on: bool,
}

impl Feature {
    pub const fn on(tag: &[u8; 4]) -> Feature {
        Feature {
            tag: *tag,
            on: true,
        }
    }

    pub const fn off(tag: &[u8; 4]) -> Feature {
        Feature {
            tag: *tag,
            on: false,
        }
    }

    /// The word the shader reads: the packed tag, plus one bit that says which
    /// way. A tag is four ASCII bytes, so bit 31 is free.
    pub fn word(&self) -> u32 {
        u32::from_be_bytes(self.tag) | if self.on { SHAPE_FEATURE_ON } else { 0 }
    }
}

/// Strings to shape in one dispatch.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    /// Every string's bytes, concatenated. Uploaded as `u32` words by
    /// [`Batch::utf8_words`] — Slang has no byte-addressed storage buffer
    /// without an extension, and packing four bytes per word is cheaper than
    /// asking for one.
    bytes: Vec<u8>,
    requests: Vec<ShapeRequest>,
    glyph_capacity: u32,
    /// The dispatch's feature pool, which a request names a slice of. Runs that
    /// force the same features share one slice, so a corpus file whose every
    /// case sets `--features="test"` costs the pool one word.
    features: Vec<u32>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Add a string at a pixel size; returns its request index, which is also
    /// its index in the run array the shader writes.
    ///
    /// A string is given **four glyph slots per byte plus eight**, capped at
    /// [`MAX_RUN_GLYPHS`].
    ///
    /// One slot per byte is wrong, and the HarfBuzz corpus shows why: a
    /// ligature removes glyphs, but a **MultipleSubst adds them**, and a
    /// context whose nested lookup is one turns two input glyphs into five.
    /// A run that hits the cap comes back [`SHAPE_RUN_TRUNCATED`], which is
    /// never a wrong answer but is not the answer either.
    pub fn push(&mut self, text: &str, px_size: f32) -> usize {
        self.push_with_features(text, px_size, &[])
            .expect("a run that forces no feature always fits the pool")
    }

    /// The same, forcing a small fixed list of features on or off over the
    /// whole run.
    ///
    /// `None` means the dispatch's feature pool ([`MAX_SHAPE_FEATURES`] words)
    /// cannot hold another distinct set, and the caller should start a new
    /// batch — never a run shaped with the wrong features, which is the one
    /// answer a conformance harness must not be given quietly.
    pub fn push_with_features(
        &mut self,
        text: &str,
        px_size: f32,
        features: &[Feature],
    ) -> Option<usize> {
        let (feature_start, feature_count) = self.intern(features)?;
        let byte_start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(text.as_bytes());
        let out_cap = (text.len() as u32 * 4 + 8).min(MAX_RUN_GLYPHS);
        let index = self.requests.len();
        self.requests.push(ShapeRequest {
            byte_start,
            byte_len: text.len() as u32,
            px_size,
            out_start: self.glyph_capacity,
            out_cap,
            feature_start,
            feature_count,
            _rsvd0: 0,
        });
        self.glyph_capacity += out_cap;
        Some(index)
    }

    /// Where this run's feature words sit in the pool, sharing a slice with an
    /// earlier run that forced the same ones.
    fn intern(&mut self, features: &[Feature]) -> Option<(u32, u32)> {
        if features.is_empty() {
            return Some((0, 0));
        }
        let words: Vec<u32> = features.iter().map(Feature::word).collect();
        if let Some(at) = self
            .features
            .windows(words.len())
            .position(|run| run == words)
        {
            return Some((at as u32, words.len() as u32));
        }
        if self.features.len() + words.len() > MAX_SHAPE_FEATURES as usize {
            return None;
        }
        let at = self.features.len() as u32;
        self.features.extend_from_slice(&words);
        Some((at, words.len() as u32))
    }

    /// The requests, for upload.
    pub fn requests(&self) -> &[ShapeRequest] {
        &self.requests
    }

    /// Strings in the batch.
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Glyph slots the output array needs. Always at least one, so no buffer
    /// is created with size zero.
    pub fn glyph_capacity(&self) -> u32 {
        self.glyph_capacity.max(1)
    }

    /// The UTF-8 buffer as `u32` words, little-endian, zero-padded.
    pub fn utf8_words(&self) -> Vec<u32> {
        let mut words = Vec::with_capacity(self.bytes.len().div_ceil(4).max(1));
        for chunk in self.bytes.chunks(4) {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            words.push(u32::from_le_bytes(word));
        }
        if words.is_empty() {
            words.push(0);
        }
        words
    }

    /// The dispatch's scalars for `arena`, the feature pool included.
    ///
    /// `deadline_ticks` is left at zero — the caller sets it, because only the
    /// caller knows whether its device came up with `VK_KHR_shader_clock` and
    /// what one tick of that clock is worth (see `SHAPE_DEADLINE_TICKS`).
    pub fn params(&self, arena: &Arena) -> ShapeParams {
        let mut features = [[0u32; 4]; 4];
        for (at, word) in self.features.iter().enumerate() {
            features[at / 4][at % 4] = *word;
        }
        ShapeParams {
            font: arena.font(),
            request_count: self.requests.len() as u32,
            glyph_capacity: self.glyph_capacity(),
            version: SHAPE_ABI_VERSION,
            deadline_ticks: 0,
            features,
        }
    }

    /// Workgroups to dispatch for this batch, at [`crate::abi::SHAPE_WORKGROUP`]
    /// threads each.
    pub fn workgroups(&self) -> u32 {
        (self.requests.len() as u32)
            .div_ceil(crate::abi::SHAPE_WORKGROUP)
            .max(1)
    }
}

#[cfg(test)]
mod tests;
