//! Slang → SPIR-V compilation: the one slangc command line in the workspace.
//!
//! A build script compiles a shader once at build time (`text/build.rs` bakes
//! the text demo this way), a layout test reflects one, and a consumer that
//! recompiles its shader at run time calls the same functions. Every failure
//! path ends in a [`SlangError`] carrying slangc's diagnostics *verbatim*:
//! that text is the entire feedback loop for whoever is editing the shader, so
//! a caller that keeps its old module when a recompile fails can print it
//! straight through.
//!
//! One copy of the invocation means the build, the tests and a run-time
//! compile cannot disagree about flags.
//!
//! # The strings a shader owns
//!
//! Every compile also carries slangc's
//! `hashedStrings` table: the literal behind every `getStringHash("…")` in the
//! program, keyed by the hash the shader itself will hand back to the CPU.
//! That is what lets a shader name a string and the CPU find its text without
//! either side holding a list of strings. The table rides on
//! [`Compiled`] out of the one slangc run that produced the SPIR-V —
//! `-reflection-json` costs nothing measurable next to a compile (within
//! timing noise; parsing the JSON is about 2 ms an entry point) and a second
//! run would be a second chance to disagree with the first.

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The slangc binary to run: `$SLANGC` if set, else `slangc` from `PATH`.
///
/// Read fresh on every compile rather than cached, so pointing `$SLANGC` at a
/// different toolchain takes effect at the next compile.
pub fn slangc_program() -> OsString {
    std::env::var_os("SLANGC").unwrap_or_else(|| OsString::from("slangc"))
}

/// Compile one entry point of a Slang module to a SPIR-V binary.
///
/// `defines` are bare `NAME` or `NAME=VALUE` strings (the `-D` is added here);
/// `extra_args` are passed through untouched, for per-variant flags like
/// `-default-image-format-unknown`.
///
/// The source's own directory goes on the module search path, which is what
/// makes `import text;` resolve for a shader that sits beside the module.

pub fn compile(
    source: &Path,
    entry: &str,
    defines: &[&str],
    extra_args: &[&str],
) -> Result<Vec<u8>, SlangError> {
    let includes: Vec<&Path> = source.parent().into_iter().collect();
    compile_with(
        &slangc_program(),
        source,
        entry,
        defines,
        extra_args,
        &includes,
    )
}

/// slangc's own reflection for a shader, as JSON: every struct reachable
/// from `entry`, with a byte offset per field.
///
/// The layout tests (punchcutter-shape, punchcutter-text) are the callers — they compare these
/// offsets against `offset_of!` so the Rust and Slang halves of an ABI cannot
/// drift apart silently. Flags match [`compile`]
/// exactly, `-reflection-json` aside, so what is reflected is what the engine
/// runs. [`struct_offsets`] turns the JSON into a table.
pub fn reflect(
    source: &Path,
    entry: &str,
    defines: &[&str],
    extra_args: &[&str],
) -> Result<String, SlangError> {
    let includes: Vec<&Path> = source.parent().into_iter().collect();
    reflect_with(
        &slangc_program(),
        source,
        source,
        entry,
        defines,
        extra_args,
        &includes,
    )
}

/// The reflection run itself. `blame` is the file a diagnostic names, which is
/// the file a diagnostic should name, when it is not the one compiled.
fn reflect_with(
    program: &OsStr,
    source: &Path,
    blame: &Path,
    entry: &str,
    defines: &[&str],
    extra_args: &[&str],
    includes: &[&Path],
) -> Result<String, SlangError> {
    let json = TempPath::new(blame, entry, "reflection.json");
    let output = TempPath::new(blame, entry, "spv");

    let mut command = Command::new(program);
    command
        .arg(source)
        .args(["-target", "spirv"])
        .args(["-profile", "glsl_450"])
        // The shaper reads the device's wall clock, and slangc would otherwise
        // upgrade the profile with a warning, which a zero-warnings build
        // reads as a failure. Naming it here covers every compile path.
        .args(["-capability", "spvShaderClockKHR"])
        .args(["-entry", entry])
        .args(["-stage", "compute"])
        .arg("-fvk-use-entrypoint-name");
    for include in includes {
        command.arg("-I").arg(include);
    }
    command.args(extra_args);
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    command.arg("-reflection-json").arg(json.path());
    command.arg("-o").arg(output.path());

    let result = command.output().map_err(|err| SlangError::Spawn {
        program: program.to_string_lossy().into_owned(),
        source: err,
    })?;
    if !result.status.success() {
        return Err(SlangError::Compile {
            source_path: blame.to_path_buf(),
            entry: entry.to_string(),
            diagnostics: merge_streams(&result.stdout, &result.stderr),
        });
    }

    std::fs::read_to_string(json.path()).map_err(|err| SlangError::Output {
        path: json.path().to_path_buf(),
        source: err,
    })
}

