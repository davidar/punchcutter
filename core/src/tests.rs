use super::*;

/// The directory a consumer puts on its include path holds the four modules,
/// byte for byte the ones compiled in, and a second call is the same
/// directory.
#[test]
fn the_slang_dir_holds_the_engines_modules() {
    let dir = slang_dir();
    assert!(dir.is_absolute(), "{} is not absolute", dir.display());
    assert_eq!(
        dir.file_name().and_then(|name| name.to_str()),
        Some(format!("{:016x}", modules::hash()).as_str())
    );
    for (name, text) in modules::ALL {
        let on_disk = std::fs::read_to_string(dir.join(name))
            .unwrap_or_else(|err| panic!("{name} is not in {}: {err}", dir.display()));
        assert_eq!(on_disk, text, "{name} differs from the compiled copy");
    }
    assert_eq!(slang_dir(), dir);
}

/// The compiled copies are the checked-in files, so an edit is a rebuild and
/// nothing else.
#[test]
fn the_compiled_modules_are_the_checked_in_files() {
    let checked_in = Path::new(env!("CARGO_MANIFEST_DIR")).join("slang");
    for (name, text) in modules::ALL {
        let file = std::fs::read_to_string(checked_in.join(name)).expect(name);
        assert_eq!(file, text, "{name}");
    }
}

/// `write_to` makes a directory slangc can use, and makes it again over
/// itself.
#[test]
fn write_to_fills_a_directory() {
    let dir = std::env::temp_dir().join(format!("punchcutter-write-to-{}", std::process::id()));
    modules::write_to(&dir).unwrap();
    modules::write_to(&dir).unwrap();
    assert!(modules::is_written(&dir));
    std::fs::write(dir.join("ot.slang"), "// not the module").unwrap();
    assert!(!modules::is_written(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
}
