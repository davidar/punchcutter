//! punchcutter-slang — the engine's Slang modules, and the compiler that
//! builds a shader against them.
//!
//! The compiler is Slang itself, in process: the `shader-slang` crate carries
//! the official Slang release inside the binary, so a build script, a test
//! and a program that recompiles its shader at run time all compile the same
//! way, with nothing installed and no environment variable set. Every
//! failure path ends in a [`SlangError`] carrying Slang's diagnostics
//! *verbatim*: that text is the entire feedback loop for whoever is editing
//! the shader, so a caller that keeps its old module when a recompile fails
//! can print it straight through.
//!
//! One copy of the compiler settings means the build, the tests and a
//! run-time compile cannot disagree about them.
//!
//! # The modules
//!
//! [`modules`] holds `text.slang`, `shape.slang` and the two `shape.slang`
//! imports as strings compiled into this crate. A shader compiled here says
//! `import text;` or `import shape;` and nothing else: the modules are on the
//! search path of every compile, written out once per machine by
//! [`slang_dir`].
//!
//! # What a compile produces
//!
//! A [`Program`]: one SPIR-V module per entry point, from one link of all of
//! them, and the [`Reflection`] of the whole program. The reflection carries
//! the three things a host needs from its shader and would otherwise keep a
//! second copy of by hand: every global parameter's descriptor
//! ([`Binding`]), every struct's field offsets ([`Reflection::structs`]), and
//! the literal behind every `getStringHash("…")`, keyed by the hash the
//! shader hands back ([`Reflection::strings`]). All of it comes out of the
//! same link that produced the code, so there is no second run to disagree
//! with the first.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use shader_slang as slang;
use slang::reflection::{Shader, TypeLayout};

/// The engine's Slang modules, compiled into this crate.
///
/// A consumer's shader says `import text;` or `import shape;`; `shape`
/// imports `ot` and `shape_abi`. The four travel inside any binary built on
/// this crate, so the engine a program was built against is the engine it
/// runs with, and no checkout has to be where it was when the program was
/// built. Every compile through this crate finds them; [`slang_dir`] is where
/// they are written for anything else that wants to.
pub mod modules {
    use std::io;
    use std::path::Path;

    /// Glyph and icon coverage.
    pub const TEXT: &str = include_str!("../modules/text.slang");
    /// The shaper.
    pub const SHAPE: &str = include_str!("../modules/shape.slang");
    /// OpenType's binary format, walked in place.
    pub const OT: &str = include_str!("../modules/ot.slang");
    /// The arena's header, rendered from `punchcutter-shape`'s declarations.
    pub const SHAPE_ABI: &str = include_str!("../modules/shape_abi.slang");

    /// Every module with the file name an `import` resolves to.
    pub const ALL: [(&str, &str); 4] = [
        ("text.slang", TEXT),
        ("shape.slang", SHAPE),
        ("ot.slang", OT),
        ("shape_abi.slang", SHAPE_ABI),
    ];

    /// One number for the four, FNV-1a over their names and texts: what
    /// [`slang_dir`](super::slang_dir) names its directory by, so two
    /// versions of the engine on one machine never read each other's files.
    pub fn hash() -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for (name, text) in ALL {
            for byte in name.bytes().chain([0]).chain(text.bytes()).chain([0]) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    /// Writes the four modules into `dir`, creating it, so that `dir` can go
    /// on a Slang search path. A compile through this crate needs none of
    /// this; it is for a shader compiled some other way.
    pub fn write_to(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (name, text) in ALL {
            std::fs::write(dir.join(name), text)?;
        }
        Ok(())
    }

    /// Whether `dir` already holds the four, byte for byte.
    pub(super) fn is_written(dir: &Path) -> bool {
        ALL.iter().all(|(name, text)| {
            std::fs::read(dir.join(name)).is_ok_and(|on_disk| on_disk == text.as_bytes())
        })
    }
}

/// A directory holding the engine's Slang modules ([`modules`]): the last
/// entry on the search path of every compile through this crate, and a
/// directory to hand to anything else that compiles a shader importing them.
///
/// The modules are written from the copies compiled into this crate, so the
/// directory exists wherever the binary runs. It is
/// `$XDG_CACHE_HOME/punchcutter/slang/<hash>/` (`~/.cache` when that is
/// unset), named by [`modules::hash`] so each version of the engine has its
/// own; the system temporary directory stands in when the cache is not
/// writable. Written once, atomically (into a sibling, then renamed), and
/// checked rather than rewritten when already there. Resolved once per
/// process, so a caller can hash or compare it.
///
/// When no directory can be written the last one tried comes back as it is,
/// and the compile that uses it reports the missing module, with the reason
/// on stderr here.
pub fn slang_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let name = format!("{:016x}", modules::hash());
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
        let roots = cache.into_iter().chain([std::env::temp_dir()]);
        let mut last = PathBuf::from(&name);
        for root in roots {
            let dir = root.join("punchcutter").join("slang").join(&name);
            if modules::is_written(&dir) {
                return dir;
            }
            match materialise(&dir) {
                Ok(()) => return dir,
                Err(err) => eprintln!("punchcutter: cannot write {}: {err}", dir.display()),
            }
            last = dir;
        }
        last
    })
}

