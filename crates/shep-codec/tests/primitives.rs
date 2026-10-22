//! Built-in impls: scalars, strings, bytes, options, tuples, arrays, and
//! collections. Golden-byte tests pin the wire format.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use shep_codec::{Decode, DecodeError, Decoder, from_bytes, to_bytes};
use support::{decode, reencode, round_trip};

#[test]
fn scalars_are_fixed_width_little_endian() {
    assert_eq!(round_trip(&0xabu8)[..], [0xab]);
    assert_eq!(round_trip(&0x0102u16)[..], [0x02, 0x01]);
    assert_eq!(round_trip(&0x0102_0304u32)[..], [0x04, 0x03, 0x02, 0x01]);
    assert_eq!(
        round_trip(&0x0102_0304_0506_0708u64)[..],
        [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
    );
    assert_eq!(round_trip(&-1i8)[..], [0xff]);
    assert_eq!(round_trip(&-2i16)[..], [0xfe, 0xff]);
    assert_eq!(round_trip(&-2i32)[..], [0xfe, 0xff, 0xff, 0xff]);
    assert_eq!(round_trip(&i64::MIN)[..], [0, 0, 0, 0, 0, 0, 0, 0x80]);
    assert_eq!(round_trip(&1.0f32)[..], [0x00, 0x00, 0x80, 0x3f]);
    assert_eq!(round_trip(&-2.5f64)[..], (-2.5f64).to_le_bytes());
}

#[test]
fn scalar_extremes_round_trip() {
    for v in [0, 1, u64::MAX] {
        round_trip(&v);
    }
    for v in [i32::MIN, -1, 0, i32::MAX] {
        round_trip(&v);
    }
    for v in [u16::MIN, u16::MAX] {
        round_trip(&v);
    }
    for v in [i8::MIN, i8::MAX] {
        round_trip(&v);
    }
    for v in [f64::MIN, f64::MAX, f64::INFINITY, -0.0] {
        round_trip(&v);
    }
    // NaN != NaN, so compare bits.
    let nan = f64::from_bits(0x7ff8_0000_0000_0001);
    let back: f64 = reencode(&nan).unwrap();
    assert_eq!(back.to_bits(), nan.to_bits());
}

#[test]
fn bool_is_one_byte_zero_or_one() {
    assert_eq!(round_trip(&false)[..], [0]);
    assert_eq!(round_trip(&true)[..], [1]);
    // Deviation from redpanda/src/v/serde/rw/scalar.h, which reads any
    // nonzero byte as true.
    for b in 2..=u8::MAX {
        assert_eq!(decode::<bool>(&[b]), Err(DecodeError::InvalidBool(b)));
    }
}

#[test]
fn string_is_length_then_utf8() {
    assert_eq!(round_trip(&String::new())[..], [0, 0, 0, 0]);
    assert_eq!(round_trip(&"hi".to_string())[..], [2, 0, 0, 0, b'h', b'i']);
    // The length counts bytes, not chars.
    assert_eq!(round_trip(&"é".to_string())[..], [2, 0, 0, 0, 0xc3, 0xa9]);
    // `str` and `&str` encode the same way.
    assert_eq!(to_bytes("hi"), to_bytes(&"hi".to_string()));
    assert_eq!(to_bytes(&"hi"), to_bytes(&"hi".to_string()));
}

#[test]
fn string_rejects_invalid_utf8() {
    assert_eq!(
        decode::<String>(&[1, 0, 0, 0, 0xff]),
        Err(DecodeError::InvalidUtf8)
    );
    // A truncated multi-byte sequence.
    assert_eq!(
        decode::<String>(&[1, 0, 0, 0, 0xc3]),
        Err(DecodeError::InvalidUtf8)
    );
    // Lone surrogate encoded as UTF-8.
    assert_eq!(
        decode::<String>(&[3, 0, 0, 0, 0xed, 0xa0, 0x80]),
        Err(DecodeError::InvalidUtf8)
    );
}

// Adapted from serde_bytes_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn bytes_is_length_then_raw() {
    assert_eq!(round_trip(&Bytes::new())[..], [0, 0, 0, 0]);
    assert_eq!(
        round_trip(&Bytes::from_static(&[0, 1, 0xff]))[..],
        [3, 0, 0, 0, 0, 1, 0xff]
    );
    // Same wire format as a Vec<u8>.
    let v = vec![9u8, 8, 7];
    assert_eq!(to_bytes(&v), to_bytes(&Bytes::from(v.clone())));
    let back: Vec<u8> = reencode(&Bytes::from(v.clone())).unwrap();
    assert_eq!(back, v);
}

