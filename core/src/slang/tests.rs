use super::*;

/// The workspace's `slang/`, resolved against this crate so the tests do not
/// care about the harness's working directory. This crate sits one level down
/// from the root, like every other member.
fn slang_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../slang")
}

/// The SPIR-V magic number, little-endian.
fn assert_spirv(module: &[u8]) {
    assert!(module.len() > 20, "suspiciously short module");
    assert_eq!(
        &module[..4],
        &[0x03, 0x02, 0x23, 0x07],
        "output does not start with the SPIR-V magic number"
    );
}

#[test]
fn syntax_errors_surface_the_slangc_diagnostic() {
    let path =
        std::env::temp_dir().join(format!("punchcutter-broken-{}.slang", std::process::id()));
    std::fs::write(
        &path,
        "[shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void broken(uint3 gid: SV_DispatchThreadID)\n\
         {\n    this is not slang;\n}\n",
    )
    .expect("temp dir is writable");

    let err = compile(&path, "broken", &[], &[]).expect_err("garbage should not compile");
    let _ = std::fs::remove_file(&path);

    let rendered = err.to_string();
    assert!(
        matches!(err, SlangError::Compile { .. }),
        "expected a compile failure, got: {rendered}"
    );
    // The diagnostic must survive intact: file, line, and slangc's own
    // wording are what a caller shows the person editing the file.
    let diagnostics = err.diagnostics().expect("compile errors carry diagnostics");
    assert!(
        diagnostics.contains("error"),
        "diagnostics lost slangc's text: {diagnostics:?}"
    );
    assert!(
        rendered.contains(&diagnostics.to_string()),
        "Display dropped the diagnostics: {rendered}"
    );
    assert!(
        rendered.contains("broken"),
        "Display should name the entry point: {rendered}"
    );
}

#[test]
fn a_missing_compiler_is_reported_as_such() {
    let program = OsString::from("slangc-that-does-not-exist");
    let err = compile_with(
        &program,
        &slang_dir().join("text_demo.slang"),
        "text_demo",
        &[],
        &[],
        &[],
    )
    .expect_err("a missing binary cannot compile anything");
    assert!(matches!(err, SlangError::Spawn { .. }), "{err}");
    assert!(err.to_string().contains("$SLANGC"), "{err}");
}

#[test]
fn temp_output_is_cleaned_up() {
    let path = {
        let temp = TempPath::new(Path::new("slang/text_demo.slang"), "text_demo", "spv");
        std::fs::write(temp.path(), b"scratch").expect("temp dir is writable");
        temp.path().to_path_buf()
    };
    assert!(!path.exists(), "TempPath should remove {}", path.display());
}

