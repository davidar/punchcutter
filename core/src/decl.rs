//! **Declared once**: the two `macro_rules!` every
//! struct and constant of an ABI is written through, and the tables they
//! build.
//!
//! [`abi_struct!`](crate::abi_struct) emits the Rust struct *and* an [`AbiStruct`] describing it:
//! its name, its `///` text, its fields with the Slang type each renders as,
//! and a function giving `offset_of!` for every field. [`abi_consts!`](crate::abi_consts) emits a
//! group of constants *and* a [`ConstGroup`] describing them. [`render`] turns
//! those tables into a Slang module, which is **checked in** and compared
//! against the rendering by a test — so the Slang half is written by the Rust half and never kept by hand.
//!
//! Nothing here is read at run time. The tables cost two pointers a struct in the binary, and there is no pair of declarations to compare.
//!
//! # Who declares through it
//!
//! The OpenType arena's header in `shape/src/abi.rs` is declared this way and
//! rendered to `slang/shape_abi.slang` by `cargo run -p punchcutter-shape --bin
//! gen-slang`. The mechanism lives here rather than in that crate so that a
//! consumer can declare its own buffers the same way, without depending on the
//! shaper.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// The tables
// ---------------------------------------------------------------------------

/// One field's Slang type: a base type name, and — for an array — the Slang
/// *spelling* of its length, which is the constant's own name wherever the
/// Rust declaration used one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlangTy {
    pub base: &'static str,
    pub len: Option<&'static str>,
}

impl SlangTy {
    pub const fn scalar(base: &'static str) -> SlangTy {
        SlangTy { base, len: None }
    }

    pub const fn array(base: &'static str, len: &'static str) -> SlangTy {
        SlangTy {
            base,
            len: Some(len),
        }
    }

    /// The field as Slang declares it, semicolon included.
    pub fn declare(&self, name: &str) -> String {
        match self.len {
            Some(len) => format!("public {} {name}[{len}];", self.base),
            None => format!("public {} {name};", self.base),
        }
    }
}

/// The Slang name of a type a field may have. Implemented for the three scalars here and for every ABI struct by [`abi_struct!`](crate::abi_struct).
pub trait SlangType {
    const NAME: &'static str;
}

impl SlangType for u32 {
    const NAME: &'static str = "uint";
}

impl SlangType for i32 {
    const NAME: &'static str = "int";
}

impl SlangType for f32 {
    const NAME: &'static str = "float";
}

/// One field of a declared struct.
#[derive(Clone, Copy, Debug)]
pub struct FieldDecl {
    pub name: &'static str,
    pub ty: SlangTy,
    pub doc: &'static [&'static str],
}

/// One declared struct, as Slang needs to see it.
#[derive(Clone, Copy, Debug)]
pub struct StructDecl {
    pub name: &'static str,
    pub doc: &'static [&'static str],
    pub fields: &'static [FieldDecl],
}

/// A declared struct and the offsets this side gives its fields.
///
/// `bound` says whether the compiled shader reaches the struct from one of its
/// bindings. slangc reflects the types a parameter's layout needs and nothing
/// else, so a struct the GPU does not read through a binding has no
/// reflected layout to compare against and the layout test says so rather than failing.
#[derive(Clone, Copy)]
pub struct AbiStruct {
    pub decl: StructDecl,
    pub bound: bool,
    pub offsets: fn() -> Vec<(&'static str, usize)>,
}

impl std::fmt::Debug for AbiStruct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbiStruct")
            .field("decl", &self.decl)
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

/// One declared constant. `literal` is the Rust source of its value, kept so a
/// hexadecimal mask renders as one rather than as ten digits.
#[derive(Clone, Copy, Debug)]
pub struct ConstDecl {
    pub name: &'static str,
    pub value: u32,
    pub literal: &'static str,
    pub doc: &'static [&'static str],
}

impl ConstDecl {
    /// The value as Slang spells it: the Rust literal's own hexadecimal, minus
    /// the digit separators Slang has no use for, or the decimal value for
    /// anything computed.
    pub fn slang_value(&self) -> String {
        match self.literal.trim().strip_prefix("0x") {
            Some(hex) => format!("0x{}u", hex.replace('_', "")),
            None => format!("{}u", self.value),
        }
    }
}

