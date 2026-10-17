//! The demo reel: half a minute of video in which every pixel is the engine's.
//!
//! Each frame is three compute passes in one submit (`reel.slang`): the
//! shaper shapes every string, a typesetter breaks the shaped runs into
//! lines, justifies and aligns them, and a draw pass covers every pixel from
//! the glyphs' own curves through a camera and a lens. The CPU loads the font,
//! decides the clock and the camera's targets, and writes the finished pixels
//! out; it never sees a glyph id or a position.
//!
//! The frames go to stdout as raw RGBA, for an encoder to read:
//!
//! ```text
//! cargo run -p punchcutter-fonts --example reel | \
//!     ffmpeg -f rawvideo -pix_fmt rgba -s 1280x720 -r 60 -i - \
//!     -c:v libx264 -pix_fmt yuv420p -crf 18 reel.mp4
//! ```
//!
//! `--live` plays it in a window instead, in real time and on a loop, the
//! frames going from the image buffer straight to the swapchain (Escape
//! quits). The window is the `live` feature, so `--live` is `cargo run -p
//! punchcutter-fonts --features live --example reel -- --live`. `--profile`
//! prints each pass's GPU time every two seconds, from timestamp queries, and
//! `--samples 1|4` sets the coverage samples a pixel (four offline; live, by
//! the window's size). `--still <seconds> <file.png>` writes one frame.
//! `--size WxH`, `--fps N`, `--from <s>` and `--to <s>` change what is
//! rendered, and
//! `--font <path>` the face (else fontconfig's EB Garamond, else Noto Serif).
//! The adapter is chosen as `text/examples/render.rs` chooses it, integrated
//! or software, unless `--adapter discrete` asks for a discrete GPU: a render
//! of half a minute is worth waking one for, and a GPU the desktop is not
//! drawing on leaves the desktop alone while it works.

#[cfg(feature = "live")]
mod live;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use punchcutter_core::plain::{Plain, bytes_of, bytes_of_slice};
use punchcutter_fonts::fallback::Chain;
use punchcutter_fonts::{Loaded, Merged};
use punchcutter_shape::{Batch, Feature, ShapeRequest, ShapeRun, ShapedGlyph};

/// Pixels per em every string is shaped at; `SHAPE_PX` in `reel.slang`.
const SHAPE_PX: f32 = 256.0;

/// Bytes per shaped string: a paragraph is cut at a space before this, well
/// inside `MAX_RUN_GLYPHS`.
const CHUNK: usize = 180;

/// How long the reel runs, seconds.
const LENGTH: f32 = 33.0;

/// Rows the draw pass covers per submit. A whole frame in one dispatch is
/// around a tenth of a second on an integrated GPU, which is a tenth of a
/// second the compositor sharing it cannot draw the cursor; a strip of this
/// many rows is a few milliseconds.
const STRIP: u32 = 48;

const BLOCK_SCREEN: u32 = 1;
const BLOCK_HERO: u32 = 2;
const BLOCK_WAVE: u32 = 4;
const BLOCK_WRAP: u32 = 8;
const BLOCK_REVEAL: u32 = 16;

const ALIGN_LEFT: u32 = 0;
const ALIGN_CENTRE: u32 = 1;
const ALIGN_RIGHT: u32 = 2;
const ALIGN_JUSTIFY: u32 = 3;

const RUN_PARAGRAPH: u32 = 1;
const RUN_BLANK: u32 = 2;

const IVORY: [f32; 4] = [0.93, 0.89, 0.81, 1.0];
const COPPER: [f32; 4] = [0.91, 0.50, 0.30, 1.0];
const DIM: [f32; 4] = [0.60, 0.56, 0.50, 1.0];

const SMCP: &[Feature] = &[Feature::on(b"smcp")];
const KERN_OFF: &[Feature] = &[Feature::off(b"kern")];
const ONUM: &[Feature] = &[Feature::on(b"onum")];
const FRAC: &[Feature] = &[Feature::on(b"frac")];

// ---------------------------------------------------------------------------
// The structs the passes share, mirroring `reel.slang`
// ---------------------------------------------------------------------------

/// `ReelBlock`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Block {
    origin: [f32; 2],
    width: f32,
    size: f32,
    leading: f32,
    gap: f32,
    columns: u32,
    rows: u32,
    align: u32,
    flags: u32,
    run_first: u32,
    run_count: u32,
    glyph_first: u32,
    glyph_cap: u32,
    line_first: u32,
    line_cap: u32,
    type_start: f32,
    type_rate: f32,
    opacity: f32,
    hero_byte: u32,
}

/// `ReelRun`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RunMeta {
    block: u32,
    byte_in_block: u32,
    flags: u32,
    _rsvd0: u32,
    colour: [f32; 4],
}

/// `ReelFrame`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    clock: [f32; 4],
    cam_a: [f32; 4],
    cam_b: [f32; 4],
    cam: [f32; 4],
    lens: [f32; 4],
    hero: [f32; 4],
    probe: [f32; 4],
    look: [f32; 4],
    counts: [u32; 4],
    out: [u32; 4],
    partial: [u32; 4],
    run_list: [[u32; 4]; 4],
    block_list: [[u32; 4]; 8],
}

/// `Frame::partial`: shape, or set, all of them.
const EVERY: u32 = u32::MAX;

// SAFETY: `#[repr(C)]` aggregates of 4-byte leaves with no implicit padding.
unsafe impl Plain for Block {}
unsafe impl Plain for RunMeta {}
unsafe impl Plain for Frame {}

/// Bytes of the GPU-only structs: `ReelGlyph`, `ReelLine`, `ReelBlockOut`,
/// `ReelScene`.
const GLYPH_BYTES: u64 = 32;
const LINE_BYTES: u64 = 16;
const BLOCK_OUT_BYTES: u64 = 32;
const SCENE_BYTES: u64 = 32;
/// `ReelView`: a camera, a count, and `MAX_VISIBLE` block indices.
const VIEW_BYTES: u64 = 16 + 16 + 64 * 4;

// ---------------------------------------------------------------------------
// The text
// ---------------------------------------------------------------------------

/// How a paragraph is set.
#[derive(Clone, Copy)]
struct Style {
    colour: [f32; 4],
    features: &'static [Feature],
}

const BODY: Style = Style {
    colour: IVORY,
    features: &[],
};
const HEADING: Style = Style {
    colour: COPPER,
    features: SMCP,
};

/// Where and how a block is set; the runs and slots are the builder's.
#[derive(Clone, Copy)]
struct Spec {
    origin: [f32; 2],
    width: f32,
    size: f32,
    leading: f32,
    columns: u32,
    rows: u32,
    align: u32,
    flags: u32,
    type_start: f32,
    type_rate: f32,
}

impl Spec {
    /// One line, never broken, at `origin`.
    fn line(origin: [f32; 2], size: f32, align: u32) -> Spec {
        Spec {
            origin,
            width: 0.0,
            size,
            leading: size * 1.3,
            columns: 1,
            rows: 0,
            align,
            flags: 0,
            type_start: 0.0,
            type_rate: 0.0,
        }
    }

    fn screen(mut self) -> Spec {
        self.flags |= BLOCK_SCREEN;
        self
    }