/// The three literals of a scratch shader, recovered from the same slangc run
/// that produced the module, with no second slangc run. One of the three hashes
/// is negative as JSON and one literal is not ASCII, the two shapes that break
/// a naive reader.
#[test]
fn the_hashed_strings_table_rides_on_the_compile() {
    let dir = std::env::temp_dir().join(format!("punchcutter-strings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir is writable");
    let path = dir.join("literals.slang");
    std::fs::write(
        &path,
        "RWStructuredBuffer<uint> sink;\n\
         \n\
         [shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void named(uint3 gid: SV_DispatchThreadID)\n\
         {\n\
         \x20   sink[gid.x] = getStringHash(\"New window\")\n\
         \x20       ^ getStringHash(\"Close\")\n\
         \x20       ^ getStringHash(\"Open — now\");\n\
         }\n",
    )
    .expect("temp dir is writable");

    let compiled = compile_capturing(&slangc_program(), &path, "named", &[], &[], &[])
        .unwrap_or_else(|err| panic!("the scratch module should compile:\n{err}"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);

    assert_spirv(&compiled.spirv);
    let table = &compiled.strings;
    assert_eq!(table.len(), 3, "{table:?}");
    let literals: Vec<&str> = table.values().map(String::as_str).collect();
    for literal in ["New window", "Close", "Open — now"] {
        assert!(
            literals.contains(&literal),
            "{literal:?} is missing: {table:?}"
        );
    }
    // "Close" hashes to -1629961352 as JSON — the signed reading is a fact
    // about the file, not about the shader, which sees a `uint` with the
    // top bit set. Asserting the *shape* rather than the constant keeps a
    // slangc that changes its hash function from failing this test for the
    // wrong reason.
    assert!(
        table.keys().any(|hash| hash & 0x8000_0000 != 0),
        "one of these hashes is negative as JSON: {table:?}"
    );
}

/// The cast, pinned on a fixture rather than on slangc's hash function:
/// `-1629961352` in the JSON is `2665005944` to the shader.
#[test]
fn a_negative_hash_is_read_as_the_unsigned_bits() {
    let table =
        hashed_strings("{\"hashedStrings\": {\"Close\": -1629961352}}").expect("a fixture parses");
    assert_eq!(
        table.get(&2_665_005_944),
        Some(&"Close".to_string()),
        "{table:?}"
    );
}

/// slangc escapes a non-ASCII byte in octal, which is not JSON. The repair
/// is load-bearing: without it the first accented string in a shader would
/// take the whole table down with it.
#[test]
fn octal_escapes_decode_to_the_utf8_they_stand_for() {
    let json = "{\"hashedStrings\": {\"Bluetooth \\342\\200\\224 off\": -816505869,\n\
                \x20   \"back\\\\slash\": 1, \"quo\\\"te\": 2, \"tab\\there\": 3}}";
    assert!(
        serde_json::from_str::<serde_json::Value>(json).is_err(),
        "this test is pointless if slangc's output is already valid JSON"
    );
    let table = hashed_strings(json).expect("the repaired text parses");
    assert_eq!(
        table.get(&3_478_461_427).map(String::as_str),
        Some("Bluetooth — off")
    );
    assert_eq!(table.get(&1).map(String::as_str), Some("back\\slash"));
    assert_eq!(table.get(&2).map(String::as_str), Some("quo\"te"));
    assert_eq!(table.get(&3).map(String::as_str), Some("tab\there"));
}

/// A program that names no strings has no `hashedStrings` key at all, an empty table, not an error.
#[test]
fn no_literals_is_an_empty_table_and_garbage_is_an_error() {
    let table = hashed_strings("{\"bindlessSpaceIndex\": 1}").expect("valid JSON with no key");
    assert!(table.is_empty(), "{table:?}");

    let err = hashed_strings("{\"hashedStrings\": {\"Close\": ").expect_err("truncated JSON");
    assert!(matches!(err, SlangError::Reflection { .. }), "{err}");
    // The message has to say the module was fine and the reader was not.
    assert!(err.to_string().contains("reflection JSON"), "{err}");
}

/// A hand-written fixture covering every kind [`bindings`] classifies, out
/// of order and across three spaces — so the sort and the `space`-absent
/// default are exercised where a real reflection could not be made to vary.
///
/// The shapes are slangc's own, trimmed: a read-only resource carries no
/// `access` key at all, a set-0 parameter carries no `space`, and a
/// structured buffer's element size is the `uniform` entry of its result
/// type's `sizes`.
#[test]
fn the_fixture_classifies_every_kind_and_comes_back_sorted() {
    let json = r#"{"parameters": [
        {"name": "table", "binding": {"kind": "descriptorTableSlot", "space": 2, "index": 1},
         "type": {"kind": "resource", "baseShape": "structuredBuffer",
                  "resultType": {"kind": "struct", "name": "Row",
                                 "sizes": [{"kind": "uniform", "value": 48, "alignment": 4}]}}},
        {"name": "sink", "binding": {"kind": "descriptorTableSlot", "index": 1},
         "type": {"kind": "resource", "baseShape": "structuredBuffer", "access": "readWrite",
                  "resultType": {"kind": "scalar", "scalarType": "uint32",
                                 "sizes": [{"kind": "uniform", "value": 4, "alignment": 4}]}}},
        {"name": "knobs", "binding": {"kind": "descriptorTableSlot", "index": 0},
         "type": {"kind": "constantBuffer"}},
        {"name": "slots", "binding": {"kind": "descriptorTableSlot", "index": 3},
         "type": {"kind": "resource", "baseShape": "texture2D", "array": true}},
        {"name": "one", "binding": {"kind": "descriptorTableSlot", "index": 2},
         "type": {"kind": "resource", "baseShape": "texture2D"}},
        {"name": "out", "binding": {"kind": "descriptorTableSlot", "index": 4},
         "type": {"kind": "resource", "baseShape": "texture2D", "access": "write"}},
        {"name": "ids", "binding": {"kind": "descriptorTableSlot", "index": 5},
         "type": {"kind": "resource", "baseShape": "texture2D", "access": "readWrite"}},
        {"name": "smooth", "binding": {"kind": "descriptorTableSlot", "index": 6},
         "type": {"kind": "samplerState"}},
        {"name": "curves", "binding": {"kind": "descriptorTableSlot", "space": 1, "index": 0},
         "type": {"kind": "resource", "baseShape": "structuredBuffer",
                  "resultType": {"kind": "struct", "name": "Point",
                                 "sizes": [{"kind": "uniform", "value": 16, "alignment": 4}]}}}
    ]}"#;

    let bindings = bindings(json).expect("the fixture classifies");
    let seen: Vec<(u32, u32, &str)> = bindings
        .iter()
        .map(|binding| (binding.set, binding.index, binding.name.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (0, 0, "knobs"),
            (0, 1, "sink"),
            (0, 2, "one"),
            (0, 3, "slots"),
            (0, 4, "out"),
            (0, 5, "ids"),
            (0, 6, "smooth"),
            (1, 0, "curves"),
            (2, 1, "table"),
        ]
    );

    let kinds: Vec<BindingKind> = bindings.iter().map(|binding| binding.kind).collect();
    assert_eq!(
        kinds,
        vec![
            BindingKind::Uniform,
            BindingKind::Storage {
                read_only: false,
                stride: Some(4)
            },
            BindingKind::Texture { array: false },
            BindingKind::Texture { array: true },
            BindingKind::StorageTexture { write_only: true },
            BindingKind::StorageTexture { write_only: false },
            BindingKind::Sampler,
            BindingKind::Storage {
                read_only: true,
                stride: Some(16)
            },
            BindingKind::Storage {
                read_only: true,
                stride: Some(48)
            },
        ]
    );
}

