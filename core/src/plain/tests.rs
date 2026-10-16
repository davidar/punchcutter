use super::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pair {
    a: u32,
    b: f32,
}

// SAFETY: repr(C), two 4-byte leaves, no padding, every bit pattern valid.
unsafe impl Plain for Pair {}

#[test]
fn a_value_and_a_slice_agree_on_their_bytes() {
    let pair = Pair { a: 7, b: 1.5 };
    assert_eq!(bytes_of(&pair).len(), 8);
    let run = [pair, Pair { a: 9, b: -1.0 }];
    assert_eq!(bytes_of_slice(&run).len(), 16);
    assert_eq!(&bytes_of_slice(&run)[8..], bytes_of(&run[1]));
}

#[test]
fn a_write_needs_exactly_the_right_length() {
    let mut dst = Pair { a: 0, b: 0.0 };
    let src = Pair { a: 3, b: 2.5 };
    assert!(write_from_bytes(&mut dst, bytes_of(&src)));
    assert_eq!(dst, src);
    assert!(!write_from_bytes(&mut dst, &[0u8; 4]));
    assert_eq!(dst, src, "a rejected write leaves the value alone");
}