/// A run of constants rendered under one banner, in declaration order.
#[derive(Clone, Copy, Debug)]
pub struct ConstGroup {
    pub title: &'static str,
    pub consts: &'static [ConstDecl],
}

// ---------------------------------------------------------------------------
// The macros
// ---------------------------------------------------------------------------

/// The Slang type one field renders as.
///
/// Every field of this ABI is a `u32`, an `i32`, an `f32`, an array of one of those, an
/// ABI struct or an array of one — and a field type is always exactly one
/// token tree, which is what lets the length's *spelling* survive into Slang.
/// The two- and four-wide scalar arrays are vectors (`float2`, `uint4`); every
/// other length is an array, including a four-wide array of vectors.
#[macro_export]
macro_rules! slang_ty {
    ([f32; 2]) => {
        $crate::decl::SlangTy::scalar("float2")
    };
    ([f32; 4]) => {
        $crate::decl::SlangTy::scalar("float4")
    };
    ([u32; 2]) => {
        $crate::decl::SlangTy::scalar("uint2")
    };
    ([u32; 4]) => {
        $crate::decl::SlangTy::scalar("uint4")
    };
    ([[f32; 4]; $len:expr]) => {
        $crate::decl::SlangTy::array("float4", stringify!($len))
    };
    ([[u32; 4]; $len:expr]) => {
        $crate::decl::SlangTy::array("uint4", stringify!($len))
    };
    ([$elem:tt; $len:expr]) => {
        $crate::decl::SlangTy::array(<$elem as $crate::decl::SlangType>::NAME, stringify!($len))
    };
    ($ty:tt) => {
        $crate::decl::SlangTy::scalar(<$ty as $crate::decl::SlangType>::NAME)
    };
}

