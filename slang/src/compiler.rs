//! The compiler half of the crate: Slang in process, behind the `compiler`
//! feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use shader_slang as slang;
use slang::reflection::{Shader, TypeLayout};

use crate::{Binding, BindingKind, Entry, Options, Program, Reflection, SlangError, modules};

/// Compile one entry point of a Slang module to a SPIR-V binary.
///
/// What Slang said on the way to success (warnings) goes to `tracing` at
/// warn level; a caller that wants the text itself, or the reflection, calls
/// [`compile_program`].
pub fn compile(source: &Path, entry: &str, options: &Options) -> Result<Vec<u8>, SlangError> {
    let mut program = compile_program(source, &[entry], options)?;
    if !program.warnings.is_empty() {
        // Warnings still come back on a successful compile. Surfacing them
        // keeps a slow rot (deprecated intrinsics, implicit conversions)
        // visible.
        tracing::warn!(entry, "slang: {}", program.warnings);
    }
    Ok(program.entries.remove(0).spirv)
}

/// Compile every entry point named, from one link of the module, with the
/// reflection of the whole program.
///
/// One link rather than one per entry point: the entry points share the
/// module's parameters and its strings, and compiling a shader's entry
/// points together is what makes a host's reload one call. The SPIR-V for
/// each is its own module, in the order asked for.
pub fn compile_program(
    source: &Path,
    entries: &[&str],
    options: &Options,
) -> Result<Program, SlangError> {
    build(source, entries, options, true)
}

/// The reflection of `entry`'s program, without generating code: what the
/// layout tests compare against `offset_of!`, so the Rust and Slang halves of
/// an ABI cannot drift apart silently.
///
/// The settings are [`compile`]'s exactly, so what is reflected is what the
/// engine runs. Reflecting an entry point reflects every type reachable from
/// the module's parameters, not only what the entry point reads.
pub fn reflect(source: &Path, entry: &str, options: &Options) -> Result<Reflection, SlangError> {
    build(source, &[entry], options, false).map(|program| program.reflection)
}

/// What compiled: this crate's version, the Slang release it carries, and
/// the hash of the engine's modules. A cache of compiled shaders keys on
/// this, so a new compiler or a new engine misses it.
pub fn version() -> String {
    format!(
        "punchcutter-slang {} / Slang {} / modules {:016x}",
        env!("CARGO_PKG_VERSION"),
        slang::SLANG_VERSION,
        modules::hash()
    )
}

/// One global session per thread, made on first use.
///
/// Creating one opens the embedded library and loads Slang's core module,
/// which is the one expensive step; a session for a compile is cheap beside
/// it. Per thread rather than shared: Slang allows one global session on
/// many threads only under a lock, and distinct global sessions without
/// one, so two threads compiling at once do not wait on each other.
fn with_global<T>(
    f: impl FnOnce(&slang::GlobalSession) -> Result<T, SlangError>,
) -> Result<T, SlangError> {
    thread_local! {
        static GLOBAL: Result<slang::GlobalSession, String> =
            slang::GlobalSession::try_new().map_err(|err| err.to_string());
    }
    GLOBAL.with(|global| match global {
        Ok(global) => f(global),
        Err(detail) => Err(SlangError::Toolchain {
            detail: detail.clone(),
        }),
    })
}