    fn typed(mut self, start: f32, rate: f32) -> Spec {
        self.type_start = start;
        self.type_rate = rate;
        self
    }

    /// Shaped whole from the first frame, its glyphs shown at the times
    /// typing would show them: for a centred line, which would otherwise move
    /// as it grew.
    fn revealed(mut self, start: f32, rate: f32) -> Spec {
        self.flags |= BLOCK_REVEAL;
        self.typed(start, rate)
    }
}

/// The batch, its runs' metadata and the blocks, built together so the
/// indices agree.
#[derive(Default)]
struct Reel {
    batch: Batch,
    runs: Vec<RunMeta>,
    /// Each run's whole text, for typing it out a prefix at a time.
    texts: Vec<String>,
    blocks: Vec<Block>,
    glyphs: u32,
    lines: u32,
}

impl Reel {
    /// Add a block of paragraphs; returns its index. `hero` names a byte of
    /// the block's text whose glyph the overlay draws.
    fn add(&mut self, spec: Spec, paragraphs: &[(&str, Style)], hero: Option<usize>) -> usize {
        let index = self.blocks.len();
        let run_first = self.runs.len() as u32;
        let mut byte = 0u32;
        let mut glyph_cap = 0u32;
        for (p, (text, style)) in paragraphs.iter().enumerate() {
            for (c, chunk) in chunks(text, CHUNK).into_iter().enumerate() {
                let request = self
                    .batch
                    .push_with_features(chunk, SHAPE_PX, style.features)
                    .expect("the reel forces few enough feature sets to share one pool");
                glyph_cap += self.batch.requests()[request].out_cap;
                let mut flags = 0;
                if c == 0 {
                    flags |= RUN_PARAGRAPH;
                    // A heading after the first paragraph gets a line of air.
                    if p > 0 && style.features == SMCP && spec.flags & BLOCK_WRAP != 0 {
                        flags |= RUN_BLANK;
                    }
                }
                self.runs.push(RunMeta {
                    block: index as u32,
                    byte_in_block: byte,
                    flags,
                    _rsvd0: 0,
                    colour: style.colour,
                });
                self.texts.push(chunk.to_owned());
                byte += chunk.len() as u32;
            }
        }
        let line_cap = if spec.flags & BLOCK_WRAP != 0 && spec.rows > 0 {
            spec.rows * spec.columns
        } else if spec.flags & BLOCK_WRAP != 0 {
            // Balanced on the GPU; room for lines of twenty-five bytes.
            byte / 25 + paragraphs.len() as u32 * 2 + 8
        } else {
            paragraphs.len() as u32 * 2 + 1
        };
        self.blocks.push(Block {
            origin: spec.origin,
            width: spec.width,
            size: spec.size,
            leading: spec.leading,
            gap: spec.size * 3.0,
            columns: spec.columns,
            rows: spec.rows,
            align: spec.align,
            flags: spec.flags | if hero.is_some() { BLOCK_HERO } else { 0 },
            run_first,
            run_count: self.runs.len() as u32 - run_first,
            glyph_first: self.glyphs,
            glyph_cap,
            line_first: self.lines,
            line_cap,
            type_start: spec.type_start,
            type_rate: spec.type_rate,
            opacity: 1.0,
            hero_byte: hero.unwrap_or(usize::MAX) as u32,
        });
        self.glyphs += glyph_cap;
        self.lines += line_cap;
        index
    }

    /// The strings to shape again, and the blocks to set again, given this
    /// frame's `requests` and the length each string had when it was last
    /// shaped: a string part way through being typed, and one whose length
    /// has changed since (it finished, or the reel started again).
    fn typing(&self, requests: &[ShapeRequest], shaped: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let mut runs = Vec::new();
        let mut blocks = Vec::new();
        for (b, block) in self.blocks.iter().enumerate() {
            if block.type_rate <= 0.0 || block.flags & BLOCK_REVEAL != 0 {
                continue;
            }
            let before = runs.len();
            for r in block.run_first..block.run_first + block.run_count {
                let len = requests[r as usize].byte_len;
                let full = self.texts[r as usize].len() as u32;
                if len != shaped[r as usize] || (len > 0 && len < full) {
                    runs.push(r);
                }
            }
            if runs.len() > before {
                blocks.push(b as u32);
            }
        }
        (runs, blocks)
    }

    /// The requests as this frame shapes them: a block being typed has only
    /// the bytes that have arrived, cut back to a character boundary.
    fn requests_at(&self, t: f32) -> Vec<ShapeRequest> {
        let mut requests = self.batch.requests().to_vec();
        for (r, request) in requests.iter_mut().enumerate() {
            let meta = &self.runs[r];
            let block = &self.blocks[meta.block as usize];
            if block.type_rate <= 0.0 || block.flags & BLOCK_REVEAL != 0 {
                continue;
            }
            let typed = ((t - block.type_start) * block.type_rate).max(0.0) as usize;
            let text = &self.texts[r];
            let mut len = typed
                .saturating_sub(meta.byte_in_block as usize)
                .min(text.len());
            while !text.is_char_boundary(len) {
                len -= 1;
            }
            request.byte_len = len as u32;
        }
        requests
    }
}

/// `text` cut at spaces into pieces of at most `max` bytes, each keeping its
/// trailing space so the typesetter still sees the break.
fn chunks(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut limit = max;
        while !rest.is_char_boundary(limit) {
            limit -= 1;
        }
        let cut = rest[..limit].rfind(' ').map(|at| at + 1).unwrap_or(limit);
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// This repository's README as paragraphs of plain text: code blocks and
/// tables dropped, headings marked, list items made paragraphs of their own,
/// and the inline markup taken out.
fn readme_paragraphs(markdown: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut fenced = false;
    let flush = |current: &mut String, out: &mut Vec<(String, bool)>| {
        if !current.trim().is_empty() {
            out.push((inline(current.trim()), false));
        }
        current.clear();
    };
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            flush(&mut current, &mut out);
            continue;
        }
        if fenced || trimmed.starts_with('|') {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut current, &mut out);
        } else if let Some(heading) = trimmed.strip_prefix('#') {
            flush(&mut current, &mut out);
            out.push((inline(heading.trim_start_matches('#').trim()), true));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            flush(&mut current, &mut out);
            current.push_str("• ");
            current.push_str(item);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(trimmed);
        }
    }
    flush(&mut current, &mut out);
    out
}

/// Markdown's inline markup, removed: links keep their text, code spans and
/// emphasis lose their marks.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' | '*' => {}
            '<' => {
                // An autolink: its text is the address.
                for inner in chars.by_ref() {
                    if inner == '>' {
                        break;
                    }
                    out.push(inner);
                }
            }
            ']' if chars.peek() == Some(&'(') => {
                for inner in chars.by_ref() {
                    if inner == ')' {
                        break;
                    }
                }
            }
            '[' => {}
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The script
// ---------------------------------------------------------------------------

/// Block indices the timeline drives.
struct Cast {
    title: usize,
    page: usize,
    board: (usize, usize),
    /// Each row's label, its sample's undrawn whole copy, and when the
    /// sample starts typing.
    rows: Vec<(usize, usize, f32)>,
    finale: (usize, usize),
    captions: Vec<(usize, usize, f32, f32)>,
}