/// Every `getStringHash("…")` literal in the program, keyed by its hash.
///
/// slangc records the table under `"hashedStrings"`, as `literal -> hash`, and
/// the hash is a *signed* 32-bit integer in the JSON — `"Close": -1629961352`.
/// The shader sees the same number as a `uint`, so the low 32 bits are the
/// key whichever sign JSON gave them.
///
/// A program with no literals has no `hashedStrings` key at all (not an empty
/// object), which reads as an empty table. The empty literal hashes to 0, so 0
/// means "no string" in practice.
///
/// Errors rather than panics: a caller that parses this on a run-time
/// recompile has to be able to fail that recompile and keep the running shader
/// and its old strings, never swap in half a pair.
pub fn hashed_strings(reflection_json: &str) -> Result<BTreeMap<u32, String>, SlangError> {
    let value = reflection_value(reflection_json)?;
    let mut out = BTreeMap::new();
    let Some(table) = value.get("hashedStrings") else {
        return Ok(out);
    };
    let table = table.as_object().ok_or_else(|| SlangError::Reflection {
        detail: "hashedStrings is not an object".into(),
    })?;
    for (literal, hash) in table {
        let hash = hash.as_i64().ok_or_else(|| SlangError::Reflection {
            detail: format!("the hash of {literal:?} is not an integer: {hash}"),
        })?;
        if let Some(other) = out.insert(hash as u32, literal.clone())
            && &other != literal
        {
            // Two literals on one hash would make a lookup ambiguous, and
            // silence here would surface as the wrong text on screen.
            tracing::warn!(
                "slangc: {literal:?} and {other:?} share the hash {}",
                hash as u32
            );
        }
    }
    Ok(out)
}

/// One global parameter of a compiled program: where its descriptor lands,
/// and what kind of descriptor it has to be.
///
/// The point of reading these back is that a program declares them *once*, in
/// the shader, as `[[vk::binding(index, space)]]`. The alternative — a second,
/// hand-written list of wgpu layout entries on the CPU, kept in agreement by
/// tests — is exactly the kind of duplication reflection exists to end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    /// The parameter's name in the Slang source, which is the only handle a
    /// caller has on it. Two facts a descriptor needs are *not* in the
    /// reflection — the texel format of a storage image the program declares
    /// format-unknown, and which buffer to bind — and both are looked up by
    /// this.
    pub name: String,
    /// The descriptor set (`space` in Slang, absent for set 0).
    pub set: u32,
    /// The binding number within the set.
    pub index: u32,
    pub kind: BindingKind,
}

/// What kind of descriptor a [`Binding`] needs, in the terms a layout entry is
/// written in.
///
/// Deliberately not a wgpu type: `punchcutter-core`'s `gpu` feature is optional and a
/// build script must be able to depend on this crate without dragging the
/// graphics stack into the build graph (the note on `wgpu` in `Cargo.toml`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingKind {
    /// `ConstantBuffer<T>`.
    Uniform,
    /// `StructuredBuffer<T>` (read-only) or `RWStructuredBuffer<T>`.
    Storage {
        read_only: bool,
        /// One element's size in bytes, when slangc laid the element type out
        /// — the floor a binding of this buffer has to clear.
        stride: Option<u32>,
    },
    /// `RWTexture2D<T>` — written, and read back too when `write_only` is
    /// false. The *texel format* is not here: SPIR-V bakes it into the module
    /// and a format-unknown image (one compiled with
    /// `-default-image-format-unknown`) reflects none at all, so the caller
    /// supplies it by name.
    StorageTexture { write_only: bool },
    /// `Texture2D<T>` or `Texture2DArray<T>`, sampled.
    Texture { array: bool },
    /// `SamplerState`.
    Sampler,
}