/// Writes the modules into a sibling of `dir` and renames it into place, so
/// no reader ever sees a half-written directory. Two processes doing this at
/// once both succeed: the rename that loses finds the winner's copy in place.
fn materialise(dir: &Path) -> std::io::Result<()> {
    let parent = dir
        .parent()
        .expect("a directory under a cache root has a parent");
    let staging = parent.join(format!(
        "{}.tmp-{}",
        dir.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("slang"),
        std::process::id()
    ));
    modules::write_to(&staging)?;
    match std::fs::rename(&staging, dir) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            if modules::is_written(dir) {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The compile
// ---------------------------------------------------------------------------

/// The settings of one compile, beyond the source and its entry points.
///
/// What is *not* here is fixed for every compile: SPIR-V for the `glsl_450`
/// profile with the `spvShaderClockKHR` capability (the shaper reads the
/// device's wall clock, and Slang would otherwise upgrade the profile with a
/// warning), compute entry points, and entry points named in the SPIR-V by
/// their own names.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options<'a> {
    /// Preprocessor definitions, `NAME` or `NAME=VALUE`.
    pub defines: &'a [&'a str],
    /// Directories searched for an `import`, after the source's own and
    /// before the engine's modules.
    pub includes: &'a [&'a Path],
    /// Give storage images no format, so a `RWTexture2D` binds to a texture
    /// of whatever format the host made, and the host names the format when
    /// it builds the layout.
    pub image_format_unknown: bool,
}

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

/// A module Slang accepted: its code, its reflection, and whatever Slang said
/// on the way.
#[derive(Clone, Debug)]
pub struct Program {
    /// One SPIR-V module per entry point asked for, in that order. Empty
    /// when the program was only reflected.
    pub entries: Vec<Entry>,
    /// Slang's diagnostics on a *successful* compile — warnings, trimmed,
    /// empty when it was quiet. A build that treats warnings as errors gates
    /// on this.
    pub warnings: String,
    pub reflection: Reflection,
}

/// One entry point's SPIR-V.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub spirv: Vec<u8>,
}

/// What a program declares, read back from the link that compiled it.
///
/// The point of reading these back is that a program declares them *once*,
/// in the shader. The alternative, a second, hand-written list on the CPU
/// kept in agreement by tests, is exactly the kind of duplication reflection
/// exists to end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reflection {
    /// Every global parameter, sorted by `(set, index)`.
    ///
    /// The host builds its descriptor-set layouts from this. Sorted because
    /// a layout's entries are consumed in binding order (wgpu-hal numbers a
    /// Vulkan binding by an entry's *position* in the list) and Slang lists
    /// the parameters in declaration order, which need not be set order.
    pub bindings: Vec<Binding>,
    /// Every struct the program lays out, as `name -> {field -> byte offset}`.
    ///
    /// A field's offset is relative to its own struct, wherever the struct is
    /// used: inside a constant buffer, as a buffer's element, in an array.
    /// Structs with no laid-out fields (an interface's witness table, say)
    /// are not here.
    pub structs: BTreeMap<String, BTreeMap<String, usize>>,
    /// Every `getStringHash("…")` literal in the program, keyed by its hash:
    /// the number the shader hands back to the CPU, which is what lets a
    /// shader name a string and the CPU find its text without either side
    /// holding a list of strings. A program with no literals has an empty
    /// table. The empty literal hashes to 0, so 0 means "no string" in
    /// practice.
    pub strings: BTreeMap<u32, String>,
}

impl Reflection {
    /// The reflection as JSON, for a build script to bake beside the SPIR-V
    /// and a process to carry or cache: [`from_json`](Self::from_json) reads
    /// it back. The format is `serde_json`'s rendering of this struct, and
    /// changes only when the struct does.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a reflection has nothing that cannot be JSON")
    }

    /// A reflection [`to_json`](Self::to_json) wrote.
    ///
    /// Errors rather than panics: a caller that reads this on a run-time
    /// reload has to be able to fail that reload and keep the running
    /// shader and its old strings, never swap in half a pair.
    pub fn from_json(json: &str) -> Result<Reflection, SlangError> {
        serde_json::from_str(json).map_err(|err| SlangError::Reflection {
            detail: format!("not a reflection this crate wrote: {err}"),
        })
    }
}

/// One global parameter of a compiled program: where its descriptor lands,
/// and what kind of descriptor it has to be.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    /// The parameter's name in the Slang source, which is the only handle a
    /// caller has on it. Two facts a descriptor needs are *not* in the
    /// reflection — the texel format of a storage image the program declares
    /// format-unknown, and which buffer to bind — and both are looked up by
    /// this.
    pub name: String,
    /// The descriptor set (`space` in Slang).
    pub set: u32,
    /// The binding number within the set.
    pub index: u32,
    pub kind: BindingKind,
}

