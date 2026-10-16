//! Outlines as runs of quadratic Béziers: the one shape the band builder and
//! the coverage walk understand.
//!
//! Every producer lands here. A TrueType `glyf` glyph is already quadratic, so
//! it passes straight through; a CFF glyph, like a caller's own vector path
//! (an SVG's, say), is cubic, and cubics are split. Straight lines become quadratics with the second endpoint
//! duplicated — `{p1, p2, p2}` — which is what the Slug repository's own notes
//! recommend, and which keeps `a == b` in the root solver rather than landing on
//! its near-linear branch.

/// One quadratic Bézier: `C(t) = (1-t)^2 p0 + 2t(1-t) p1 + t^2 p2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub p0: [f32; 2],
    pub p1: [f32; 2],
    pub p2: [f32; 2],
}

impl Quad {
    /// The largest x over the three control points. The horizontal band sort
    /// key, and the early-out test in the walk.
    pub fn max_x(&self) -> f32 {
        self.p0[0].max(self.p1[0]).max(self.p2[0])
    }

    /// The largest y over the three control points — the vertical band's key.
    pub fn max_y(&self) -> f32 {
        self.p0[1].max(self.p1[1]).max(self.p2[1])
    }

    pub fn min_x(&self) -> f32 {
        self.p0[0].min(self.p1[0]).min(self.p2[0])
    }

    pub fn min_y(&self) -> f32 {
        self.p0[1].min(self.p1[1]).min(self.p2[1])
    }

    /// Every control point shares a y, so no ray parallel to x can cross this
    /// curve transversally and it must be kept out of horizontal bands. The
    /// Slug notes are explicit about this: such a curve cannot contribute to
    /// the winding number, and including it only costs a solve whose
    /// discriminant is zero.
    pub fn is_horizontal(&self) -> bool {
        self.p0[1] == self.p1[1] && self.p1[1] == self.p2[1]
    }

    /// The same, for vertical bands.
    pub fn is_vertical(&self) -> bool {
        self.p0[0] == self.p1[0] && self.p1[0] == self.p2[0]
    }

    /// A curve with no extent at all — a zero-length closing segment, which
    /// `glyf` and SVG both produce. It contributes nothing and is dropped.
    pub fn is_degenerate(&self) -> bool {
        self.p0 == self.p1 && self.p1 == self.p2
    }
}

/// A closed contour: quadratics end-to-end, each starting where the last
/// finished, the last ending where the first began.
#[derive(Clone, Debug, Default)]
pub struct Contour {
    pub quads: Vec<Quad>,
}

/// How finely a cubic is approximated, in em units. One part in 4096 of an em
/// is a fortieth of a pixel at 96 px, which is an order of magnitude finer than
/// any text size worth drawing; the cost is curves, and only for CFF glyphs and
/// a caller's own cubic outlines, since `glyf` never reaches the splitter.
const CUBIC_TOLERANCE: f32 = 1.0 / 4096.0;

/// Most pieces one cubic may become. A cubic needing more than this is a
/// pathological S-curve, and at 16 pieces the residual error is already below
/// a thousandth of an em for anything a glyph contains.
const CUBIC_MAX_PIECES: u32 = 16;

/// Collects `move_to`/`line_to`/`quad_to`/`curve_to`/`close` into [`Contour`]s,
/// in whatever space the caller's `map` puts them.
///
/// `map` exists because producers disagree about units and about which way y
/// points: a font hands over font units with y up, an SVG path hands over
/// user units with y down, and the buffers want em units with y up in every
/// case.
pub struct Builder<F> {
    map: F,
    contours: Vec<Contour>,
    current: Contour,
    start: [f32; 2],
    cursor: [f32; 2],
    open: bool,
}

impl<F: Fn(f32, f32) -> [f32; 2]> Builder<F> {
    pub fn new(map: F) -> Self {
        Builder {
            map,
            contours: Vec::new(),
            current: Contour::default(),
            start: [0.0, 0.0],
            cursor: [0.0, 0.0],
            open: false,
        }
    }

    /// Finish, closing any contour the source left open — `glyf` always closes,
    /// SVG does not have to, and an unclosed filled path is filled as if it
    /// were closed.
    pub fn finish(mut self) -> Vec<Contour> {
        self.end_contour();
        self.contours
    }

    fn end_contour(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        if self.cursor != self.start {
            self.push(Quad {
                p0: self.cursor,
                p1: self.start,
                p2: self.start,
            });
            self.cursor = self.start;
        }
        let done = std::mem::take(&mut self.current);
        if !done.quads.is_empty() {
            self.contours.push(done);
        }
    }