/// Every global parameter of a reflected program, sorted by `(set, index)`.
///
/// The caller builds its descriptor-set layouts from this instead of writing
/// them out a second time. Sorted because a layout's entries are consumed in
/// binding order — wgpu-hal numbers a Vulkan binding by an entry's *position*
/// in the list — and slangc emits the parameters in declaration order, which
/// need not be set order.
///
/// A parameter this cannot classify is an [`SlangError::Reflection`] naming
/// it, never a silent skip: a binding dropped from a layout renumbers every
/// binding after it, which is a wrong buffer with no error anywhere.
pub fn bindings(reflection_json: &str) -> Result<Vec<Binding>, SlangError> {
    let value = reflection_value(reflection_json)?;
    // A program with no global parameters has no `parameters` key at all,
    // which reads as an empty list — the same reading `hashed_strings` gives a
    // missing `hashedStrings`.
    let Some(parameters) = value.get("parameters") else {
        return Ok(Vec::new());
    };
    let parameters = parameters
        .as_array()
        .ok_or_else(|| SlangError::Reflection {
            detail: "parameters is not an array".into(),
        })?;

    let mut out = Vec::with_capacity(parameters.len());
    for parameter in parameters {
        out.push(one_binding(parameter)?);
    }
    out.sort_by_key(|binding| (binding.set, binding.index));
    Ok(out)
}

/// One entry of the `parameters` array.
fn one_binding(parameter: &serde_json::Value) -> Result<Binding, SlangError> {
    use serde_json::Value;

    let name = parameter
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string();
    let blame = |detail: String| SlangError::Reflection {
        detail: format!("the parameter `{name}` {detail}"),
    };

    let slot = parameter
        .get("binding")
        .ok_or_else(|| blame("has no binding".into()))?;
    match slot.get("kind").and_then(Value::as_str) {
        Some("descriptorTableSlot") => {}
        other => {
            return Err(blame(format!(
                "binds as {other:?} rather than a descriptor table slot"
            )));
        }
    }
    let index = slot
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| blame("has no binding index".into()))? as u32;
    // `space` is omitted for set 0, which is most of them.
    let set = slot.get("space").and_then(Value::as_u64).unwrap_or(0) as u32;

    let ty = parameter
        .get("type")
        .ok_or_else(|| blame("has no type".into()))?;
    let access = ty.get("access").and_then(Value::as_str);
    let kind = match ty.get("kind").and_then(Value::as_str) {
        Some("constantBuffer") => BindingKind::Uniform,
        Some("samplerState") => BindingKind::Sampler,
        Some("resource") => match ty.get("baseShape").and_then(Value::as_str) {
            Some("structuredBuffer") => BindingKind::Storage {
                // slangc omits `access` entirely for a read-only resource.
                read_only: match access {
                    None | Some("read") => true,
                    Some("readWrite") => false,
                    Some(other) => {
                        return Err(blame(format!(
                            "is a structured buffer with access {other:?}, which has no \
                             descriptor here"
                        )));
                    }
                },
                stride: element_stride(ty),
            },
            Some("texture2D") => match access {
                None | Some("read") => BindingKind::Texture {
                    array: ty.get("array").and_then(Value::as_bool).unwrap_or(false),
                },
                Some("write") => BindingKind::StorageTexture { write_only: true },
                Some("readWrite") => BindingKind::StorageTexture { write_only: false },
                Some(other) => {
                    return Err(blame(format!(
                        "is a 2D texture with access {other:?}, which has no descriptor here"
                    )));
                }
            },
            other => {
                return Err(blame(format!(
                    "is a resource of shape {other:?}, which has no descriptor here"
                )));
            }
        },
        other => {
            return Err(blame(format!(
                "has type kind {other:?}, which has no descriptor here"
            )));
        }
    };

    Ok(Binding {
        name,
        set,
        index,
        kind,
    })
}

/// One element of a structured buffer, in bytes, from the laid-out size of its
/// result type. `None` when slangc reflected no uniform layout for it: an
/// opaque element type.
fn element_stride(ty: &serde_json::Value) -> Option<u32> {
    let sizes = ty.get("resultType")?.get("sizes")?.as_array()?;
    sizes
        .iter()
        .find(|size| size.get("kind").and_then(serde_json::Value::as_str) == Some("uniform"))
        .and_then(|size| size.get("value"))
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as u32)
}

