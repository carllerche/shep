//! Derived structs: the envelope, version checks, field evolution, transparent
//! newtypes, and generics.

mod support;

use std::collections::BTreeMap;

use bytes::{Bytes, BytesMut};
use shep_codec::{Decode, DecodeError, Encode, from_bytes, to_bytes};
use support::{decode, reencode, round_trip};

// The test_msg* family from redpanda/src/v/serde/test/serde_test.cc.

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
#[codec(version = 1, compat = 0)]
struct TestMsg0 {
    i: i8,
    j: i8,
}

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
#[codec(version = 4, compat = 0)]
struct TestMsg1 {
    a: i32,
    m: TestMsg0,
    b: i32,
    c: i32,
}

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
#[codec(version = 5, compat = 5)]
struct TestMsg1Incompatible {
    m: TestMsg0,
}

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
#[codec(version = 10, compat = 5)]
struct TestMsg1New {
    a: i32,
    m: TestMsg0,
    b: i32,
    c: i32,
}

fn msg1() -> TestMsg1 {
    TestMsg1 {
        a: 55,
        m: TestMsg0 {
            i: b'i' as i8,
            j: b'j' as i8,
        },
        b: 33,
        c: 44,
    }
}

#[test]
fn envelope_golden_bytes() {
    let bytes = round_trip(&TestMsg0 { i: 1, j: -1 });
    #[rustfmt::skip]
    assert_eq!(bytes[..], [
        1, 0,          // version, compat version
        2, 0, 0, 0,    // body size
        1, 0xff,       // i, j
    ]);
}

// Adapted from simple_envelope_test and envelope_test in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn nested_envelopes_round_trip() {
    let bytes = round_trip(&msg1());
    #[rustfmt::skip]
    assert_eq!(bytes[..], [
        4, 0, 20, 0, 0, 0,
        55, 0, 0, 0,
        1, 0, 2, 0, 0, 0, b'i', b'j',
        33, 0, 0, 0,
        44, 0, 0, 0,
    ]);
}

// Adapted from incompatible_version_throws in
// redpanda/src/v/serde/test/serde_test.cc: the data's version (4) is below
// the reader's compat version (5).
#[test]
fn rejects_data_older_than_reader_compat_version() {
    assert_eq!(
        reencode::<_, TestMsg1Incompatible>(&msg1()),
        Err(DecodeError::IncompatibleVersion {
            type_name: std::any::type_name::<TestMsg1Incompatible>(),
            version: 4,
            compat_version: 0,
            reader_version: 5,
            reader_compat_version: 5,
        })
    );
}

// Adapted from envelope_test_version_older_than_compat_version in
// redpanda/src/v/serde/test/serde_test.cc: the data's compat version (5) is
// above the reader's version (4).
#[test]
fn rejects_data_requiring_a_newer_reader() {
    let new = TestMsg1New {
        a: 55,
        m: TestMsg0 { i: 1, j: 2 },
        b: 33,
        c: 44,
    };
    assert_eq!(
        reencode::<_, TestMsg1>(&new),
        Err(DecodeError::IncompatibleVersion {
            type_name: std::any::type_name::<TestMsg1>(),
            version: 10,
            compat_version: 5,
            reader_version: 4,
            reader_compat_version: 0,
        })
    );
    // The boundaries are inclusive: compat == reader version and
    // version == reader compat are both readable.
    let mut bytes = BytesMut::from(&to_bytes(&TestMsg0 { i: 1, j: 2 })[..]);
    bytes[0] = 9; // version
    bytes[1] = 1; // compat version == TestMsg0's version
    assert_eq!(
        from_bytes::<TestMsg0>(bytes.freeze()),
        Ok(TestMsg0 { i: 1, j: 2 })
    );
}

// Adapted from envelope_test_buffer_too_short in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn rejects_envelope_shorter_than_its_size() {
    let bytes = to_bytes(&msg1());
    assert!(matches!(
        from_bytes::<TestMsg1>(bytes.slice(..bytes.len() - 1)),
        Err(DecodeError::UnexpectedEnd { .. })
    ));
}

// `small`/`big` from redpanda/src/v/serde/test/serde_test.cc. Redpanda's `big`
// gives `d` an in-class initializer of 0x1234; here that's a `Default`.

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
struct Small {
    a: i32,
    b: i32,
    c: i32,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1)]
struct Big {
    a: i32,
    b: i32,
    c: i32,
    #[codec(default)]
    d: D,
}

#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq)]
#[codec(transparent)]
struct D(i32);