/// The rows of the shaping board: a label, a sample, and what the sample is
/// shaped with.
const BOARD: &[(&str, &str, &[Feature])] = &[
    ("ligatures", "Official waffles & fluffy fjords", &[]),
    ("kerning off", "AVATAR · Wavy Type · Toyota", KERN_OFF),
    ("kerning on", "AVATAR · Wavy Type · Toyota", &[]),
    ("small capitals", "Small Capitals For Headings", SMCP),
    ("old-style figures", "Founded 1879, rebuilt 2026", ONUM),
    ("fractions", "1/2 cup · 3/4 inch · 7/8 mile", FRAC),
    // Bases and marks no precomposed character covers, so the marks are
    // placed by the font's GPOS anchors, not swapped for another glyph.
    (
        "mark attachment",
        "x\u{302} · n\u{306} · g\u{303} · q\u{303}",
        &[],
    ),
    ("fallback faces", "活字 · 日本語 · Ελληνικά", &[]),
];

const BOARD_START: f32 = 17.7;
const BOARD_STEP: f32 = 0.85;
const TYPE_RATE: f32 = 24.0;

fn script(reel: &mut Reel, readme: &str, height: f32) -> Cast {
    // --- the page: a title, and the README in three justified columns ------

    let paragraphs = readme_paragraphs(readme);
    let column = 340.0;
    let size = 10.5;
    let columns = 3u32;
    let page_width = column * columns as f32 + size * 3.0 * (columns - 1) as f32;
    let centre = page_width * 0.5;

    let title_text = "Punches & Curves";
    let title = reel.add(
        Spec::line([centre, 0.0], 84.0, ALIGN_CENTRE),
        &[(title_text, BODY)],
        title_text.find('&'),
    );
    reel.add(
        Spec::line([centre, 44.0], 15.0, ALIGN_CENTRE),
        &[(
            "the readme of a gpu text engine, shaped and set by the engine itself",
            Style {
                colour: COPPER,
                features: SMCP,
            },
        )],
        None,
    );
    let body: Vec<(&str, Style)> = paragraphs
        .iter()
        .map(|(text, heading)| (text.as_str(), if *heading { HEADING } else { BODY }))
        .collect();
    let page = reel.add(
        Spec {
            origin: [0.0, 110.0],
            width: column,
            size,
            leading: size * 1.32,
            columns,
            // The typesetter counts the lines and balances the columns.
            rows: 0,
            align: ALIGN_JUSTIFY,
            flags: BLOCK_WRAP,
            type_start: 0.0,
            type_rate: 0.0,
        },
        &body,
        None,
    );

    // --- the shaping board, to the right of the page -----------------------

    let board_x = page_width + 1300.0;
    let pitch = 84.0;
    let mut rows_cast = Vec::new();
    let board_first = reel.blocks.len();
    for (i, (label, sample, features)) in BOARD.iter().enumerate() {
        let y = i as f32 * pitch;
        let start = BOARD_START + i as f32 * BOARD_STEP;
        let label = reel.add(
            Spec::line([board_x - 36.0, y], 19.0, ALIGN_RIGHT),
            &[(
                label,
                Style {
                    colour: COPPER,
                    features: SMCP,
                },
            )],
            None,
        );
        let sample_text = *sample;
        reel.add(
            Spec::line([board_x, y], 46.0, ALIGN_LEFT).typed(start, TYPE_RATE),
            &[(
                sample,
                Style {
                    colour: IVORY,
                    features,
                },
            )],
            None,
        );
        // The same row shaped whole and never drawn: what the camera frames,
        // so the board is framed as it will be rather than as it grows.
        let ghost = reel.add(
            Spec::line([board_x, y], 46.0, ALIGN_LEFT),
            &[(
                sample_text,
                Style {
                    colour: IVORY,
                    features,
                },
            )],
            None,
        );
        rows_cast.push((label, ghost, start));
    }
    let board = (board_first, reel.blocks.len() - 1);

    // --- the last word, below the board ------------------------------------

    let finale_y = BOARD.len() as f32 * pitch + 900.0;
    let finale_x = board_x + 200.0;
    let name = reel.add(
        Spec {
            flags: BLOCK_WAVE,
            ..Spec::line([finale_x, finale_y], 150.0, ALIGN_CENTRE)
        },
        &[("punchcutter", BODY)],
        None,
    );
    let tagline = reel.add(
        Spec::line([finale_x, finale_y + 74.0], 23.0, ALIGN_CENTRE).revealed(28.6, 34.0),
        &[(
            "text shaped and cut from its curves on the gpu, every frame",
            Style {
                colour: COPPER,
                features: SMCP,
            },
        )],
        None,
    );

    // --- the captions, in screen space, bottom left -------------------------

    let scale = height / 720.0;
    let x = 56.0 * scale;
    let mut captions = Vec::new();
    for (main, sub, from, to) in [
        (
            "Every glyph is cut from its own curves.",
            "slug coverage: two rays a sample, no atlas, no bitmaps",
            1.0,
            5.3,
        ),
        (
            "The engine sets its own readme.",
            "shaped, broken into lines and justified on the gpu",
            8.8,
            11.9,
        ),
        (
            "Magnified per pixel, not per texture.",
            "the lens asks the same curves at three times the size",
            12.3,
            15.2,
        ),
        (
            "OpenType, in a compute shader.",
            "gsub and gpos, shaped again every frame as it types",
            17.6,
            26.0,
        ),
    ] {
        let a = reel.add(
            Spec::line([x, height - 76.0 * scale], 25.0 * scale, ALIGN_LEFT).screen(),
            &[(main, BODY)],
            None,
        );
        let b = reel.add(
            Spec::line([x, height - 48.0 * scale], 15.0 * scale, ALIGN_LEFT).screen(),
            &[(
                sub,
                Style {
                    colour: DIM,
                    features: SMCP,
                },
            )],
            None,
        );
        captions.push((a, b, from, to));
    }

    Cast {
        title,
        page,
        board,
        rows: rows_cast,
        finale: (name, tagline),
        captions,
    }
}

fn smooth(t: f32, a: f32, b: f32) -> f32 {
    let x = ((t - a) / (b - a)).clamp(0.0, 1.0);
    x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
}

/// In over `[a, b]`, out over `[c, d]`.
fn window(t: f32, a: f32, b: f32, c: f32, d: f32) -> f32 {
    smooth(t, a, b) * (1.0 - smooth(t, c, d))
}

/// The camera's stops: when, what it frames (`[-1, -1]` is the hero glyph,
/// else a range of blocks), how full, and how far up the screen, pixels at
/// 720 lines.
fn camera_keys(cast: &Cast) -> Vec<(f32, [f32; 2], f32, f32)> {
    let hero = [-1.0, -1.0];
    let page = [cast.title as f32, cast.page as f32];
    let board = [cast.board.0 as f32, cast.board.1 as f32];
    let finale = [cast.finale.0 as f32, cast.finale.1 as f32];
    vec![
        (0.0, hero, 0.74, 22.0),
        (5.4, hero, 0.80, 22.0),
        (9.8, page, 0.86, 26.0),
        (15.4, page, 0.88, 26.0),
        (17.6, board, 0.80, 34.0),
        (26.4, board, 0.82, 34.0),
        (28.4, finale, 0.72, 0.0),
        (LENGTH, finale, 0.76, 0.0),
    ]
}

