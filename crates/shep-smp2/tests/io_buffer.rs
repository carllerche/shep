#![forbid(unsafe_code)]
use shep_smp2::io::Buffer;
#[test]
fn buffer_alignment_move_resize_and_initialized_bytes() {
    for alignment in [1, 2, 8, 64, 4096] {
        for len in [0, 1, 7, 4096] {
            let mut b = Buffer::new(len, alignment).unwrap();
            assert_eq!(b.as_ptr().addr() % alignment, 0);
            assert!(b.iter().all(|b| *b == 0));
            let ptr = b.as_ptr();
            b.resize(0);
            let mut moved = b;
            moved.resize(len);
            assert_eq!(moved.as_ptr(), ptr);
            assert!(moved.iter().all(|b| *b == 0));
        }
    }
}
#[test]
fn buffer_rejects_bad_layout_and_zeroes_reexposed_bytes() {
    assert!(Buffer::new(1, 0).is_err());
    assert!(Buffer::new(1, 3).is_err());
    assert!(Buffer::new(usize::MAX, 1).is_err());
    let mut b = Buffer::from_slice(b"abcd", 8).unwrap();
    b.resize(1);
    b.resize(4);
    assert_eq!(&*b, b"a\0\0\0");
}