/// The one compile: a session, the module, its entry points, one link, and
/// the code and reflection out of it.
fn build(
    source: &Path,
    entries: &[&str],
    options: &Options,
    with_code: bool,
) -> Result<Program, SlangError> {
    let source_path = source.to_path_buf();
    let all_entries = entries.join(", ");
    let failed =
        |entry: &str, diagnostics: &slang::Diagnostics, err: &slang::Error| SlangError::Compile {
            source_path: source_path.clone(),
            entry: entry.to_string(),
            diagnostics: if diagnostics.is_empty() {
                err.to_string()
            } else {
                diagnostics.as_str().trim_end().to_string()
            },
        };

    // A session per compile: Slang never replaces a module a session has
    // loaded, so a session that outlived one compile would hand the next an
    // import as it was, not as it is on disk.
    let session = with_global(|global| session(global, source, options))?;

    let mut diagnostics = slang::Diagnostics::new();
    let module = session
        .load_module_with_diagnostics(&source.to_string_lossy(), &mut diagnostics)
        .map_err(|err| failed(&all_entries, &diagnostics, &err))?;

    let mut parts: Vec<slang::ComponentType> = vec![module.clone().into()];
    for entry in entries {
        let point = module
            .find_and_check_entry_point_with_diagnostics(
                entry,
                slang::Stage::Compute,
                &mut diagnostics,
            )
            .map_err(|err| failed(entry, &diagnostics, &err))?;
        parts.push(point.into());
    }
    let program = session
        .create_composite_component_type_with_diagnostics(&parts, &mut diagnostics)
        .map_err(|err| failed(&all_entries, &diagnostics, &err))?;
    let linked = program
        .link_with_diagnostics(&mut diagnostics)
        .map_err(|err| failed(&all_entries, &diagnostics, &err))?;

    let layout = linked
        .layout_with_diagnostics(0, &mut diagnostics)
        .map_err(|err| failed(&all_entries, &diagnostics, &err))?;
    let reflection = Reflection {
        bindings: bindings(layout)?,
        structs: struct_offsets(layout),
        strings: hashed_strings(layout),
    };

    let mut compiled = Vec::with_capacity(entries.len());
    if with_code {
        for (index, entry) in entries.iter().enumerate() {
            let code = linked
                .entry_point_code_with_diagnostics(index as i64, 0, &mut diagnostics)
                .map_err(|err| failed(entry, &diagnostics, &err))?;
            let spirv = code.as_slice().to_vec();
            validate_spirv(&spirv).map_err(|diagnostics| SlangError::Validation {
                source_path: source_path.clone(),
                entry: entry.to_string(),
                diagnostics,
            })?;
            compiled.push(Entry {
                name: entry.to_string(),
                spirv,
            });
        }
    }

    Ok(Program {
        entries: compiled,
        warnings: diagnostics.as_str().trim().to_string(),
        reflection,
    })
}

/// The search path under which the engine's modules are served from memory:
/// the last on every session's list, so a caller's own file of the same
/// name wins, and never a real directory.
const ENGINE: &str = "punchcutter-modules";

/// A session with the fixed settings and the search path: the source's own
/// directory, the caller's, then the engine's modules, which come from the
/// copies compiled into this crate rather than from disk.
fn session(
    global: &slang::GlobalSession,
    source: &Path,
    options: &Options,
) -> Result<slang::Session, SlangError> {
    let toolchain = |detail: String| SlangError::Toolchain { detail };

    let mut session_options = slang::CompilerOptions::default().vulkan_use_entry_point_name(true);
    for define in options.defines {
        let (name, value) = define.split_once('=').unwrap_or((define, ""));
        session_options = session_options.macro_define(name, value);
    }
    if options.image_format_unknown {
        session_options = session_options.default_image_format_unknown(true);
    }

    let capability = global.find_capability("spvShaderClockKHR");
    if capability.is_unknown() {
        return Err(toolchain(
            "Slang does not know the capability spvShaderClockKHR".into(),
        ));
    }
    let profile = global.find_profile("glsl_450");
    if profile.is_unknown() {
        return Err(toolchain("Slang does not know the profile glsl_450".into()));
    }
    let target_options = slang::CompilerOptions::default().capability(capability);
    let targets = [slang::TargetDesc::default()
        .format(slang::CompileTarget::Spirv)
        .profile(profile)
        .options(&target_options)];

    let search: Vec<&Path> = source
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .into_iter()
        .chain(options.includes.iter().copied())
        .chain([Path::new(ENGINE)])
        .collect();
    // Slang asks for `<search path>/<name>`; under the engine's pseudo
    // directory the answer is the compiled-in text, everywhere else the
    // platform's file.
    let file_system = slang::FileSystem::new(|path: &str, default: &slang::DefaultFileSystem| {
        let served = path
            .strip_prefix(ENGINE)
            .and_then(|rest| rest.strip_prefix(['/', '\\']))
            .and_then(|name| modules::ALL.iter().find(|(file, _)| *file == name));
        match served {
            Some((_, text)) => Ok(text.as_bytes().to_vec()),
            None => default.load(path),
        }
    });
    let desc = slang::SessionDesc::default()
        .targets(&targets)
        .search_paths(search)
        .file_system(&file_system)
        .options(&session_options);
    global
        .try_create_session(&desc)
        .map_err(|err| toolchain(format!("Slang could not create a session: {err}")))
}