impl Default for D {
    fn default() -> D {
        D(0x1234)
    }
}

/// Like `Big`, but `d` has no default.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1)]
struct BigRequired {
    a: i32,
    b: i32,
    c: i32,
    d: i32,
}

// Adapted from compat_test_added_field in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn new_reader_defaults_missing_field() {
    let big: Big = reencode(&Small { a: 1, b: 2, c: 3 }).unwrap();
    assert_eq!(
        big,
        Big {
            a: 1,
            b: 2,
            c: 3,
            d: D(0x1234)
        }
    );
}

// Adapted from compat_test_added_field_vector in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn new_reader_defaults_missing_field_in_vec() {
    let old = vec![Small { a: 1, b: 2, c: 3 }, Small { a: 4, b: 5, c: 6 }];
    let new: Vec<Big> = reencode(&old).unwrap();
    assert_eq!(new.len(), 2);
    assert_eq!(
        (new[0].a, new[0].b, new[0].c, new[0].d),
        (1, 2, 3, D(0x1234))
    );
    assert_eq!(
        (new[1].a, new[1].b, new[1].c, new[1].d),
        (4, 5, 6, D(0x1234))
    );
}

/// Unlike Redpanda, a field missing from old data is an error unless it's
/// marked `#[codec(default)]`.
#[test]
fn new_reader_rejects_missing_required_field() {
    assert_eq!(
        reencode::<_, BigRequired>(&Small { a: 1, b: 2, c: 3 }),
        Err(DecodeError::MissingField {
            type_name: std::any::type_name::<BigRequired>(),
            field: "d",
        })
    );
}

// Adapted from compat_test_removed_field in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn old_reader_skips_unknown_fields() {
    let big = Big {
        a: 1,
        b: 2,
        c: 3,
        d: D(4),
    };
    assert_eq!(reencode(&big), Ok(Small { a: 1, b: 2, c: 3 }));
}

// Adapted from compat_test_removed_field_vector in
// redpanda/src/v/serde/test/serde_test.cc: skipping must land exactly at the
// next element.
#[test]
fn old_reader_skips_unknown_fields_in_vec() {
    let new = vec![
        Big {
            a: 1,
            b: 2,
            c: 3,
            d: D(0x77),
        },
        Big {
            a: 123,
            b: 456,
            c: 789,
            d: D(0x77),
        },
    ];
    let old: Vec<Small> = reencode(&new).unwrap();
    assert_eq!(
        old,
        vec![
            Small { a: 1, b: 2, c: 3 },
            Small {
                a: 123,
                b: 456,
                c: 789
            }
        ]
    );
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1)]
struct HalfField1 {
    a: i32,
    b: i32,
    c: i16,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1)]
struct HalfField2 {
    a: i32,
    b: i32,
    c: i32,
    d: i16,
}

// Adapted from compat_test_half_field_1 in
// redpanda/src/v/serde/test/serde_test.cc: a field can't be read from part of
// another field's bytes, even if the envelope's size would allow it.
#[test]
fn field_cannot_straddle_the_envelope_end() {
    let half = HalfField1 { a: 1, b: 2, c: 3 };
    assert!(matches!(
        reencode::<_, Small>(&half),
        Err(DecodeError::UnexpectedEnd { .. })
    ));
}

// Adapted from compat_test_half_field_2 in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn partial_trailing_field_is_skipped_or_rejected() {
    let half = HalfField2 {
        a: 1,
        b: 2,
        c: 3,
        d: 0x1234,
    };
    // Small stops before `d` and skips it.
    assert_eq!(reencode(&half), Ok(Small { a: 1, b: 2, c: 3 }));
    // Big tries to read an i32 `d` from two bytes.
    assert!(matches!(
        reencode::<_, Big>(&half),
        Err(DecodeError::UnexpectedEnd { .. })
    ));
}

// Adapted from complex_msg_test in redpanda/src/v/serde/test/serde_test.cc.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1, compat = 1)]
struct InnerDifferingSizes {
    ints: Vec<i32>,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 3, compat = 3)]
struct ComplexMsg {
    vec: Vec<InnerDifferingSizes>,
    x: i32,
}

#[test]
fn envelopes_of_differing_sizes_in_a_vec() {
    let small = InnerDifferingSizes {
        ints: vec![1, 2, 3],
    };
    let big = InnerDifferingSizes {
        ints: vec![4; u8::MAX as usize + 1],
    };
    let msg = ComplexMsg {
        vec: [&small, &big, &small, &big, &small, &big]
            .into_iter()
            .cloned()
            .collect(),
        x: 3,
    };
    let bytes = to_bytes(&msg);
    assert_eq!(msg.encoded_len(), bytes.len());
    assert_eq!(from_bytes::<ComplexMsg>(bytes), Ok(msg));
}