    fn push(&mut self, quad: Quad) {
        if quad.is_degenerate() {
            return;
        }
        self.current.quads.push(quad);
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.end_contour();
        self.start = (self.map)(x, y);
        self.cursor = self.start;
        self.open = true;
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        let p = (self.map)(x, y);
        // `{p1, p2, p2}`, per the Slug repository's notes on encoding a line.
        self.push(Quad {
            p0: self.cursor,
            p1: p,
            p2: p,
        });
        self.cursor = p;
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let c = (self.map)(cx, cy);
        let p = (self.map)(x, y);
        self.push(Quad {
            p0: self.cursor,
            p1: c,
            p2: p,
        });
        self.cursor = p;
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        let c1 = (self.map)(c1x, c1y);
        let c2 = (self.map)(c2x, c2y);
        let p = (self.map)(x, y);
        let p0 = self.cursor;
        for quad in split_cubic(p0, c1, c2, p) {
            self.push(quad);
        }
        self.cursor = p;
    }

    pub fn close(&mut self) {
        self.end_contour();
    }
}

/// A cubic as a run of quadratics, within [`CUBIC_TOLERANCE`].
///
/// The single best quadratic approximation of a cubic takes the control point
/// `(3c1 + 3c2 - p0 - p3)/4`, and its error is bounded by `sqrt(3)/36` times
/// the length of `p3 - 3c2 + 3c1 - p0`. Halving the parameter range divides
/// that vector by eight, so splitting into `n` equal pieces divides the error
/// by `n^3` — which is the `cbrt` below, and why even a tolerance this tight
/// costs two or three pieces rather than dozens.
pub fn split_cubic(p0: [f32; 2], c1: [f32; 2], c2: [f32; 2], p3: [f32; 2]) -> Vec<Quad> {
    let d = [
        p3[0] - 3.0 * c2[0] + 3.0 * c1[0] - p0[0],
        p3[1] - 3.0 * c2[1] + 3.0 * c1[1] - p0[1],
    ];
    let err = (3.0f32).sqrt() / 36.0 * (d[0] * d[0] + d[1] * d[1]).sqrt();
    let pieces = if err <= CUBIC_TOLERANCE {
        1
    } else {
        ((err / CUBIC_TOLERANCE).cbrt().ceil() as u32).clamp(1, CUBIC_MAX_PIECES)
    };

    let mut out = Vec::with_capacity(pieces as usize);
    let step = 1.0 / pieces as f32;
    for i in 0..pieces {
        let t0 = i as f32 * step;
        let t1 = (i + 1) as f32 * step;
        let (s0, s1, s2, s3) = cubic_segment(p0, c1, c2, p3, t0, t1);
        // The midpoint-control quadratic for this piece.
        let ctrl = [
            (3.0 * s1[0] + 3.0 * s2[0] - s0[0] - s3[0]) * 0.25,
            (3.0 * s1[1] + 3.0 * s2[1] - s0[1] - s3[1]) * 0.25,
        ];
        out.push(Quad {
            p0: s0,
            p1: ctrl,
            p2: s3,
        });
    }
    out
}

/// The sub-cubic over `[t0, t1]`, by the usual Bézier restriction formulas.
fn cubic_segment(
    p0: [f32; 2],
    c1: [f32; 2],
    c2: [f32; 2],
    p3: [f32; 2],
    t0: f32,
    t1: f32,
) -> ([f32; 2], [f32; 2], [f32; 2], [f32; 2]) {
    let at = |t: f32| cubic_at(p0, c1, c2, p3, t);
    let tangent = |t: f32| {
        let u = 1.0 - t;
        [
            3.0 * (u * u * (c1[0] - p0[0])
                + 2.0 * u * t * (c2[0] - c1[0])
                + t * t * (p3[0] - c2[0])),
            3.0 * (u * u * (c1[1] - p0[1])
                + 2.0 * u * t * (c2[1] - c1[1])
                + t * t * (p3[1] - c2[1])),
        ]
    };
    let dt = (t1 - t0) / 3.0;
    let a = at(t0);
    let b = at(t1);
    let da = tangent(t0);
    let db = tangent(t1);
    (
        a,
        [a[0] + da[0] * dt, a[1] + da[1] * dt],
        [b[0] - db[0] * dt, b[1] - db[1] * dt],
        b,
    )
}

fn cubic_at(p0: [f32; 2], c1: [f32; 2], c2: [f32; 2], p3: [f32; 2], t: f32) -> [f32; 2] {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    [
        a * p0[0] + b * c1[0] + c * c2[0] + d * p3[0],
        a * p0[1] + b * c1[1] + c * c2[1] + d * p3[1],
    ]
}

/// The [`ttf_parser::OutlineBuilder`] face of [`Builder`]. Kept separate so a
/// caller driving the same builder from its own path segments does not have
/// to pretend to be a font.
pub struct FontOutline<F> {
    pub builder: Builder<F>,
}

impl<F: Fn(f32, f32) -> [f32; 2]> ttf_parser::OutlineBuilder for FontOutline<F> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

#[cfg(test)]
mod tests;