#[test]
fn bytes_decode_is_zero_copy() {
    let input = to_bytes(&(7u8, Bytes::from_static(b"payload"), 9u8));
    let range = input.as_ptr() as usize..input.as_ptr() as usize + input.len();
    let (a, payload, b): (u8, Bytes, u8) = from_bytes(input.clone()).unwrap();
    assert_eq!((a, &payload[..], b), (7, &b"payload"[..], 9));
    // The decoded bytes point into the input's buffer.
    assert!(range.contains(&(payload.as_ptr() as usize)));
    assert_eq!(payload.as_ptr() as usize, range.start + 1 + 4);
}

#[test]
fn option_is_flag_then_value() {
    assert_eq!(round_trip(&None::<u32>)[..], [0]);
    assert_eq!(round_trip(&Some(5u8))[..], [1, 5]);
    assert_eq!(round_trip(&Some(None::<u8>))[..], [1, 0]);
    assert_eq!(round_trip(&Some(Some(3u8)))[..], [1, 1, 3]);
    assert_eq!(
        decode::<Option<u8>>(&[2, 5]),
        Err(DecodeError::InvalidBool(2))
    );
}

// Adapted from vector_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn vec_is_count_then_elements() {
    assert_eq!(round_trip(&Vec::<u64>::new())[..], [0, 0, 0, 0]);
    assert_eq!(
        round_trip(&vec![1i32, 2, 3])[..],
        [3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]
    );
    round_trip(&vec![
        vec!["a".to_string()],
        vec![],
        vec!["b".into(), "cd".into()],
    ]);
    // A slice encodes like a Vec.
    assert_eq!(to_bytes(&[1u16, 2][..]), to_bytes(&vec![1u16, 2]));
}

// Adapted from array_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn arrays_have_no_prefix() {
    assert_eq!(
        round_trip(&[1i32, 2, 3])[..],
        [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]
    );
    assert_eq!(round_trip(&[0u8; 0])[..], []);
    round_trip(&[[1u8, 2], [3, 4]]);
    round_trip(&["x".to_string(), "yz".to_string()]);
}

// Adapted from pair_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn tuples_are_fields_in_order() {
    assert_eq!(round_trip(&(42u8,))[..], [42]);
    assert_eq!(round_trip(&(1u8, 2u16))[..], [1, 2, 0]);
    round_trip(&("hello".to_string(), 456i32));
    round_trip(&("test".to_string(), vec![1i32, 2, 3]));
    round_trip(&((10i32, 20i32), "nested".to_string()));
    round_trip(&vec![
        ("one".to_string(), 1i32),
        ("two".to_string(), 2),
        ("three".to_string(), 3),
    ]);
    round_trip(&(
        1u8, 2u16, 3u32, 4u64, 5i8, 6i16, 7i32, 8i64, true, b'x', 0u8, 1u8,
    ));
}

#[test]
fn smart_pointers_encode_as_inner() {
    assert_eq!(round_trip(&Box::new(7u16))[..], [7, 0]);
    assert_eq!(
        round_trip(&Arc::new("a".to_string()))[..],
        [1, 0, 0, 0, b'a']
    );
    let boxed: Box<str> = "hi".into();
    assert_eq!(to_bytes(&boxed), to_bytes("hi"));
}