/// Data from a newer, still-compatible writer: fields both know are read and
/// the rest skipped, even when the extra fields are envelopes themselves.
#[test]
fn compatible_newer_version_is_read() {
    #[derive(Encode, Decode, Debug, PartialEq)]
    #[codec(version = 1)]
    struct V1 {
        id: u64,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    #[codec(version = 2, compat = 1)]
    struct V2 {
        id: u64,
        #[codec(default)]
        tags: BTreeMap<String, TestMsg0>,
        #[codec(default)]
        note: Option<String>,
    }

    let v2 = V2 {
        id: 9,
        tags: BTreeMap::from([("t".to_string(), TestMsg0 { i: 1, j: 2 })]),
        note: Some("n".into()),
    };
    assert_eq!(reencode(&v2), Ok(V1 { id: 9 }));
    assert_eq!(
        reencode(&V1 { id: 9 }),
        Ok(V2 {
            id: 9,
            tags: BTreeMap::new(),
            note: None
        })
    );
    // The skipped bytes don't leak into whatever follows.
    let pair: (V1, u8) = reencode(&(v2, 0xaau8)).unwrap();
    assert_eq!(pair, (V1 { id: 9 }, 0xaa));
}

#[test]
fn body_size_bounds_field_decoding() {
    // TestMsg0 claims a 1-byte body: `j` is missing and the byte after the
    // envelope isn't read as `j`.
    assert_eq!(
        decode::<TestMsg0>(&[1, 0, 1, 0, 0, 0, 7, 8]),
        Err(DecodeError::MissingField {
            type_name: std::any::type_name::<TestMsg0>(),
            field: "j",
        })
    );
    // A size beyond the input.
    assert_eq!(
        decode::<TestMsg0>(&[1, 0, 0xff, 0xff, 0xff, 0xff, 7, 8]),
        Err(DecodeError::UnexpectedEnd {
            needed: u32::MAX as usize,
            remaining: 2
        })
    );
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
struct Unit;

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
struct Empty {}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 3, compat = 2)]
struct Tuple(u8, String, #[codec(default)] Option<u16>);

#[test]
fn unit_and_tuple_structs() {
    assert_eq!(round_trip(&Unit)[..], [0, 0, 0, 0, 0, 0]);
    assert_eq!(round_trip(&Empty {})[..], [0, 0, 0, 0, 0, 0]);
    assert_eq!(
        round_trip(&Tuple(7, "a".into(), Some(1)))[..],
        [3, 2, 9, 0, 0, 0, 7, 1, 0, 0, 0, b'a', 1, 1, 0]
    );
    // Old data without the third field.
    assert_eq!(
        decode::<Tuple>(&[3, 2, 6, 0, 0, 0, 7, 1, 0, 0, 0, b'a']),
        Ok(Tuple(7, "a".into(), None))
    );
    // Missing tuple fields are named by index.
    assert_eq!(
        decode::<Tuple>(&[3, 2, 1, 0, 0, 0, 7]),
        Err(DecodeError::MissingField {
            type_name: std::any::type_name::<Tuple>(),
            field: "1",
        })
    );
    // A unit struct skips whatever a newer writer put in its body.
    assert_eq!(decode::<Unit>(&[1, 0, 2, 0, 0, 0, 1, 2]), Ok(Unit));
}

#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[codec(transparent)]
struct NodeId(u64);

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(transparent)]
struct Name {
    value: String,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(transparent)]
struct Wrapper<T>(T);

// Adapted from the named_type case of all_types_test in
// redpanda/src/v/serde/test/serde_test.cc.
#[test]
fn transparent_encodes_as_inner() {
    assert_eq!(round_trip(&NodeId(123)), to_bytes(&123u64));
    assert_eq!(round_trip(&Name { value: "n".into() }), to_bytes("n"));
    assert_eq!(round_trip(&Wrapper(vec![1u8])), to_bytes(&vec![1u8]));
    assert_eq!(
        round_trip(&Wrapper(TestMsg0 { i: 1, j: 2 })),
        to_bytes(&TestMsg0 { i: 1, j: 2 })
    );
    round_trip(&BTreeMap::from([(NodeId(1), Name { value: "a".into() })]));
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 2, compat = 1)]
struct Pair<A, B> {
    a: A,
    b: Vec<B>,
}

