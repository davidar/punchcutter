//! Bakes `slang/modules/text_demo.slang` — and with it `slang/modules/text.slang`,
//! which it imports — into SPIR-V for the harness.
//!
//! The compiler is `punchcutter_slang`, in process, the way a consumer's build
//! script would use it. One entry point, one module, no variants: nothing
//! here writes a swapchain image.
//!
//! Baking it at build time rather than in the example means a Slang mistake is a
//! build failure, which is the cheaper place to find one.

use std::path::PathBuf;

use punchcutter_slang::Options;

const ENTRY: &str = "text_demo";

fn main() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../slang/modules/text_demo.slang");

    // The demo's own file: `text.slang` travels inside `punchcutter-slang`,
    // so an edit to it rebuilds that crate and this script with it.
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));

    let mut program = punchcutter_slang::compile_program(&source, &[ENTRY], &Options::default())
        .unwrap_or_else(|err| panic!("{err}"));
    // Nothing subscribes to `tracing` in a build script, so Slang's
    // success-path warnings are surfaced by hand. The gate is zero of them.
    for line in program.warnings.lines() {
        println!("cargo:warning=slang: {line}");
    }

    let output = out_dir.join(format!("{ENTRY}.spv"));
    std::fs::write(&output, &program.entries.remove(0).spirv)
        .unwrap_or_else(|err| panic!("could not write {}: {err}", output.display()));
}