/// Everything the CPU decides about the frame at `t`: the uniform, and each
/// block's opacity.
fn direct(cast: &Cast, reel: &mut Reel, t: f32, width: u32, height: u32) -> Frame {
    let keys = camera_keys(cast);
    let at = keys
        .windows(2)
        .position(|pair| t < pair[1].0)
        .unwrap_or(keys.len() - 2);
    let (t0, a, fa, la) = keys[at];
    let (t1, b, fb, lb) = keys[at + 1];
    let lift = height as f32 / 720.0;
    let s = smooth(t, t0, t1);

    let (w, h) = (width as f32, height as f32);
    let lens_on = window(t, 10.0, 10.8, 14.4, 15.2);
    let u = smooth(t, 10.0, 15.2);
    let lens = [
        w * (0.24 + 0.52 * u),
        h * (0.56 + 0.16 * (u * std::f32::consts::TAU).sin()),
        185.0 * h / 720.0,
        1.0 + 2.2 * lens_on,
    ];

    for block in reel.blocks.iter_mut() {
        block.opacity = 1.0;
    }
    // The title's other letters wait while the ampersand is being cut.
    reel.blocks[cast.title].opacity = 0.18 + 0.82 * smooth(t, 5.0, 7.0);
    for &(label, ghost, start) in &cast.rows {
        reel.blocks[label].opacity = smooth(t, start - 0.4, start + 0.1);
        reel.blocks[ghost].opacity = 0.0;
    }
    reel.blocks[cast.finale.0].opacity = smooth(t, 26.6, 28.0);
    let mut scrim = 0.0f32;
    for &(main, sub, from, to) in &cast.captions {
        let o = window(t, from, from + 0.7, to - 0.6, to);
        reel.blocks[main].opacity = o;
        reel.blocks[sub].opacity = o;
        scrim = scrim.max(o);
    }

    Frame {
        clock: [t, 0.0, 0.0, 0.0],
        cam_a: [a[0], a[1], fa, la * lift],
        cam_b: [b[0], b[1], fb, lb * lift],
        cam: [s, scrim, 0.0, 0.0],
        lens,
        hero: [
            window(t, 0.2, 0.7, 4.9, 5.9),
            smooth(t, 0.4, 3.0),
            smooth(t, 3.4, 4.7),
            window(t, 1.6, 2.3, 4.4, 5.2),
        ],
        probe: [
            0.5 + 0.36 * (1.3 * t).sin(),
            0.5 + 0.34 * (0.9 * t + 1.0).sin(),
            0.85 * window(t, 0.3, 1.2, 4.6, 5.6),
            0.0,
        ],
        look: [
            window(t, 0.0, 0.8, LENGTH - 0.9, LENGTH),
            0.07 * (1.0 - smooth(t, 27.4, 30.2)),
            2.6,
            t * 3.2,
        ],
        counts: [reel.blocks.len() as u32, width, height, 0],
        out: [width, 0, 0, 0],
        partial: [EVERY, EVERY, 0, 0],
        run_list: [[0; 4]; 4],
        block_list: [[0; 4]; 8],
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

struct Args {
    width: u32,
    height: u32,
    fps: f32,
    from: f32,
    to: f32,
    still: Option<(f32, PathBuf)>,
    font: Option<PathBuf>,
    discrete: bool,
    live: bool,
    profile: bool,
    /// Coverage samples a pixel, 1 or 4; unset, four offline and by the
    /// window's size live.
    samples: Option<u32>,
}

fn args() -> Result<Args> {
    let mut out = Args {
        width: 1280,
        height: 720,
        fps: 60.0,
        from: 0.0,
        to: LENGTH,
        still: None,
        font: None,
        discrete: false,
        live: false,
        profile: false,
        samples: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} wants a value"));
        match arg.as_str() {
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').context("--size is WxH")?;
                out.width = w.parse()?;
                out.height = h.parse()?;
            }
            "--fps" => out.fps = value()?.parse()?,
            "--from" => out.from = value()?.parse()?,
            "--to" => out.to = value()?.parse()?,
            "--font" => out.font = Some(PathBuf::from(value()?)),
            "--live" => out.live = true,
            "--profile" => out.profile = true,
            "--samples" => {
                out.samples = match value()?.as_str() {
                    "1" => Some(1),
                    "4" => Some(4),
                    other => bail!("--samples is 1 or 4, not {other}"),
                }
            }
            "--adapter" => {
                out.discrete = match value()?.as_str() {
                    "discrete" => true,
                    "integrated" => false,
                    other => bail!("--adapter is integrated or discrete, not {other}"),
                }
            }
            "--still" => {
                let t = value()?.parse()?;
                out.still = Some((t, PathBuf::from(value()?)));
            }
            other => bail!("unknown argument {other}; see the header of examples/reel/main.rs"),
        }
    }
    Ok(out)
}

/// The face: the one asked for, else EB Garamond, else Noto Serif.
fn face(asked: Option<PathBuf>) -> Result<(String, PathBuf)> {
    if let Some(path) = asked {
        let family = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok((family, path));
    }
    ["EB Garamond", "Noto Serif"]
        .into_iter()
        .find_map(|family| punchcutter_fonts::fc_match(family).filter(|(found, _)| found == family))
        .context("neither EB Garamond nor Noto Serif is installed; pass --font <path>")
}

/// A serif face for the CJK sample, so it does not fall back to a sans; the
/// usual chain when there is none.
fn chain(primary: &Path) -> Chain {
    let found = std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}\t%{file}\t%{index}", "serif:lang=ja"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|line| {
            let mut fields = line.split('\t');
            let family = fields.next()?.to_owned();
            let path = PathBuf::from(fields.next()?);
            // fontconfig puts a variable font's named instance above bit 16.
            let index = fields.next()?.trim().parse::<u32>().ok()? & 0xFFFF;
            Some((family, path, index))
        });
    match found {
        Some(face) if face.0.contains("Serif") => Chain::of([face]),
        _ => Chain::discover(primary),
    }
}