/// `T` has no `Default`; the derive bounds only the defaulted field's type.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
struct WithDefaultGeneric<T> {
    t: T,
    #[codec(default)]
    more: Vec<T>,
    #[codec(default)]
    maybe: Option<T>,
}

#[test]
fn generic_structs() {
    round_trip(&Pair {
        a: 1u8,
        b: vec!["x".to_string()],
    });
    round_trip(&Pair {
        a: Pair {
            a: NodeId(1),
            b: vec![Some(2u16), None],
        },
        b: vec![Wrapper(Pair::<u8, u8> { a: 1, b: vec![] })],
    });
    round_trip(&Wrapper(Pair {
        a: (1u8, "s".to_string()),
        b: vec![BTreeMap::from([(1u8, Bytes::from_static(b"v"))])],
    }));

    round_trip(&WithDefaultGeneric {
        t: NodeId(1),
        more: vec![NodeId(2)],
        maybe: Some(NodeId(3)),
    });
    // Old data with only `t`.
    let only_t = {
        let mut b = BytesMut::from(&[0u8, 0, 8, 0, 0, 0][..]);
        NodeId(5).encode(&mut b);
        b.freeze()
    };
    assert_eq!(
        from_bytes::<WithDefaultGeneric<NodeId>>(only_t),
        Ok(WithDefaultGeneric {
            t: NodeId(5),
            more: vec![],
            maybe: None
        })
    );
}

/// Encode works for borrowed data too.
#[test]
fn borrowed_fields_encode() {
    #[derive(Encode)]
    struct Borrowed<'a> {
        name: &'a str,
        items: &'a [u32],
    }

    #[derive(Decode, Debug, PartialEq)]
    struct Owned {
        name: String,
        items: Vec<u32>,
    }

    let items = [1, 2];
    let borrowed = Borrowed {
        name: "b",
        items: &items,
    };
    let bytes = to_bytes(&borrowed);
    assert_eq!(borrowed.encoded_len(), bytes.len());
    assert_eq!(
        from_bytes::<Owned>(bytes),
        Ok(Owned {
            name: "b".into(),
            items: vec![1, 2]
        })
    );
}

/// Hand-written impls compose with derived ones.
#[test]
fn custom_impls_compose() {
    // Adapted from custom_read_write_test in
    // redpanda/src/v/serde/test/serde_test.cc.
    #[derive(Debug, PartialEq)]
    struct Plus2(u32);

    impl Encode for Plus2 {
        fn encode(&self, buf: &mut BytesMut) {
            (self.0 + 1).encode(buf);
        }
    }

    impl Decode for Plus2 {
        fn decode(buf: &mut Bytes) -> Result<Self, DecodeError> {
            let v = u32::decode(buf)?;
            if v == 0 {
                return Err(DecodeError::Custom("zero".into()));
            }
            Ok(Plus2(v + 1))
        }
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Holder {
        x: Plus2,
    }

    let holder = Holder { x: Plus2(123) };
    // The default `encoded_len` encodes into scratch space.
    assert_eq!(holder.encoded_len(), 6 + 4);
    assert_eq!(reencode(&holder), Ok(Holder { x: Plus2(125) }));
    assert_eq!(
        decode::<Holder>(&[0, 0, 4, 0, 0, 0, 0, 0, 0, 0]),
        Err(DecodeError::Custom("zero".into()))
    );
}

/// Fields that encode to nothing: a body with nothing left still holds them,
/// so they aren't missing.
#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
struct ZeroByteFields {
    a: [u8; 0],
    b: u32,
    c: [u64; 0],
    d: ([u8; 0],),
}

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
#[codec(transparent)]
struct Nothing([u8; 0]);

#[derive(Encode, Decode, Debug, Default, Clone, PartialEq)]
struct OnlyNothing {
    n: Nothing,
}

#[test]
fn zero_byte_fields_round_trip() {
    round_trip(&ZeroByteFields {
        b: 7,
        ..Default::default()
    });
    round_trip(&OnlyNothing::default());
    // A field that does take bytes is still missing from an empty body.
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Empty {}
    #[derive(Encode, Decode, Debug, PartialEq)]
    struct Later {
        a: [u8; 0],
        b: u32,
    }
    assert!(matches!(
        from_bytes::<Later>(to_bytes(&Empty {})),
        Err(DecodeError::MissingField { field: "b", .. })
    ));
}