/// Every struct slangc reflected, as `name -> {field -> byte offset}`.
///
/// The reflection JSON nests a struct wherever it is used (inside a buffer's
/// result type, inside an array's element type), and a field's offset is
/// relative to its own struct — which is what makes a plain recursive walk
/// enough. Structs with no laid-out fields (an interface's witness table, say)
/// are skipped rather than recorded empty.
pub fn struct_offsets(json: &str) -> BTreeMap<String, BTreeMap<String, usize>> {
    use serde_json::Value;

    fn walk(value: &Value, out: &mut BTreeMap<String, BTreeMap<String, usize>>) {
        match value {
            Value::Object(map) => {
                if map.get("kind").and_then(Value::as_str) == Some("struct")
                    && let (Some(name), Some(Value::Array(fields))) =
                        (map.get("name").and_then(Value::as_str), map.get("fields"))
                {
                    let offsets: BTreeMap<String, usize> = fields
                        .iter()
                        .filter_map(|field| {
                            let name = field.get("name")?.as_str()?.to_string();
                            let offset = field.get("binding")?.get("offset")?.as_u64()?;
                            Some((name, offset as usize))
                        })
                        .collect();
                    if !offsets.is_empty() {
                        out.insert(name.to_string(), offsets);
                    }
                }
                for nested in map.values() {
                    walk(nested, out);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    walk(nested, out);
                }
            }
            _ => {}
        }
    }

    let value = reflection_value(json).expect("slangc emits reflection JSON this reader can parse");
    let mut out = BTreeMap::new();
    walk(&value, &mut out);
    out
}

/// slangc's reflection output as a parsed value, octal escapes and all.
///
/// The output is *almost* JSON. A non-ASCII character in a hashed string comes
/// back with each of its UTF-8 bytes as a three-digit octal escape —
/// `"Open \342\200\224 now"` — and `\342` is not one of JSON's escapes, so
/// a parser handed the text verbatim stops at the first accented label. Every
/// other escape slangc writes is JSON's own (`\\`, `\"`, `\t`, `\n`), so
/// decoding those triples back into the bytes they stand for is the whole
/// repair: raw UTF-8 is legal inside a JSON string. Verified against slangc
/// 2026.14.1, and worth knowing: a future slangc that emits `\uXXXX` instead
/// would make this a no-op rather than a bug.
fn reflection_value(json: &str) -> Result<serde_json::Value, SlangError> {
    serde_json::from_slice(&octal_escapes_decoded(json)).map_err(|err| SlangError::Reflection {
        detail: err.to_string(),
    })
}

/// `\342` → the byte `0o342`, leaving every other escape pair untouched.
///
/// Escapes are consumed whole, which is what keeps a literal backslash from
/// being misread: slangc writes one as `\\`, so `\\342` decodes to a backslash
/// followed by the three characters `342`. Only values in `0x80..=0xff` are
/// decoded — those are the ones slangc escapes this way, and a raw control byte
/// would turn legal text into illegal JSON.
fn octal_escapes_decoded(json: &str) -> Vec<u8> {
    let bytes = json.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
                if (0x80..=0xff).contains(&value) {
                    out.push(value as u8);
                    i += 4;
                    continue;
                }
            }
        }
        // Any other escape goes through as the pair it is, so the parser sees
        // slangc's `\\`, `\"`, `\t` and `\n` exactly as JSON's own.
        out.extend_from_slice(&bytes[i..(i + 2).min(bytes.len())]);
        i += 2;
    }
    out
}

/// [`compile`], with the compiler binary and the module search path named
/// explicitly.
///
/// The explicit binary is what lets the tests exercise the spawn-failure path
/// without mutating the process environment (`set_var` is unsafe and racy
/// under a threaded test harness). The explicit includes are for a source that
/// does not sit next to what it imports.
pub fn compile_with(
    program: &OsStr,
    source: &Path,
    entry: &str,
    defines: &[&str],
    extra_args: &[&str],
    includes: &[&Path],
) -> Result<Vec<u8>, SlangError> {
    let compiled = compile_capturing(program, source, entry, defines, extra_args, includes)?;
    if !compiled.warnings.is_empty() {
        // Warnings still come back on a successful exit. Surfacing them keeps
        // a slow rot (deprecated intrinsics, implicit conversions) visible.
        tracing::warn!(entry, "slangc: {}", compiled.warnings);
    }
    Ok(compiled.spirv)
}