fn main() -> Result<()> {
    let args = args()?;
    if !args.live && args.still.is_none() && std::io::stdout().is_terminal() {
        bail!(
            "the frames are raw RGBA on stdout; pipe them to an encoder, or pass --live \
             (see the header)"
        );
    }
    // Pinning the loader to the integrated GPU's driver would hide a discrete
    // one, so it is only done when the integrated one is wanted.
    if !args.discrete
        && std::env::var_os("VK_DRIVER_FILES").is_none()
        && let Some(icd) = integrated_icd()
    {
        // SAFETY: called on the one thread that exists, before wgpu's instance
        // and therefore before the loader reads the variable.
        unsafe { std::env::set_var("VK_DRIVER_FILES", &icd) };
        eprintln!("VK_DRIVER_FILES was unset; pinned to {icd}");
    }

    if args.live && !cfg!(feature = "live") {
        bail!(
            "--live needs the window, which is the `live` feature: \
             cargo run -p punchcutter-fonts --features live --example reel -- --live"
        );
    }
    let (fonts, readme) = load(&args)?;
    #[cfg(feature = "live")]
    if args.live {
        return live::run(args, fonts, readme);
    }

    let gpu = Gpu::new(&instance(), args.discrete, args.profile, None)?;
    let mut renderer = Renderer::new(gpu, fonts, readme, args.width, args.height, false)?;
    renderer.samples = args.samples.unwrap_or(4);
    if args.profile {
        renderer.profiler = Some(Profiler::new(&renderer.gpu));
    }
    let times: Vec<f32> = match &args.still {
        Some((t, _)) => vec![*t],
        None => {
            let count = ((args.to - args.from) * args.fps).round() as u32;
            (0..count)
                .map(|i| args.from + i as f32 / args.fps)
                .collect()
        }
    };
    let mut stdout = std::io::BufWriter::with_capacity(1 << 22, std::io::stdout().lock());
    let mut rgba = Vec::new();
    let started = Instant::now();
    for (i, &t) in times.iter().enumerate() {
        renderer.draw(t)?;
        renderer.read(&mut rgba)?;
        match &args.still {
            Some((_, path)) => {
                write_png(path, args.width, args.height, &rgba)?;
                eprintln!("wrote {} (t = {t} s)", path.display());
            }
            None => stdout
                .write_all(&rgba)
                .context("the encoder stopped reading")?,
        }
        if (i + 1) % 60 == 0 || i + 1 == times.len() {
            let elapsed = started.elapsed().as_secs_f64();
            eprintln!(
                "frame {}/{} ({:.1} ms a frame)",
                i + 1,
                times.len(),
                elapsed * 1e3 / (i + 1) as f64
            );
        }
    }
    stdout.flush()?;
    if let Some(profiler) = renderer.profiler.as_mut() {
        profiler.report();
    }
    eprintln!("adapter used: {}", renderer.gpu.name);
    Ok(())
}

/// The font, merged with the faces its fallback will need, and the README the
/// page sets.
fn load(args: &Args) -> Result<(Merged, String)> {
    let (family, path) = face(args.font.clone())?;
    let loaded = Loaded::load_path(&family, &path).map_err(anyhow::Error::msg)?;
    eprintln!(
        "font: {family} ({}), {} glyphs, {} curve buffers",
        path.display(),
        loaded.font_glyphs,
        loaded.start()
    );
    let fonts = Merged::with_chain(loaded, chain(&path));
    let readme_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../README.md");
    let readme = std::fs::read_to_string(&readme_path)
        .with_context(|| format!("could not read {}", readme_path.display()))?;
    Ok((fonts, readme))
}

// ---------------------------------------------------------------------------
// The renderer: one frame at a time, into the image buffer
// ---------------------------------------------------------------------------

/// Everything a frame is drawn from: the device, the three passes, their
/// buffers and the script.
struct Renderer {
    gpu: Gpu,
    passes: Passes,
    buffers: Buffers,
    groups: (wgpu::BindGroup, wgpu::BindGroup),
    reel: Reel,
    cast: Cast,
    /// For setting the captions again when a window is resized.
    #[cfg_attr(not(feature = "live"), allow(dead_code))]
    readme: String,
    width: u32,
    height: u32,
    /// Pixels per row of the image buffer: the width rounded up to 64, so that
    /// a row is a whole number of the 256 bytes a texture copy wants.
    pitch: u32,
    /// Whether the image's bytes go out blue first, as a BGRA swapchain wants.
    bgra: bool,
    /// Coverage samples a pixel, 1 or 4.
    samples: u32,
    /// Whether every string has been shaped and every block set at least
    /// once at this size. After that a frame does only the typed ones.
    settled: bool,
    /// Each string's length when it was last shaped.
    shaped: Vec<u32>,
    /// Blocks to set again on the next frame whatever the typing does: the
    /// captions, after a resize moves them.
    #[cfg_attr(not(feature = "live"), allow(dead_code))]
    reset: Vec<u32>,
    /// `--profile`'s timestamps, when asked for.
    profiler: Option<Profiler>,
    /// For reading frames back to the CPU, made on the first read.
    readback: Option<wgpu::Buffer>,
}

impl Renderer {
    fn new(
        gpu: Gpu,
        mut fonts: Merged,
        readme: String,
        width: u32,
        height: u32,
        bgra: bool,
    ) -> Result<Renderer> {
        let mut reel = Reel::default();
        let cast = script(&mut reel, &readme, height as f32);
        let everything: String = reel.texts.concat();
        let cover = fonts.cover(&everything);
        for face in &cover.faces {
            eprintln!(
                "fallback: {} codepoints from {}",
                face.codepoints, face.family
            );
        }
        eprintln!(
            "{} strings in {} blocks, {} glyph slots, {} line slots",
            reel.runs.len(),
            reel.blocks.len(),
            reel.glyphs,
            reel.lines
        );

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/reel/reel.slang");
        let started = Instant::now();
        let program = punchcutter_slang::compile_program(
            &source,
            &["reel_shape", "reel_typeset", "reel_view", "reel_frame"],
            &punchcutter_slang::Options::default(),
        )
        .map_err(|err| anyhow::anyhow!("{err}"))?;
        for line in program.warnings.lines() {
            eprintln!("slang: {line}");
        }
        eprintln!(
            "shaders compiled in {:.0} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
        let passes = gpu.passes(&program)?;

        let loaded = &fonts.loaded;
        let mut params = reel.batch.params(&loaded.arena);
        if gpu.clock {
            params.deadline_ticks = punchcutter_shape::abi::SHAPE_DEADLINE_TICKS;
        }
        let pitch = width.next_multiple_of(64);
        let words = reel.batch.utf8_words();
        let buffers = Buffers {
            params: gpu.uniform("params", bytes_of(&params)),
            requests: gpu.storage("requests", bytes_of_slice(reel.batch.requests()), 0),
            ot: gpu.storage("ot", loaded.arena_bytes(), 0),
            utf8: gpu.storage("utf8", words_bytes(&words), 0),
            out_glyphs: gpu.storage(
                "out-glyphs",
                &[],
                u64::from(reel.batch.glyph_capacity()) * size_of::<ShapedGlyph>() as u64,
            ),
            out_runs: gpu.storage(
                "out-runs",
                &[],
                reel.batch.len() as u64 * size_of::<ShapeRun>() as u64,
            ),
            frame: gpu.uniform("frame", bytes_of(&Frame::default())),
            blocks: gpu.storage("blocks", bytes_of_slice(&reel.blocks), 0),
            runs: gpu.storage("runs", bytes_of_slice(&reel.runs), 0),
            placed: gpu.storage("placed", &[], u64::from(reel.glyphs) * GLYPH_BYTES),
            lines: gpu.storage("lines", &[], u64::from(reel.lines) * LINE_BYTES),
            block_out: gpu.storage("block-out", &[], reel.blocks.len() as u64 * BLOCK_OUT_BYTES),
            scene: gpu.storage("scene", &[], SCENE_BYTES),
            image: gpu.storage("image", &[], u64::from(pitch) * u64::from(height) * 4),
            view: gpu.storage("view", &[], VIEW_BYTES),
            curves: loaded
                .curve_bytes()
                .iter()
                .enumerate()
                .map(|(i, bytes)| gpu.storage(&format!("curves-{i}"), bytes, 0))
                .collect(),
        };
        let groups = gpu.groups(&buffers);
        Ok(Renderer {
            gpu,
            passes,
            buffers,
            groups,
            reel,
            cast,
            readme,
            width,
            height,
            pitch,
            bgra,
            samples: 4,
            settled: false,
            shaped: Vec::new(),
            reset: Vec::new(),
            profiler: None,
            readback: None,
        })
    }

    /// A new size: a new image buffer, and the captions placed again for it.
    #[cfg(feature = "live")]
    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pitch = width.next_multiple_of(64);
        self.buffers.image =
            self.gpu
                .storage("image", &[], u64::from(self.pitch) * u64::from(height) * 4);
        self.groups = self.gpu.groups(&self.buffers);
        self.readback = None;
        // Only the captions depend on the size: the strings, their shaping
        // and every block in the world stay as they are, and the captions,
        // placed in pixels, are set again on the next frame.
        let mut fresh = Reel::default();
        self.cast = script(&mut fresh, &self.readme, height as f32);
        self.reel.blocks = fresh.blocks;
        self.reset = (0..self.reel.blocks.len() as u32)
            .filter(|&b| self.reel.blocks[b as usize].flags & BLOCK_SCREEN != 0)
            .collect();
    }