/// Every `getStringHash("…")` literal in the program, keyed by its hash.
fn hashed_strings(layout: &Shader) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for literal in layout.hashed_strings() {
        let hash = slang::reflection::compute_string_hash(literal);
        if let Some(other) = out.insert(hash, literal.to_string())
            && other != literal
        {
            // Two literals on one hash would make a lookup ambiguous, and
            // silence here would surface as the wrong text on screen.
            tracing::warn!("slang: {literal:?} and {other:?} share the hash {hash}");
        }
    }
    out
}

/// Every global parameter of the program, sorted by `(set, index)`.
///
/// A parameter this cannot classify is an [`SlangError::Reflection`] naming
/// it, never a silent skip: a binding dropped from a layout renumbers every
/// binding after it, which is a wrong buffer with no error anywhere.
///
/// A specialisation constant (`[[vk::constant_id(n)]] const …`) is a global
/// parameter too, and binds nothing: its value is the pipeline's, set when the
/// pipeline is created, so it has no place in a descriptor set and none here.
fn bindings(layout: &Shader) -> Result<Vec<Binding>, SlangError> {
    let mut out = Vec::with_capacity(layout.parameter_count() as usize);
    for parameter in layout.parameters() {
        let name = parameter.name().unwrap_or("<unnamed>").to_string();
        let blame = |detail: String| SlangError::Reflection {
            detail: format!("the parameter `{name}` {detail}"),
        };
        match parameter.category() {
            Some(slang::ParameterCategory::DescriptorTableSlot) => {}
            Some(slang::ParameterCategory::SpecializationConstant) => continue,
            other => {
                return Err(blame(format!(
                    "binds as {other:?} rather than a descriptor table slot"
                )));
            }
        }
        let type_layout = parameter
            .type_layout()
            .ok_or_else(|| blame("has no type".into()))?;
        let kind = match type_layout.kind() {
            slang::TypeKind::ConstantBuffer => BindingKind::Uniform,
            slang::TypeKind::SamplerState => BindingKind::Sampler,
            slang::TypeKind::Resource => {
                let shape = type_layout
                    .resource_shape()
                    .ok_or_else(|| blame("is a resource of no shape".into()))?;
                let access = type_layout.resource_access();
                match shape.base_shape() {
                    slang::ResourceShape::SlangStructuredBuffer => BindingKind::Storage {
                        read_only: match access {
                            Some(slang::ResourceAccess::Read) => true,
                            Some(slang::ResourceAccess::ReadWrite) => false,
                            other => {
                                return Err(blame(format!(
                                    "is a structured buffer with access {other:?}, which has \
                                     no descriptor here"
                                )));
                            }
                        },
                        stride: type_layout
                            .element_type_layout()
                            .map(|element| element.size(slang::ParameterCategory::Uniform))
                            .filter(|size| *size > 0)
                            .map(|size| size as u32),
                    },
                    slang::ResourceShape::SlangTexture2d => match access {
                        Some(slang::ResourceAccess::Read) => BindingKind::Texture {
                            array: shape.is_array(),
                        },
                        Some(slang::ResourceAccess::Write) => {
                            BindingKind::StorageTexture { write_only: true }
                        }
                        Some(slang::ResourceAccess::ReadWrite) => {
                            BindingKind::StorageTexture { write_only: false }
                        }
                        other => {
                            return Err(blame(format!(
                                "is a 2D texture with access {other:?}, which has no \
                                 descriptor here"
                            )));
                        }
                    },
                    other => {
                        return Err(blame(format!(
                            "is a resource of shape {other:?}, which has no descriptor here"
                        )));
                    }
                }
            }
            other => {
                return Err(blame(format!(
                    "has type kind {other:?}, which has no descriptor here"
                )));
            }
        };
        out.push(Binding {
            name,
            set: parameter.binding_space(),
            index: parameter.binding_index(),
            kind,
        });
    }
    out.sort_by_key(|binding| (binding.set, binding.index));
    Ok(out)
}

