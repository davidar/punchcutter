//! The harness shader's own two structs, mirroring `slang/modules/text_demo.slang`.
//!
//! They are not part of the curve buffers — a consumer does not bind them — but
//! the harness uploads them, so they are pinned by `tests/layout.rs` alongside the
//! four that are. A demo whose parameters silently shift is a demo that proves
//! nothing.

/// One glyph, placed in output pixels: `origin` is on the baseline at the pen,
/// y down, which is where a shaper's output lands.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Placement {
    pub glyph: u32,
    pub px_per_em: f32,
    pub origin: [f32; 2],
}

/// The dispatch's bounds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub count: u32,
    pub _rsvd0: u32,
}

// SAFETY: `#[repr(C)]` aggregates of `u32`/`f32` with no implicit padding.
unsafe impl punchcutter_core::plain::Plain for Placement {}
unsafe impl punchcutter_core::plain::Plain for Params {}

#[cfg(test)]
mod tests;