/// A module slangc accepted, and whatever it said on the way.
pub struct Compiled {
    pub spirv: Vec<u8>,
    /// slangc's diagnostics on a *successful* exit — warnings, trimmed, empty
    /// when it was quiet.
    pub warnings: String,
    /// Every `getStringHash("…")` literal in this entry point's reachable code,
    /// by hash ([`hashed_strings`]). Empty for a shader that names no strings.
    /// Reflection only reports what the entry point reaches, so a caller
    /// wanting a whole program's strings merges the tables from all of its
    /// entry points.
    pub strings: BTreeMap<u32, String>,
    /// slangc's reflection of the program, verbatim: the same JSON `strings`
    /// was read from. A build script bakes it beside the SPIR-V so a process
    /// starting on baked modules knows its bindings and its literals without
    /// running slangc.
    pub reflection: String,
}

/// [`compile_with`], handing the caller slangc's success-path warnings rather
/// than logging them.
///
/// Two callers care: a build script, which turns them into `cargo:warning=`
/// lines (nothing subscribes to `tracing` in a build script), and a test whose
/// gate is zero warnings.
pub fn compile_capturing(
    program: &OsStr,
    source: &Path,
    entry: &str,
    defines: &[&str],
    extra_args: &[&str],
    includes: &[&Path],
) -> Result<Compiled, SlangError> {
    // slangc writes SPIR-V to a file, not to stdout, so a scratch file it is.
    let output = TempPath::new(source, entry, "spv");
    // And the reflection beside it, from the same run: the strings table is
    // part of what a compile produces (this file's header).
    let reflection = TempPath::new(source, entry, "reflection.json");

    let mut command = Command::new(program);
    command
        .arg(source)
        .args(["-target", "spirv"])
        .args(["-profile", "glsl_450"])
        // The shaper reads the device's wall clock, and slangc would otherwise
        // upgrade the profile with a warning, which a zero-warnings build
        // reads as a failure. Naming it here covers every compile path.
        .args(["-capability", "spvShaderClockKHR"])
        .args(["-entry", entry])
        .args(["-stage", "compute"])
        .arg("-fvk-use-entrypoint-name");
    // Module search path: the engine's `slang/` and the source's own.
    for include in includes {
        command.arg("-I").arg(include);
    }
    command.args(extra_args);
    for define in defines {
        command.arg(format!("-D{define}"));
    }
    command.arg("-reflection-json").arg(reflection.path());
    command.arg("-o").arg(output.path());

    let result = command.output().map_err(|err| SlangError::Spawn {
        program: program.to_string_lossy().into_owned(),
        source: err,
    })?;

    // slangc splits diagnostics across both streams depending on the phase;
    // the caller wants all of it, in one piece, unedited.
    let diagnostics = merge_streams(&result.stdout, &result.stderr);

    if !result.status.success() {
        return Err(SlangError::Compile {
            source_path: source.to_path_buf(),
            entry: entry.to_string(),
            diagnostics,
        });
    }

    let spirv = std::fs::read(output.path()).map_err(|err| SlangError::Output {
        path: output.path().to_path_buf(),
        source: err,
    })?;

    validate_spirv(output.path()).map_err(|diagnostics| SlangError::Validation {
        source_path: source.to_path_buf(),
        entry: entry.to_string(),
        diagnostics,
    })?;

    let json = std::fs::read_to_string(reflection.path()).map_err(|err| SlangError::Output {
        path: reflection.path().to_path_buf(),
        source: err,
    })?;

    Ok(Compiled {
        spirv,
        warnings: diagnostics,
        strings: hashed_strings(&json)?,
        reflection: json,
    })
}

/// Run `spirv-val` over a compiled module when SPIRV-Tools is installed.
///
/// Optional by design: a machine with only slangc must still build the
/// workspace and still compile shaders, so an absent validator is a `debug!` line and an
/// `Ok`. Where the validator does exist, a malformed module is far nicer to
/// read here than as a device-lost three layers down. `Err` carries
/// spirv-val's own text.
pub fn validate_spirv(module: &Path) -> Result<(), String> {
    let result = match Command::new("spirv-val").arg(module).output() {
        Ok(result) => result,
        Err(err) => {
            tracing::debug!(
                module = %module.display(),
                "spirv-val unavailable ({err}); skipping SPIR-V validation"
            );
            return Ok(());
        }
    };
    if result.status.success() {
        return Ok(());
    }
    Err(merge_streams(&result.stdout, &result.stderr))
}