    /// Shape, typeset and draw the frame at `t` into the image buffer.
    fn draw(&mut self, t: f32) -> Result<()> {
        let (gpu, reel) = (&self.gpu, &mut self.reel);
        let mut frame = direct(&self.cast, reel, t, self.width, self.height);
        frame.out = [self.pitch, u32::from(self.bgra), self.samples, 0];
        // What changes from frame to frame is the typing: those strings are
        // shaped again and those blocks set again, and nothing else is, once
        // everything has been done once.
        let requests = reel.requests_at(t);
        let (runs, mut blocks) = if self.settled {
            reel.typing(&requests, &self.shaped)
        } else {
            (Vec::new(), Vec::new())
        };
        for b in self.reset.drain(..) {
            if !blocks.contains(&b) {
                blocks.push(b);
            }
        }
        let partial = self.settled && runs.len() <= 16 && blocks.len() <= 32;
        if partial {
            for &r in &runs {
                self.shaped[r as usize] = requests[r as usize].byte_len;
            }
        } else {
            self.shaped = requests.iter().map(|request| request.byte_len).collect();
        }
        if partial {
            frame.partial = [runs.len() as u32, blocks.len() as u32, 0, 0];
            for (i, &r) in runs.iter().enumerate() {
                frame.run_list[i / 4][i % 4] = r;
            }
            for (i, &b) in blocks.iter().enumerate() {
                frame.block_list[i / 4][i % 4] = b;
            }
        }
        self.settled = true;
        let shape_threads = if partial {
            runs.len() as u32
        } else {
            reel.batch.len() as u32
        };
        let typeset_threads = if partial {
            blocks.len() as u32
        } else {
            reel.blocks.len() as u32
        };
        gpu.queue
            .write_buffer(&self.buffers.frame, 0, bytes_of(&frame));
        gpu.queue
            .write_buffer(&self.buffers.blocks, 0, bytes_of_slice(&reel.blocks));
        gpu.queue
            .write_buffer(&self.buffers.requests, 0, bytes_of_slice(&requests));

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("reel"),
            });
        let groups = &self.groups;
        let profiler = self.profiler.as_ref();
        // `stamps`: which of the pass's begin and end timestamps to write, when
        // profiling; a strip writes only the begin, or only the end, or neither.
        let dispatch =
            |encoder: &mut wgpu::CommandEncoder, pass, x, y, which: usize, stamps: (bool, bool)| {
                let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("reel-pass"),
                    timestamp_writes: profiler
                        .filter(|_| stamps.0 || stamps.1)
                        .map(|p| p.writes(which, stamps)),
                });
                cpass.set_pipeline(pass);
                cpass.set_bind_group(0, &groups.0, &[]);
                cpass.set_bind_group(1, &groups.1, &[]);
                cpass.dispatch_workgroups(x, y, 1);
            };
        // A frame where nothing is being typed shapes and sets nothing.
        let ran = [shape_threads > 0, typeset_threads > 0, true];
        if ran[0] {
            let x = shape_threads.div_ceil(64);
            dispatch(&mut encoder, &self.passes.shape, x, 1, 0, (true, true));
        }
        if ran[1] {
            let x = typeset_threads.div_ceil(64);
            dispatch(&mut encoder, &self.passes.typeset, x, 1, 1, (true, true));
        }
        // One thread, untimed: the camera and the blocks on screen.
        dispatch(&mut encoder, &self.passes.view, 1, 1, 0, (false, false));
        if gpu.discrete {
            // A GPU the desktop is not drawing on takes the whole frame in
            // one dispatch.
            let (x, y) = (self.width.div_ceil(8), self.height.div_ceil(8));
            dispatch(&mut encoder, &self.passes.frame, x, y, 2, (true, true));
            gpu.queue.submit([encoder.finish()]);
            if let Some(profiler) = self.profiler.as_mut() {
                profiler.collect(&self.gpu, ran)?;
            }
            return Ok(());
        }
        gpu.queue.submit([encoder.finish()]);

        // The draw pass, a strip at a time, each waited for before the next,
        // so the desktop's own work gets the GPU in between.
        for top in (0..self.height).step_by(STRIP as usize) {
            let mut strip = frame;
            strip.counts[3] = top;
            gpu.queue
                .write_buffer(&self.buffers.frame, 0, bytes_of(&strip));
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("reel-strip"),
                });
            let (x, y) = (
                self.width.div_ceil(8),
                STRIP.min(self.height - top).div_ceil(8),
            );
            // The draw's time runs from the first strip's start to the last
            // strip's end, the desktop's turns between them included.
            let stamps = (top == 0, top + STRIP >= self.height);
            dispatch(&mut encoder, &self.passes.frame, x, y, 2, stamps);
            gpu.queue.submit([encoder.finish()]);
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|err| anyhow::anyhow!("a strip never finished: {err}"))?;
        }
        if let Some(profiler) = self.profiler.as_mut() {
            profiler.collect(&self.gpu, ran)?;
        }
        Ok(())
    }

    /// The last frame drawn, as tightly packed RGBA rows.
    fn read(&mut self, rgba: &mut Vec<u8>) -> Result<()> {
        let bytes = u64::from(self.pitch) * u64::from(self.height) * 4;
        let gpu = &self.gpu;
        let readback = self.readback.get_or_insert_with(|| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reel-readback"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("reel-copy"),
            });
        encoder.copy_buffer_to_buffer(&self.buffers.image, 0, readback, 0, bytes);
        gpu.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| anyhow::anyhow!("the frame never finished: {err}"))?;
        rx.recv()
            .context("the map callback never fired")?
            .context("the frame would not map")?;
        {
            let view = readback
                .slice(..)
                .get_mapped_range()
                .map_err(|err| anyhow::anyhow!("the frame is unavailable: {err}"))?;
            let row = self.width as usize * 4;
            let stride = self.pitch as usize * 4;
            rgba.clear();
            for y in 0..self.height as usize {
                rgba.extend_from_slice(&view[y * stride..y * stride + row]);
            }
        }
        readback.unmap();
        Ok(())
    }

    /// Copy the last frame drawn into `texture`, a swapchain image of the
    /// same size.
    #[cfg(feature = "live")]
    fn present(&self, texture: &wgpu::Texture) {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("reel-present"),
            });
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffers.image,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.pitch * 4),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue.submit([encoder.finish()]);
    }
}

