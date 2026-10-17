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

/// The SPIR-V magic number, little-endian.
fn assert_spirv(module: &[u8]) {
    assert!(module.len() > 20, "suspiciously short module");
    assert_eq!(
        &module[..4],
        &[0x03, 0x02, 0x23, 0x07],
        "output does not start with the SPIR-V magic number"
    );
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

/// The identity a cache keys on names all three things that change what a
/// compile produces.
#[test]
fn the_version_names_the_crate_the_compiler_and_the_modules() {
    let version = version();
    assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
    assert!(version.contains("Slang 20"), "{version}");
    assert!(
        version.contains(&format!("{:016x}", modules::hash())),
        "{version}"
    );
}

// ---------------------------------------------------------------------------
// Compiling
// ---------------------------------------------------------------------------

/// The two demos compile with nothing to report: the modules are found
/// through the search path, the profile and the capability are known, and
/// the SPIR-V optimiser is in reach (without it Slang still succeeds, with
/// an error on the success path).
#[test]
fn the_demos_compile_with_nothing_to_report() {
    for (file, entry) in [
        ("text_demo.slang", "text_demo"),
        ("shape_demo.slang", "shape_batch"),
    ] {
        let program = compile_program(&modules_dir().join(file), &[entry], &Options::default())
            .unwrap_or_else(|err| panic!("{file} should compile:\n{err}"));
        assert_eq!(program.entries.len(), 1);
        assert_eq!(program.entries[0].name, entry);
        assert_spirv(&program.entries[0].spirv);
        assert!(program.warnings.is_empty(), "{file}: {}", program.warnings);
    }
}

/// A shader that sits nowhere near the modules still imports them.
#[test]
fn a_shader_elsewhere_imports_the_engine() {
    let dir = scratch("elsewhere");
    let path = dir.join("far.slang");
    std::fs::write(
        &path,
        "import text;\n\
         RWStructuredBuffer<float> sink;\n\
         [shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void far(uint3 gid: SV_DispatchThreadID)\n\
         {\n\
         \x20   sink[gid.x] = textCoverage(0, textSampleAt(float2(0, 0), float2(0, 0), 16.0), 16.0);\n\
         }\n",
    )
    .expect("temp dir is writable");
    let spirv = compile(&path, "far", &Options::default())
        .unwrap_or_else(|err| panic!("the shader should compile:\n{err}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_spirv(&spirv);
}

/// One link of every entry point gives each the code it gets alone, so a
/// host can compile its program in one call and the harnesses one entry at a
/// time, and the two never disagree.
#[test]
fn a_program_is_its_entries_compiled_together() {
    let source = modules_dir().join("shape_demo.slang");
    let options = Options::default();
    let program = compile_program(&source, &["shape_batch", "clock_probe"], &options)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(program.entries.len(), 2);
    for entry in &program.entries {
        let alone = compile(&source, &entry.name, &options).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(
            alone, entry.spirv,
            "{} differs when linked alone",
            entry.name
        );
    }
    assert!(program.warnings.is_empty(), "{}", program.warnings);
}

#[test]
fn syntax_errors_surface_the_diagnostic() {
    let dir = scratch("broken");
    let path = dir.join("broken.slang");
    std::fs::write(
        &path,
        "[shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void broken(uint3 gid: SV_DispatchThreadID)\n\
         {\n    this is not slang;\n}\n",
    )
    .expect("temp dir is writable");

    let err =
        compile(&path, "broken", &Options::default()).expect_err("garbage should not compile");
    let _ = std::fs::remove_dir_all(&dir);

    let rendered = err.to_string();
    assert!(
        matches!(err, SlangError::Compile { .. }),
        "expected a compile failure, got: {rendered}"
    );
    // The diagnostic must survive intact: file, line, and Slang's own
    // wording are what a caller shows the person editing the file.
    let diagnostics = err.diagnostics().expect("compile errors carry diagnostics");
    assert!(
        diagnostics.contains("error") && diagnostics.contains("broken.slang:5:"),
        "diagnostics lost Slang's text: {diagnostics:?}"
    );
    assert!(
        rendered.contains(diagnostics),
        "Display dropped the diagnostics: {rendered}"
    );
    assert!(
        rendered.contains("broken"),
        "Display should name the entry point: {rendered}"
    );
}

/// An entry point the module does not have is a compile error naming it,
/// not a panic, and a missing file is one naming the file.
#[test]
fn a_missing_entry_point_or_file_is_a_compile_error() {
    let err = compile(
        &modules_dir().join("text_demo.slang"),
        "no_such_entry",
        &Options::default(),
    )
    .expect_err("there is no such entry point");
    assert!(
        matches!(&err, SlangError::Compile { entry, .. } if entry == "no_such_entry"),
        "{err}"
    );

    let err = compile(
        &modules_dir().join("no_such_file.slang"),
        "text_demo",
        &Options::default(),
    )
    .expect_err("there is no such file");
    assert!(matches!(err, SlangError::Compile { .. }), "{err}");
    assert!(err.to_string().contains("no_such_file"), "{err}");
}

/// A define reaches the preprocessor, in both spellings.
#[test]
fn defines_reach_the_shader() {
    let dir = scratch("defines");
    let path = dir.join("defined.slang");
    std::fs::write(
        &path,
        "RWStructuredBuffer<uint> sink;\n\
         [shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void defined(uint3 gid: SV_DispatchThreadID)\n\
         {\n\
         #ifndef FLAG\n\
         #error FLAG is not defined\n\
         #endif\n\
         \x20   sink[gid.x] = VALUE;\n\
         }\n",
    )
    .expect("temp dir is writable");
    let err = compile(&path, "defined", &Options::default()).expect_err("FLAG is not defined");
    assert!(err.to_string().contains("FLAG is not defined"), "{err}");
    let spirv = compile(
        &path,
        "defined",
        &Options {
            defines: &["FLAG", "VALUE=7"],
            ..Options::default()
        },
    )
    .unwrap_or_else(|err| panic!("{err}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_spirv(&spirv);
}

#[test]
fn temp_output_is_cleaned_up() {
    let path = {
        let temp = TempPath::new("spv");
        std::fs::write(temp.path(), b"scratch").expect("temp dir is writable");
        temp.path().to_path_buf()
    };
    assert!(!path.exists(), "TempPath should remove {}", path.display());
}

// ---------------------------------------------------------------------------
// Reflection
// ---------------------------------------------------------------------------

/// The three literals of a scratch shader, recovered from the same link that
/// produced the module. One literal is not ASCII, and the keys are the
/// hashes the shader itself computes.
#[test]
fn the_hashed_strings_table_rides_on_the_compile() {
    let dir = scratch("strings");
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

    let program = compile_program(&path, &["named"], &Options::default())
        .unwrap_or_else(|err| panic!("the scratch module should compile:\n{err}"));
    let _ = std::fs::remove_dir_all(&dir);

    assert_spirv(&program.entries[0].spirv);
    let table = &program.reflection.strings;
    assert_eq!(table.len(), 3, "{table:?}");
    for literal in ["New window", "Close", "Open — now"] {
        let hash = slang::reflection::compute_string_hash(literal);
        assert_eq!(
            table.get(&hash).map(String::as_str),
            Some(literal),
            "{table:?}"
        );
    }
    // A program that names no strings has an empty table, not an error.
    let silent = reflect(
        &modules_dir().join("text_demo.slang"),
        "text_demo",
        &Options::default(),
    )
    .unwrap_or_else(|err| panic!("{err}"));
    assert!(silent.strings.is_empty(), "{:?}", silent.strings);
}

/// A scratch shader declaring every kind [`bindings`] classifies, out of
/// order and across three spaces, so the sort and the default set are
/// exercised where the demos could not be made to vary.
#[test]
fn every_kind_of_binding_classifies_and_comes_back_sorted() {
    let dir = scratch("bindings");
    let path = dir.join("kinds.slang");
    std::fs::write(
        &path,
        "struct Row { uint a; uint b[11]; }\n\
         struct Point { float2 p; float2 c; }\n\
         struct Knobs { uint width; uint height; }\n\
         [[vk::binding(1, 2)]] StructuredBuffer<Row> table;\n\
         [[vk::binding(1, 0)]] RWStructuredBuffer<uint> sink;\n\
         [[vk::binding(0, 0)]] ConstantBuffer<Knobs> knobs;\n\
         [[vk::binding(3, 0)]] Texture2DArray<float4> slots;\n\
         [[vk::binding(2, 0)]] Texture2D<float4> one;\n\
         [[vk::binding(4, 0)]] WTexture2D<float4> written;\n\
         [[vk::binding(5, 0)]] RWTexture2D<float4> ids;\n\
         [[vk::binding(6, 0)]] SamplerState smooth;\n\
         [[vk::binding(0, 1)]] StructuredBuffer<Point> curves;\n\
         [shader(\"compute\")]\n\
         [numthreads(1, 1, 1)]\n\
         void kinds(uint3 gid: SV_DispatchThreadID)\n\
         {\n\
         \x20   float4 texel = one.SampleLevel(smooth, float2(0, 0), 0)\n\
         \x20       + slots.SampleLevel(smooth, float3(0, 0, 0), 0) + ids[gid.xy];\n\
         \x20   written.Store(gid.xy, texel);\n\
         \x20   sink[gid.x] = table[gid.x].a + knobs.width + uint(curves[gid.x].p.x);\n\
         }\n",
    )
    .expect("temp dir is writable");
    let program = compile_program(&path, &["kinds"], &Options::default())
        .unwrap_or_else(|err| panic!("the scratch module should compile:\n{err}"));
    let _ = std::fs::remove_dir_all(&dir);

    let bindings = &program.reflection.bindings;
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
            (0, 4, "written"),
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

    // The structs, wherever they sit: a buffer's element, a constant
    // buffer's contents.
    let structs = &program.reflection.structs;
    assert_eq!(structs["Row"]["a"], 0);
    assert_eq!(structs["Row"]["b"], 4);
    assert_eq!(structs["Point"]["c"], 8);
    assert_eq!(structs["Knobs"]["height"], 4);

    // And the whole reflection survives a trip through its JSON.
    let json = program.reflection.to_json();
    assert_eq!(Reflection::from_json(&json).unwrap(), program.reflection);
    assert!(
        matches!(
            Reflection::from_json("{\"bindings\": 3}"),
            Err(SlangError::Reflection { .. })
        ),
        "garbage is an error, not a panic"
    );
}

/// A parameter [`bindings`] cannot classify is an error naming it, never a
/// silent skip — a binding dropped from a layout renumbers every binding
/// after it, which is a wrong buffer with nothing to report it.
#[test]
fn a_parameter_it_cannot_classify_is_named_not_skipped() {
    let dir = scratch("unclassified");
    for (declaration, expected) in [
        ("TextureCube<float4> cube;", "`cube`"),
        ("AppendStructuredBuffer<uint> rooted;", "`rooted`"),
        (
            "[[vk::push_constant]] ConstantBuffer<Knobs> pushed;",
            "`pushed`",
        ),
    ] {
        let path = dir.join("one.slang");
        std::fs::write(
            &path,
            format!(
                "struct Knobs {{ uint width; }}\n\
                 {declaration}\n\
                 RWStructuredBuffer<uint> sink;\n\
                 [shader(\"compute\")]\n\
                 [numthreads(1, 1, 1)]\n\
                 void one(uint3 gid: SV_DispatchThreadID) {{ sink[gid.x] = 1; }}\n"
            ),
        )
        .expect("temp dir is writable");
        let err = reflect(&path, "one", &Options::default())
            .expect_err("an unclassifiable parameter is an error");
        assert!(matches!(err, SlangError::Reflection { .. }), "{err}");
        assert!(err.to_string().contains(expected), "{err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The text demo, reflected for real: the seven bindings across two spaces a
/// harness builds its layouts from. The demo's own three sit in set 0 and the
/// four curve buffers `text.slang` declares sit in set 1, which is what lets a
/// consumer keep its own bindings in set 0 and add the font beside them.
#[test]
fn the_text_demo_reflects_its_bindings() {
    let reflection = reflect(
        &modules_dir().join("text_demo.slang"),
        "text_demo",
        &Options::default(),
    )
    .unwrap_or_else(|err| panic!("the text demo should reflect:\n{err}"));
    let bindings = &reflection.bindings;

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
