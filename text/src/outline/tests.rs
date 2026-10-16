use super::*;

fn identity(x: f32, y: f32) -> [f32; 2] {
    [x, y]
}

#[test]
fn a_line_duplicates_its_endpoint() {
    let mut b = Builder::new(identity);
    b.move_to(0.0, 0.0);
    b.line_to(1.0, 2.0);
    b.close();
    let contours = b.finish();
    assert_eq!(contours.len(), 1);
    // Two quads: out and back, each with the endpoint duplicated.
    assert_eq!(
        contours[0].quads[0],
        Quad {
            p0: [0.0, 0.0],
            p1: [1.0, 2.0],
            p2: [1.0, 2.0]
        }
    );
    assert_eq!(
        contours[0].quads[1],
        Quad {
            p0: [1.0, 2.0],
            p1: [0.0, 0.0],
            p2: [0.0, 0.0]
        }
    );
}

#[test]
fn contours_are_end_to_end_and_closed() {
    let mut b = Builder::new(identity);
    b.move_to(0.0, 0.0);
    b.quad_to(1.0, 1.0, 2.0, 0.0);
    b.line_to(2.0, 2.0);
    // No `close()`: an open filled path is filled as if closed.
    let contours = b.finish();
    assert_eq!(contours.len(), 1);
    let quads = &contours[0].quads;
    for pair in quads.windows(2) {
        assert_eq!(pair[0].p2, pair[1].p0);
    }
    assert_eq!(quads.last().unwrap().p2, quads[0].p0);
}

#[test]
fn degenerate_segments_are_dropped() {
    let mut b = Builder::new(identity);
    b.move_to(1.0, 1.0);
    b.line_to(1.0, 1.0);
    b.line_to(2.0, 1.0);
    b.close();
    let contours = b.finish();
    assert_eq!(contours[0].quads.len(), 2);
}

/// A quarter circle as a cubic, against the real thing: the split must stay
/// inside the tolerance it claims.
#[test]
fn a_split_cubic_stays_within_tolerance() {
    let k = 0.5522847;
    let (p0, c1, c2, p3) = ([1.0, 0.0], [1.0, k], [k, 1.0], [0.0, 1.0]);
    let quads = split_cubic(p0, c1, c2, p3);
    assert!(!quads.is_empty());
    // End-to-end, and spanning the whole cubic.
    assert_eq!(quads[0].p0, p0);
    assert_eq!(quads.last().unwrap().p2, p3);
    for pair in quads.windows(2) {
        assert_eq!(pair[0].p2, pair[1].p0);
    }
    // Sample the cubic and find the nearest point on the chain. The chain's
    // sampling has to be an order finer than the tolerance, or what the test
    // measures is its own step size: at 200 samples per piece the spacing is
    // 0.0016 em and the "error" came out at half that, three times the real
    // deviation.
    let mut worst = 0.0f32;
    for i in 0..=256 {
        let t = i as f32 / 256.0;
        let want = cubic_at(p0, c1, c2, p3, t);
        let mut best = f32::INFINITY;
        for quad in &quads {
            for j in 0..=4096 {
                let s = j as f32 / 4096.0;
                let u = 1.0 - s;
                let got = [
                    u * u * quad.p0[0] + 2.0 * u * s * quad.p1[0] + s * s * quad.p2[0],
                    u * u * quad.p0[1] + 2.0 * u * s * quad.p1[1] + s * s * quad.p2[1],
                ];
                let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
                best = best.min(d);
            }
        }
        worst = worst.max(best);
    }
    assert!(
        worst < CUBIC_TOLERANCE * 2.0,
        "worst deviation {worst} exceeds twice the tolerance"
    );
}

/// A line is already exact, so the splitter must not waste curves on one.
#[test]
fn a_straight_cubic_is_one_quad() {
    let quads = split_cubic([0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]);
    assert_eq!(quads.len(), 1);
}
