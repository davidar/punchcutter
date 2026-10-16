use super::*;

/// The four modules a consumer imports, or that the two it imports import in
/// turn, are where [`slang_dir`] says.
#[test]
fn the_slang_dir_holds_the_engines_modules() {
    let dir = slang_dir();
    assert!(dir.is_absolute(), "{} is not absolute", dir.display());
    for module in ["text.slang", "shape.slang", "ot.slang", "shape_abi.slang"] {
        assert!(
            dir.join(module).is_file(),
            "{} is not in {}",
            module,
            dir.display()
        );
    }
}