/// What kind of descriptor a [`Binding`] needs, in the terms a layout entry is
/// written in.
///
/// Deliberately not a wgpu type: this crate is what a build script compiles
/// with, and a build script has no business pulling the graphics stack into
/// the build graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BindingKind {
    /// `ConstantBuffer<T>`.
    Uniform,
    /// `StructuredBuffer<T>` (read-only) or `RWStructuredBuffer<T>`.
    Storage {
        read_only: bool,
        /// One element's size in bytes, when Slang laid the element type out
        /// — the floor a binding of this buffer has to clear.
        stride: Option<u32>,
    },
    /// `RWTexture2D<T>` — written, and read back too when `write_only` is
    /// false. The *texel format* is not here: SPIR-V bakes it into the module
    /// and a format-unknown image ([`Options::image_format_unknown`])
    /// reflects none at all, so the caller supplies it by name.
    StorageTexture { write_only: bool },
    /// `Texture2D<T>` or `Texture2DArray<T>`, sampled.
    Texture { array: bool },
    /// `SamplerState`.
    Sampler,
}

/// What compiled: this crate's version, the Slang release it carries, and
/// the hash of the engine's modules. A cache of compiled shaders keys on
/// this, so a new compiler or a new engine misses it.
pub fn version() -> String {
    format!(
        "punchcutter-slang {} / Slang {} / modules {:016x}",
        env!("CARGO_PKG_VERSION"),
        slang::embedded::version(),
        modules::hash()
    )
}

/// One global session per thread, made on first use.
///
/// Creating one opens the embedded library and loads Slang's core module,
/// which is the one expensive step; a session for a compile is cheap beside
/// it. Per thread rather than shared because Slang's global session is not
/// documented as safe to use from two threads at once, and a compile has no
/// lock to take this way.
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

/// A session with the fixed settings and the search path: the source's own
/// directory, the caller's, then the engine's modules.
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
        .chain([slang_dir()])
        .collect();
    let desc = slang::SessionDesc::default()
        .targets(&targets)
        .search_paths(search)
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
fn bindings(layout: &Shader) -> Result<Vec<Binding>, SlangError> {
    let mut out = Vec::with_capacity(layout.parameter_count() as usize);
    for parameter in layout.parameters() {
        let name = parameter.name().unwrap_or("<unnamed>").to_string();
        let blame = |detail: String| SlangError::Reflection {
            detail: format!("the parameter `{name}` {detail}"),
        };
        match parameter.category() {
            Some(slang::ParameterCategory::DescriptorTableSlot) => {}
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

/// Everything that can go wrong between a saved `.slang` file and a SPIR-V
/// module the GPU will accept.
///
/// The `diagnostics` payloads are Slang's (or spirv-val's) own output,
/// unmodified apart from trailing whitespace — a caller prints them straight
/// through, and column-accurate error text is the point.
#[derive(Debug)]
pub enum SlangError {
    /// The Slang compiler carried in this program could not be started:
    /// its library could not be written out or opened, or it does not know
    /// the profile or capability every compile here uses.
    Toolchain { detail: String },
    /// Slang rejected the module. `entry` names the entry point whose step
    /// failed, or every entry point asked for when the module itself did.
    Compile {
        source_path: PathBuf,
        entry: String,
        diagnostics: String,
    },
    /// `spirv-val` rejected the module Slang produced.
    Validation {
        source_path: PathBuf,
        entry: String,
        diagnostics: String,
    },
    /// The program's reflection could not be read into a [`Reflection`]: a
    /// parameter with no descriptor here. Not a fact about whether the
    /// shader compiles, and the compile it fails is the right place to find
    /// out.
    Reflection { detail: String },
}

impl SlangError {
    /// The tool's own diagnostic text, when there is one. Handy for a
    /// log that wants to print the message body without the wrapper prose.
    pub fn diagnostics(&self) -> Option<&str> {
        match self {
            SlangError::Compile { diagnostics, .. }
            | SlangError::Validation { diagnostics, .. } => Some(diagnostics),
            SlangError::Toolchain { .. } | SlangError::Reflection { .. } => None,
        }
    }
}

impl fmt::Display for SlangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlangError::Toolchain { detail } => {
                write!(f, "the Slang compiler could not be started: {detail}")
            }
            SlangError::Compile {
                source_path,
                entry,
                diagnostics,
            } => write!(
                f,
                "Slang failed for {} (entry `{entry}`):\n{diagnostics}",
                source_path.display()
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
                "the program's reflection has no reading here: {detail}\n\
                 the module itself compiled; this is the reader and not the shader."
            ),
        }
    }
}

impl std::error::Error for SlangError {}

#[cfg(test)]
mod tests;
