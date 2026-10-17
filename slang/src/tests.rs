use super::*;

/// The checked-in modules, resolved against this crate so the tests do not
/// care about the harness's working directory.
fn modules_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("modules")
}

/// A scratch directory of this process's own, a few small files that the
/// caller removes.
fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("punchcutter-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

// ---------------------------------------------------------------------------
// The modules
// ---------------------------------------------------------------------------

/// The directory the engine's modules are written to holds the four, byte
/// for byte the ones compiled in, and a second call is the same directory.
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
    for (name, text) in modules::ALL {
        let file = std::fs::read_to_string(modules_dir().join(name)).expect(name);
        assert_eq!(file, text, "{name}");
    }
}

/// `write_to` makes a directory Slang can search, and makes it again over
/// itself.
#[test]
fn write_to_fills_a_directory() {
    let dir = scratch("write-to");
    modules::write_to(&dir).unwrap();
    modules::write_to(&dir).unwrap();
    assert!(modules::is_written(&dir));
    std::fs::write(dir.join("ot.slang"), "// not the module").unwrap();
    assert!(!modules::is_written(&dir));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A reflection reads back from its JSON without the compiler, and garbage
/// is an error, not a panic.
#[test]
fn a_reflection_reads_back_from_its_json() {
    let reflection = Reflection {
        bindings: vec![Binding {
            name: "sink".into(),
            set: 0,
            index: 1,
            kind: BindingKind::Storage {
                read_only: false,
                stride: Some(4),
            },
        }],
        structs: [("Row".to_string(), [("a".to_string(), 0usize)].into())].into(),
        strings: [(7u32, "seven".to_string())].into(),
    };
    assert_eq!(
        Reflection::from_json(&reflection.to_json()).unwrap(),
        reflection
    );
    assert!(
        matches!(
            Reflection::from_json("{\"bindings\": 3}"),
            Err(SlangError::Reflection { .. })
        ),
        "garbage is an error, not a panic"
    );
}