/// Both output streams as one trimmed block, in the order a terminal would
/// have interleaved them closely enough: stderr carries the diagnostics,
/// stdout the occasional note.
fn merge_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stderr).into_owned();
    let out = String::from_utf8_lossy(stdout);
    if !out.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&out);
    }
    text.trim_end().to_string()
}

/// A scratch file in the system temp directory, removed when it drops.
///
/// Unique per process, per call: several shaders may be compiled at once,
/// and a stale file from a crashed run must never be mistaken for
/// fresh output (every caller writes before reading, and the unique name makes it moot).
struct TempPath(PathBuf);

impl TempPath {
    /// `extension` is the whole tail after the dot, so a generated wrapper can
    /// be `...-wrapper.slang` and stay recognisable in a stray-file listing.
    fn new(source: &Path, entry: &str, extension: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let stem = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shader".to_string());
        let name = format!(
            "punchcutter-{}-{stem}-{entry}-{seq}.{extension}",
            std::process::id()
        );
        Self(std::env::temp_dir().join(name))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        // Best effort: a leftover temp file is not worth a log line, and the
        // interesting error (the compile failure) is already on its way up.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Everything that can go wrong between a saved `.slang` file and a SPIR-V
/// module the GPU will accept.
///
/// The `diagnostics` payloads are slangc's (or spirv-val's) own output,
/// unmodified apart from trailing whitespace — a caller prints them straight
/// through, and column-accurate error text is the point.
#[derive(Debug)]
pub enum SlangError {
    /// The compiler binary could not be run at all.
    Spawn { program: String, source: io::Error },
    /// slangc exited non-zero.
    Compile {
        source_path: PathBuf,
        entry: String,
        diagnostics: String,
    },
    /// slangc claimed success but its output could not be read back.
    Output { path: PathBuf, source: io::Error },
    /// `spirv-val` rejected the module slangc produced.
    Validation {
        source_path: PathBuf,
        entry: String,
        diagnostics: String,
    },
    /// slangc's reflection JSON could not be understood. Not a fact about the
    /// shader — slangc exited zero — but about this reader or slangc's
    /// format, and the compile it fails is the right place to find out.
    Reflection { detail: String },
}

impl SlangError {
    /// The tool's own diagnostic text, when there is one. Handy for a
    /// log that wants to print the message body without the wrapper prose.
    pub fn diagnostics(&self) -> Option<&str> {
        match self {
            SlangError::Compile { diagnostics, .. }
            | SlangError::Validation { diagnostics, .. } => Some(diagnostics),
            SlangError::Spawn { .. }
            | SlangError::Output { .. }
            | SlangError::Reflection { .. } => None,
        }
    }
}

impl fmt::Display for SlangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlangError::Spawn { program, source } => write!(
                f,
                "could not run `{program}`: {source}\n\
                 the shaders compile with the Slang toolchain; install \
                 slangc (or point $SLANGC at it)."
            ),
            SlangError::Compile {
                source_path,
                entry,
                diagnostics,
            } => write!(
                f,
                "slangc failed for {} (entry `{entry}`):\n{diagnostics}",
                source_path.display()
            ),
            SlangError::Output { path, source } => write!(
                f,
                "slangc reported success but {} could not be read: {source}",
                path.display()
            ),
            SlangError::Validation {
                source_path,
                entry,
                diagnostics,
            } => write!(
                f,
                "spirv-val rejected the module from {} (entry `{entry}`):\n{diagnostics}",
                source_path.display()
            ),
            SlangError::Reflection { detail } => write!(
                f,
                "slangc's reflection JSON could not be parsed: {detail}\n\
                 the module itself compiled; this is the reader (or slangc's \
                 format) and not the shader."
            ),
        }
    }
}

impl Error for SlangError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SlangError::Spawn { source, .. } | SlangError::Output { source, .. } => Some(source),
            SlangError::Compile { .. }
            | SlangError::Validation { .. }
            | SlangError::Reflection { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests;