fn words_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: `u32` has no padding and no invalid bit patterns, and `u8` has
    // alignment 1; the lifetime is the borrow's.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Profiling
// ---------------------------------------------------------------------------

/// The passes `--profile` times, in timestamp-pair order.
const PASSES: [&str; 3] = ["shape", "typeset", "draw"];

/// Each pass's GPU time, from a begin and an end timestamp written by the
/// pass itself, averaged over two seconds at a time.
///
/// It never waits for the GPU: a frame's timestamps are copied out and asked
/// for, and read on a later frame once they have arrived; frames drawn in
/// between are not sampled. Waiting would let the GPU idle between frames and
/// clock down, which is a different machine from the one being measured.
struct Profiler {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// Nanoseconds a timestamp tick.
    period: f64,
    /// Milliseconds summed, and frames that ran the pass, since `since`.
    sums: [f64; 3],
    counts: [u32; 3],
    /// The longest each pass took, since `since`: a frame that misses the
    /// display's interval is a worst case an average hides.
    worst: [f64; 3],
    frames: u32,
    since: Instant,
    /// The passes a sampled frame ran, while its timestamps are on their way.
    pending: Option<([bool; 3], Mapping)>,
}

/// Where a buffer's `map_async` says it is done.
type Mapping = std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>;

impl Profiler {
    fn new(gpu: &Gpu) -> Profiler {
        let bytes = (PASSES.len() * 2 * size_of::<u64>()) as u64;
        Profiler {
            queries: gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("reel-profile"),
                ty: wgpu::QueryType::Timestamp,
                count: PASSES.len() as u32 * 2,
            }),
            resolve: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reel-profile-resolve"),
                size: bytes,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reel-profile-readback"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            period: f64::from(gpu.queue.get_timestamp_period()),
            sums: [0.0; 3],
            counts: [0; 3],
            worst: [0.0; 3],
            frames: 0,
            since: Instant::now(),
            pending: None,
        }
    }

    /// The timestamps pass `which` writes: its begin, its end, or both.
    fn writes(
        &self,
        which: usize,
        (begin, end): (bool, bool),
    ) -> wgpu::ComputePassTimestampWrites<'_> {
        let at = which as u32 * 2;
        wgpu::ComputePassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: begin.then_some(at),
            end_of_pass_write_index: end.then_some(at + 1),
        }
    }

    /// Take the timestamps of an earlier frame if they have arrived, and ask
    /// for this frame's, which ran the passes in `ran`, if none are on their
    /// way. Prints the averages every two seconds.
    fn collect(&mut self, gpu: &Gpu, ran: [bool; 3]) -> Result<()> {
        if let Some((sampled, rx)) = &self.pending {
            gpu.device
                .poll(wgpu::PollType::Poll)
                .map_err(|err| anyhow::anyhow!("the profile poll failed: {err}"))?;
            match rx.try_recv() {
                Ok(mapped) => {
                    mapped.context("the timestamps would not map")?;
                    let sampled = *sampled;
                    self.read(sampled)?;
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    bail!("the timestamps' map callback was dropped")
                }
            }
        }
        if self.pending.is_none() {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("reel-profile"),
                });
            let count = PASSES.len() as u32 * 2;
            encoder.resolve_query_set(&self.queries, 0..count, &self.resolve, 0);
            encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, self.resolve.size());
            gpu.queue.submit([encoder.finish()]);
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            self.readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx.send(r);
                });
            self.pending = Some((ran, rx));
        }
        if self.since.elapsed().as_secs_f32() >= 2.0 {
            self.report();
        }
        Ok(())
    }

    /// The mapped timestamps of a frame that ran the passes in `ran`.
    fn read(&mut self, ran: [bool; 3]) -> Result<()> {
        {
            let view = self
                .readback
                .slice(..)
                .get_mapped_range()
                .map_err(|err| anyhow::anyhow!("the timestamps are unavailable: {err}"))?;
            let tick = |i: usize| {
                u64::from_le_bytes(view[i * 8..i * 8 + 8].try_into().expect("eight bytes"))
            };
            for (pass, &did) in ran.iter().enumerate() {
                if did {
                    let ticks = tick(pass * 2 + 1).wrapping_sub(tick(pass * 2));
                    let ms = ticks as f64 * self.period / 1e6;
                    self.sums[pass] += ms;
                    self.worst[pass] = self.worst[pass].max(ms);
                    self.counts[pass] += 1;
                }
            }
        }
        self.readback.unmap();
        self.frames += 1;
        Ok(())
    }

    /// The averages since the last report, then start again.
    fn report(&mut self) {
        if self.frames == 0 {
            return;
        }
        let mut line = format!("profile, {} frames sampled:", self.frames);
        let mut total = 0.0;
        for (pass, name) in PASSES.iter().enumerate() {
            if self.counts[pass] == 0 {
                line += &format!(" {name} -");
                continue;
            }
            let mean = self.sums[pass] / f64::from(self.counts[pass]);
            total += self.sums[pass];
            line += &format!(" {name} {mean:.2} ms (worst {:.2})", self.worst[pass]);
            if self.counts[pass] < self.frames {
                line += &format!(" (in {} frames)", self.counts[pass]);
            }
        }
        line += &format!("; GPU {:.2} ms a frame", total / f64::from(self.frames));
        eprintln!("{line}");
        self.sums = [0.0; 3];
        self.counts = [0; 3];
        self.worst = [0.0; 3];
        self.frames = 0;
        self.since = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------

/// The integrated GPU's Vulkan driver manifest on a machine with more than
/// one GPU, as `text/examples/render.rs` finds it.
fn integrated_icd() -> Option<String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    if nodes.len() < 2 {
        return None;
    }
    nodes.sort();
    let driver = nodes.iter().find_map(|node| {
        let link = std::fs::read_link(format!("/sys/class/drm/{node}/device/driver")).ok()?;
        let name = link.file_name()?.to_str()?.to_owned();
        (name != "nvidia").then_some(name)
    })?;
    let manifest = match driver.as_str() {
        "amdgpu" | "radeon" => "radeon_icd",
        "i915" | "xe" => "intel_icd",
        _ => return None,
    };
    let path = format!(
        "/usr/share/vulkan/icd.d/{manifest}.{}.json",
        std::env::consts::ARCH
    );
    Path::new(&path).is_file().then_some(path)
}

struct Gpu {
    /// For the window's surface to be configured against.
    #[cfg_attr(not(feature = "live"), allow(dead_code))]
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Set 0, every pass's: the shaper's six bindings and the reel's eight.
    layout: wgpu::BindGroupLayout,
    /// Set 1: `text.slang`'s four curve buffers.
    font_layout: wgpu::BindGroupLayout,
    clock: bool,
    discrete: bool,
    name: String,
}

/// A Vulkan instance. Vulkan wants no display handle up front; a window's
/// surface brings its own.
fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