/// A parameter [`bindings`] cannot classify is an error naming it, never a
/// silent skip — a binding dropped from a layout renumbers every binding
/// after it, which is a wrong buffer with nothing to report it.
#[test]
fn a_parameter_it_cannot_classify_is_named_not_skipped() {
    for (json, expected) in [
        (
            r#"{"parameters": [{"name": "cube",
                "binding": {"kind": "descriptorTableSlot", "index": 0},
                "type": {"kind": "resource", "baseShape": "textureCube"}}]}"#,
            "`cube`",
        ),
        (
            r#"{"parameters": [{"name": "rooted",
                "binding": {"kind": "descriptorTableSlot", "index": 0},
                "type": {"kind": "resource", "baseShape": "structuredBuffer",
                         "access": "append"}}]}"#,
            "`rooted`",
        ),
        (
            r#"{"parameters": [{"name": "pushed",
                "binding": {"kind": "pushConstantBuffer", "index": 0},
                "type": {"kind": "constantBuffer"}}]}"#,
            "`pushed`",
        ),
        (
            r#"{"parameters": [{"name": "faceless",
                "binding": {"kind": "descriptorTableSlot", "index": 0}}]}"#,
            "`faceless`",
        ),
    ] {
        let err = bindings(json).expect_err("an unclassifiable parameter is an error");
        assert!(matches!(err, SlangError::Reflection { .. }), "{err}");
        assert!(err.to_string().contains(expected), "{err}");
    }

    // A program with no global parameters at all is an empty list, the same
    // reading `hashed_strings` gives a missing table.
    assert!(
        bindings("{\"bindlessSpaceIndex\": 1}")
            .expect("valid JSON with no parameters")
            .is_empty()
    );
}

/// The text demo, reflected for real: the seven bindings across two spaces a
/// harness builds its layouts from. The demo's own three sit in set 0 and the
/// four curve buffers `text.slang` declares sit in set 1, which is what lets a
/// consumer keep its own bindings in set 0 and add the font beside them.
#[test]
fn the_text_demo_reflects_its_bindings() {
    let source = slang_dir().join("text_demo.slang");
    let json = reflect(&source, "text_demo", &[], &[])
        .unwrap_or_else(|err| panic!("the text demo should reflect:\n{err}"));
    let bindings = bindings(&json).expect("the demo's own reflection classifies");

    let seen: Vec<(u32, u32, &str)> = bindings
        .iter()
        .map(|binding| (binding.set, binding.index, binding.name.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            (0, 0, "coverageOut"),
            (0, 1, "placements"),
            (0, 2, "demo"),
            (1, 0, "textPoints"),
            (1, 1, "textBands"),
            (1, 2, "textBandCurves"),
            (1, 3, "textGlyphs"),
        ]
    );
    assert_eq!(bindings[2].kind, BindingKind::Uniform);
    // The four curve buffers, with the strides `text/src/layout.rs` declares:
    // a point and a control point, a band record, a curve index, a glyph.
    let strides: Vec<BindingKind> = bindings[3..].iter().map(|binding| binding.kind).collect();
    assert_eq!(
        strides,
        vec![
            BindingKind::Storage {
                read_only: true,
                stride: Some(16)
            },
            BindingKind::Storage {
                read_only: true,
                stride: Some(8)
            },
            BindingKind::Storage {
                read_only: true,
                stride: Some(4)
            },
            BindingKind::Storage {
                read_only: true,
                stride: Some(64)
            },
        ]
    );
}
