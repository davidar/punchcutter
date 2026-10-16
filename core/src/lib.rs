//! punchcutter-core — the machinery under the engine's Slang modules.
//!
//! The text engine is three Slang modules and the CPU code that fills their
//! buffers. What the two halves share is mechanism, and that is all this crate
//! is: the slangc driver (compile, reflect, validate), the byte views that
//! upload plain structs to the GPU, the declare-once ABI renderer that keeps a
//! Rust struct and its Slang mirror from drifting, and a few wgpu helpers.
//! Nothing here decides anything.
//!
//! # The modules
//!
//! - [`slang`] — the slangc driver: compile, reflect, validate. Used by build
//!   scripts, by the layout tests, and by a consumer that compiles its own
//!   shader against the engine's modules.
//! - [`plain`] — the `Plain` trait and its three byte views, the only unsafe
//!   code on the upload path.
//! - [`decl`] — **declared once**: the two `macro_rules!` an ABI struct or
//!   constant is written through, and the renderer that turns their tables into
//!   a checked-in Slang module.
//! - [`gpu`] — the wgpu-typed helpers, behind the `gpu` feature, so a build
//!   script can depend on this crate without pulling in wgpu.

pub mod decl;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod plain;
pub mod slang;