/// Every struct the program lays out, as `name -> {field -> byte offset}`,
/// found by walking down from each global and entry-point parameter.
fn struct_offsets(layout: &Shader) -> BTreeMap<String, BTreeMap<String, usize>> {
    fn walk(
        type_layout: &TypeLayout,
        depth: usize,
        out: &mut BTreeMap<String, BTreeMap<String, usize>>,
    ) {
        // A type cannot nest deeper than a pointer chain, and one of those
        // is not an ABI struct.
        if depth > 32 {
            return;
        }
        match type_layout.kind() {
            slang::TypeKind::Struct => {
                let mut offsets = BTreeMap::new();
                for field in type_layout.fields() {
                    if let Some(name) = field.name() {
                        offsets.insert(
                            name.to_string(),
                            field.offset(slang::ParameterCategory::Uniform),
                        );
                    }
                    if let Some(inner) = field.type_layout() {
                        walk(inner, depth + 1, out);
                    }
                }
                if !offsets.is_empty()
                    && let Some(name) = type_layout.name()
                {
                    out.insert(name.to_string(), offsets);
                }
            }
            _ => {
                if let Some(element) = type_layout.element_type_layout() {
                    walk(element, depth + 1, out);
                }
            }
        }
    }

    let mut out = BTreeMap::new();
    for parameter in layout.parameters() {
        if let Some(type_layout) = parameter.type_layout() {
            walk(type_layout, 0, &mut out);
        }
    }
    for entry_point in layout.entry_points() {
        for parameter in entry_point.parameters() {
            if let Some(type_layout) = parameter.type_layout() {
                walk(type_layout, 0, &mut out);
            }
        }
    }
    out
}

/// Run `spirv-val` over a compiled module when SPIRV-Tools is installed.
///
/// Optional by design: a machine without it must still build the workspace
/// and still compile shaders, so an absent validator is a `debug!` line and
/// an `Ok`. Where the validator does exist, a malformed module is far nicer
/// to read here than as a device-lost three layers down. `Err` carries
/// spirv-val's own text.
pub fn validate_spirv(spirv: &[u8]) -> Result<(), String> {
    let file = TempPath::new("spv");
    if let Err(err) = std::fs::write(file.path(), spirv) {
        tracing::debug!(
            path = %file.path().display(),
            "cannot write the module for spirv-val ({err}); skipping SPIR-V validation"
        );
        return Ok(());
    }
    let result = match Command::new("spirv-val").arg(file.path()).output() {
        Ok(result) => result,
        Err(err) => {
            tracing::debug!("spirv-val unavailable ({err}); skipping SPIR-V validation");
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
/// Unique per process, per call: several shaders may be validated at once,
/// and a stale file from a crashed run must never be mistaken for fresh
/// output.
struct TempPath(PathBuf);

impl TempPath {
    fn new(extension: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("punchcutter-{}-{seq}.{extension}", std::process::id());
        Self(std::env::temp_dir().join(name))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        // Best effort: a leftover temp file is not worth a log line, and the
        // interesting error (the validation failure) is already on its way up.
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests;