// Adapted from map_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn maps_are_count_then_entries() {
    let m = BTreeMap::from([(1u8, "a".to_string()), (2, "bc".to_string())]);
    assert_eq!(
        round_trip(&m)[..],
        [2, 0, 0, 0, 1, 1, 0, 0, 0, b'a', 2, 2, 0, 0, 0, b'b', b'c']
    );
    round_trip(&BTreeMap::<String, i32>::new());
    round_trip(&BTreeMap::from([
        ("asdf".to_string(), 33i32),
        ("fooo".to_string(), 44),
    ]));

    let h = HashMap::from([(123i32, 32i32), (456, 66), (959, 11)]);
    round_trip(&h);
    round_trip(&HashMap::from([
        ("asdf".to_string(), 33i32),
        ("fooo".to_string(), 44),
    ]));
}

// Adapted from set_test in redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn sets_are_count_then_items() {
    let s = BTreeSet::from([3u16, 1, 2]);
    assert_eq!(round_trip(&s)[..], [3, 0, 0, 0, 1, 0, 2, 0, 3, 0]);
    round_trip(&BTreeSet::from(["asdf".to_string(), "fooo".to_string()]));
    round_trip(&HashSet::from([123i32, 456, 959]));
    round_trip(&HashSet::from(["asdf".to_string(), "fooo".to_string()]));
}

// Adapted from collections_interop in redpanda/src/v/serde/test/serde_test.cc:
// collections with the same elements share a wire format.
#[test]
fn collections_interoperate() {
    let pairs = vec![(1u32, "a".to_string()), (2, "b".to_string())];
    let as_map: BTreeMap<u32, String> = reencode(&pairs).unwrap();
    assert_eq!(to_bytes(&as_map), to_bytes(&pairs));
    let as_hash: HashMap<u32, String> = reencode(&as_map).unwrap();
    assert_eq!(as_hash, pairs.iter().cloned().collect());

    let items = vec![5u64, 7, 9];
    let as_set: BTreeSet<u64> = reencode(&items).unwrap();
    assert_eq!(to_bytes(&as_set), to_bytes(&items));
    let as_hash: HashSet<u64> = reencode(&as_set).unwrap();
    let back: BTreeSet<u64> = reencode(&as_hash).unwrap();
    assert_eq!(back, as_set);
}

/// A repeated key would silently drop data, so it's rejected (Redpanda keeps
/// the first entry).
#[test]
fn duplicate_entries_are_rejected() {
    let dup_keys = vec![(1u8, 1u8), (1, 2)];
    assert!(matches!(
        reencode::<_, BTreeMap<u8, u8>>(&dup_keys),
        Err(DecodeError::DuplicateEntry { .. })
    ));
    assert!(matches!(
        reencode::<_, HashMap<u8, u8>>(&dup_keys),
        Err(DecodeError::DuplicateEntry { .. })
    ));
    let dup_items = vec![4u8, 4];
    assert!(matches!(
        reencode::<_, BTreeSet<u8>>(&dup_items),
        Err(DecodeError::DuplicateEntry { .. })
    ));
    assert!(matches!(
        reencode::<_, HashSet<u8>>(&dup_items),
        Err(DecodeError::DuplicateEntry { .. })
    ));
}

#[test]
fn truncation_reports_what_was_needed() {
    assert_eq!(
        decode::<u32>(&[1, 2]),
        Err(DecodeError::UnexpectedEnd {
            needed: 4,
            remaining: 2
        })
    );
    assert_eq!(
        decode::<Bytes>(&[5, 0, 0, 0, 1]),
        Err(DecodeError::UnexpectedEnd {
            needed: 5,
            remaining: 1
        })
    );
}

#[test]
fn from_bytes_rejects_trailing_bytes() {
    assert_eq!(decode::<u8>(&[1, 2, 3]), Err(DecodeError::TrailingBytes(2)));
    // `decode` itself stops after one value.
    let mut d = Decoder::new(Bytes::from_static(&[1, 2, 3]));
    assert_eq!(u8::decode(&mut d), Ok(1));
    assert_eq!(&d.rest()[..], [2, 3]);
}