struct Passes {
    shape: wgpu::ComputePipeline,
    typeset: wgpu::ComputePipeline,
    view: wgpu::ComputePipeline,
    frame: wgpu::ComputePipeline,
}

struct Buffers {
    params: wgpu::Buffer,
    requests: wgpu::Buffer,
    ot: wgpu::Buffer,
    utf8: wgpu::Buffer,
    out_glyphs: wgpu::Buffer,
    out_runs: wgpu::Buffer,
    frame: wgpu::Buffer,
    blocks: wgpu::Buffer,
    runs: wgpu::Buffer,
    placed: wgpu::Buffer,
    lines: wgpu::Buffer,
    block_out: wgpu::Buffer,
    scene: wgpu::Buffer,
    image: wgpu::Buffer,
    view: wgpu::Buffer,
    curves: Vec<wgpu::Buffer>,
}

/// Set 0's bindings, in order: `true` for a uniform, else a storage buffer
/// and whether it is read-only.
const SET0: [(bool, bool); 15] = [
    (true, true),   // params
    (false, true),  // requests
    (false, true),  // ot
    (false, false), // utf8: read-write, as `ShapeTables.utf8` is declared
    (false, false), // out_glyphs
    (false, false), // out_runs
    (true, true),   // frame
    (false, true),  // blocks
    (false, true),  // runs
    (false, false), // placed
    (false, false), // lines
    (false, false), // block_out
    (false, false), // scene
    (false, false), // image
    (false, false), // view
];

impl Gpu {
    /// The adapter asked for, and one that can present to `surface` when
    /// there is a window.
    fn new(
        instance: &wgpu::Instance,
        discrete: bool,
        profile: bool,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Gpu> {
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let adapter = adapters
            .into_iter()
            .find(|adapter| {
                let info = adapter.get_info();
                if surface.is_some_and(|surface| !adapter.is_surface_supported(surface)) {
                    return false;
                }
                if discrete {
                    return info.device_type == wgpu::DeviceType::DiscreteGpu;
                }
                let usable = info.device_type == wgpu::DeviceType::IntegratedGpu
                    || info.device_type == wgpu::DeviceType::Cpu;
                usable && !info.name.to_ascii_uppercase().contains("NVIDIA")
            })
            .context(if discrete {
                "--adapter discrete, but there is no discrete Vulkan adapter"
            } else {
                "no integrated or software Vulkan adapter; pass --adapter discrete to use another"
            })?;
        let info = adapter.get_info();
        eprintln!("adapter: {} ({:?})", info.name, info.device_type);
        if !adapter
            .features()
            .contains(wgpu::Features::PASSTHROUGH_SHADERS)
        {
            bail!("adapter '{}' lacks PASSTHROUGH_SHADERS", info.name);
        }
        if profile && !adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            bail!(
                "adapter '{}' has no timestamp queries to profile with",
                info.name
            );
        }

        let wanted = SET0.iter().filter(|(uniform, _)| !uniform).count() as u32 + 4;
        let ceiling = adapter.limits().max_storage_buffers_per_shader_stage;
        if ceiling < wanted {
            bail!("the frame binds {wanted} storage buffers; this adapter allows {ceiling}");
        }
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: wanted,
            max_storage_buffer_binding_size: adapter.limits().max_storage_buffer_binding_size,
            max_buffer_size: adapter.limits().max_buffer_size,
            ..wgpu::Limits::default()
        };
        let (device, queue, clock) = punchcutter_core::gpu::open_device(
            &adapter,
            &wgpu::DeviceDescriptor {
                label: Some("reel-device"),
                required_features: if profile {
                    wgpu::Features::PASSTHROUGH_SHADERS | wgpu::Features::TIMESTAMP_QUERY
                } else {
                    wgpu::Features::PASSTHROUGH_SHADERS
                },
                required_limits: limits,
                ..Default::default()
            },
        )
        .context("Vulkan device creation failed")?;

        let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let set0: Vec<_> = SET0
            .iter()
            .enumerate()
            .map(|(i, &(uniform, read_only))| {
                entry(
                    i as u32,
                    if uniform {
                        wgpu::BufferBindingType::Uniform
                    } else {
                        wgpu::BufferBindingType::Storage { read_only }
                    },
                )
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reel-set0"),
            entries: &set0,
        });
        let set1: Vec<_> = (0..4)
            .map(|i| entry(i, wgpu::BufferBindingType::Storage { read_only: true }))
            .collect();
        let font_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reel-set1"),
            entries: &set1,
        });
        Ok(Gpu {
            adapter,
            device,
            queue,
            layout,
            font_layout,
            clock: clock == punchcutter_core::gpu::ShaderClock::Device,
            discrete,
            name: info.name,
        })
    }

    fn passes(&self, program: &punchcutter_slang::Program) -> Result<Passes> {
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("reel-pl"),
                bind_group_layouts: &[Some(&self.layout), Some(&self.font_layout)],
                ..Default::default()
            });
        let spirv = |name: &str| {
            program
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.spirv.as_slice())
                .with_context(|| format!("the program has no entry {name}"))
        };
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let make = |entry: &'static str, workgroup| -> Result<wgpu::ComputePipeline> {
            Ok(punchcutter_core::gpu::passthrough_pipeline(
                &self.device,
                &layout,
                entry,
                entry,
                spirv(entry)?,
                workgroup,
            ))
        };
        let passes = Passes {
            shape: make("reel_shape", (64, 1, 1))?,
            typeset: make("reel_typeset", (64, 1, 1))?,
            view: make("reel_view", (1, 1, 1))?,
            frame: make("reel_frame", (8, 8, 1))?,
        };
        if let Some(err) = pollster::block_on(scope.pop()) {
            bail!("a reel pipeline was rejected: {err}");
        }
        Ok(passes)
    }

    fn uniform(&self, label: &str, bytes: &[u8]) -> wgpu::Buffer {
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, bytes);
        buffer
    }

    /// A storage buffer holding `bytes`, or `size` bytes of zeroes; never
    /// empty, since an empty buffer is not bindable.
    fn storage(&self, label: &str, bytes: &[u8], size: u64) -> wgpu::Buffer {
        let size = size.max(bytes.len() as u64).max(16).next_multiple_of(4);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        if !bytes.is_empty() {
            self.queue.write_buffer(&buffer, 0, bytes);
        }
        buffer
    }

    fn groups(&self, b: &Buffers) -> (wgpu::BindGroup, wgpu::BindGroup) {
        let set0 = [
            &b.params,
            &b.requests,
            &b.ot,
            &b.utf8,
            &b.out_glyphs,
            &b.out_runs,
            &b.frame,
            &b.blocks,
            &b.runs,
            &b.placed,
            &b.lines,
            &b.block_out,
            &b.scene,
            &b.image,
            &b.view,
        ];
        let entries: Vec<_> = set0
            .iter()
            .enumerate()
            .map(|(i, buffer)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        let zero = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reel-set0"),
            layout: &self.layout,
            entries: &entries,
        });
        let entries: Vec<_> = b
            .curves
            .iter()
            .enumerate()
            .map(|(i, buffer)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        let one = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reel-set1"),
            layout: &self.font_layout,
            entries: &entries,
        });
        (zero, one)
    }
}