/// One ABI struct, declared once.
///
/// The attribute group between the doc comment and the struct is emitted
/// verbatim — the `repr` and the derives are the struct's own: a struct that
/// carries a vector is `align(16)`, the arena's are plain `repr(C)`. It is a
/// brace group rather than loose attributes because a matcher cannot tell
/// `#[doc]` from `#[repr]` without ambiguity.
///
/// ```ignore
/// abi_struct! {
///     /// One run's parameters (uniform). 16 B.
///     { #[repr(C, align(16))] #[derive(Clone, Copy, Debug, Default, PartialEq)] }
///     pub struct RunParams {
///         /// Pixels per em.
///         pub px_per_em: f32,
///     }
/// }
/// ```
#[macro_export]
macro_rules! abi_struct {
    (
        $(#[doc = $sdoc:literal])*
        { $(#[$attr:meta])* }
        $vis:vis struct $name:ident {
            $(
                $(#[doc = $fdoc:literal])*
                $fvis:vis $field:ident : $fty:tt
            ),* $(,)?
        }
    ) => {
        $(#[doc = $sdoc])*
        $(#[$attr])*
        $vis struct $name {
            $(
                $(#[doc = $fdoc])*
                $fvis $field: $fty,
            )*
        }

        impl $crate::decl::SlangType for $name {
            const NAME: &'static str = stringify!($name);
        }

        impl $name {
            /// This struct as its generated Slang module renders it.
            pub const DECL: $crate::decl::StructDecl = $crate::decl::StructDecl {
                name: stringify!($name),
                doc: &[$($sdoc),*],
                fields: &[$(
                    $crate::decl::FieldDecl {
                        name: stringify!($field),
                        ty: $crate::slang_ty!($fty),
                        doc: &[$($fdoc),*],
                    },
                )*],
            };

            /// Every field's byte offset on this side, in declaration order.
            pub fn offsets() -> Vec<(&'static str, usize)> {
                vec![$((stringify!($field), ::core::mem::offset_of!($name, $field)),)*]
            }
        }
    };
}

/// A group of ABI constants, declared once.
///
/// The group's own name is a private `ConstGroup` this module lists in its
/// table; the banner is what the rendered Slang carries above the run.
///
/// ```ignore
/// abi_consts! {
///     DIRECTIONS = "Which way a run is laid out";
///     pub const DIRECTION_LTR: u32 = 0;
///     pub const DIRECTION_RTL: u32 = 1;
/// }
/// ```
#[macro_export]
macro_rules! abi_consts {
    (
        $group:ident = $title:literal;
        $(
            $(#[doc = $cdoc:literal])*
            $vis:vis const $name:ident : $ty:ty = $value:expr;
        )*
    ) => {
        $(
            $(#[doc = $cdoc])*
            $vis const $name: $ty = $value;
        )*

        const $group: $crate::decl::ConstGroup = $crate::decl::ConstGroup {
            title: $title,
            consts: &[$(
                $crate::decl::ConstDecl {
                    name: stringify!($name),
                    value: $name as u32,
                    literal: stringify!($value),
                    doc: &[$($cdoc),*],
                },
            )*],
        };
    };
}

// ---------------------------------------------------------------------------
// The renderer
// ---------------------------------------------------------------------------

/// One generated Slang module, rendered from its tables.
///
/// `command` is how the file says to regenerate itself — each crate's own, and
/// the same string the test that compares the checked-in file with the
/// rendering prints. `normative` is the sentence naming the document and the
/// crate this module is a rendering *of*. `prose` is the module's own header comment.
pub fn render(
    command: &str,
    normative: &[&str],
    module: &str,
    source: &str,
    prose: &[&str],
    imports: &[&str],
    groups: &[ConstGroup],
    structs: &[AbiStruct],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// GENERATED by {command} from {source} — do not edit."
    );
    let _ = writeln!(out, "//");
    for line in normative {
        let _ = writeln!(out, "// {line}");
    }
    if !prose.is_empty() {
        let _ = writeln!(out, "//");
        for line in prose {
            if line.is_empty() {
                let _ = writeln!(out, "//");
            } else {
                let _ = writeln!(out, "// {line}");
            }
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "module {module};");
    for import in imports {
        let _ = writeln!(out, "{import}");
    }

    for group in groups {
        let _ = writeln!(out);
        let _ = writeln!(out, "// {:-<75}", "");
        for line in wrap(group.title, 75) {
            let _ = writeln!(out, "// {line}");
        }
        let _ = writeln!(out, "// {:-<75}", "");
        for decl in group.consts {
            let _ = writeln!(out);
            for line in decl.doc {
                let _ = writeln!(out, "///{}", rustdoc(line));
            }
            let _ = writeln!(
                out,
                "public static const uint {} = {};",
                decl.name,
                decl.slang_value()
            );
        }
    }

    for entry in structs {
        let offsets: Vec<(&str, usize)> = (entry.offsets)();
        let decl = &entry.decl;
        let _ = writeln!(out);
        for line in decl.doc {
            let _ = writeln!(out, "///{}", rustdoc(line));
        }
        let _ = writeln!(out, "public struct {}", decl.name);
        let _ = writeln!(out, "{{");
        // The offset comments line up in a column: they are the byte offsets
        // docs/arena.md tabulates, and reading a struct against that table is
        // what they are for.
        let widest = decl
            .fields
            .iter()
            .map(|field| field.ty.declare(field.name).len())
            .max()
            .unwrap_or(0);
        for (index, field) in decl.fields.iter().enumerate() {
            for line in field.doc {
                let _ = writeln!(out, "    ///{}", rustdoc(line));
            }
            let text = field.ty.declare(field.name);
            let offset = offsets
                .get(index)
                .filter(|(name, _)| *name == field.name)
                .map(|(_, offset)| *offset);
            match offset {
                Some(offset) => {
                    let _ = writeln!(out, "    {text:<widest$}  // {offset}");
                }
                None => {
                    let _ = writeln!(out, "    {text}");
                }
            }
        }
        let _ = writeln!(out, "}};");
    }

    out
}

/// One `///` line as Slang should read it: rustdoc's intra-doc link brackets
/// mean nothing outside Rust, where a name in backticks says the same thing.
fn rustdoc(line: &str) -> String {
    line.replace("[`", "`").replace("`]", "`")
}

/// A banner's text, folded to `width` columns on word boundaries.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
