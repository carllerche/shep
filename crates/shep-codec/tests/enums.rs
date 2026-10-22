//! Derived enums: tags, variant shapes, unknown tags, and evolution.

mod support;

use shep_codec::{Decode, DecodeError, Encode, to_bytes};
use support::{decode, reencode, round_trip};

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
enum Shape {
    Unit,
    Tuple(u8, String),
    Struct {
        x: u16,
        #[codec(default)]
        y: Option<u32>,
    },
}

#[test]
fn variant_golden_bytes() {
    #[rustfmt::skip]
    assert_eq!(round_trip(&Shape::Unit)[..], [
        0, 0, 1, 0, 0, 0,   // envelope: version, compat, size
        0,                  // tag
    ]);
    #[rustfmt::skip]
    assert_eq!(round_trip(&Shape::Tuple(9, "a".into()))[..], [
        0, 0, 7, 0, 0, 0,
        1,
        9,
        1, 0, 0, 0, b'a',
    ]);
    #[rustfmt::skip]
    assert_eq!(round_trip(&Shape::Struct { x: 3, y: Some(4) })[..], [
        0, 0, 8, 0, 0, 0,
        2,
        3, 0,
        1, 4, 0, 0, 0,
    ]);
    round_trip(&Shape::Struct { x: 3, y: None });
}

#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq)]
#[codec(version = 2, compat = 1)]
enum Op {
    #[codec(tag = 10)]
    Put,
    #[codec(tag = 3)]
    Delete,
    /// No attribute: its declaration index.
    Get,
}

#[test]
fn explicit_tags() {
    assert_eq!(round_trip(&Op::Put)[..], [2, 1, 1, 0, 0, 0, 10]);
    assert_eq!(round_trip(&Op::Delete)[..], [2, 1, 1, 0, 0, 0, 3]);
    assert_eq!(round_trip(&Op::Get)[..], [2, 1, 1, 0, 0, 0, 2]);
}

#[test]
fn unknown_tag_is_an_error() {
    for tag in [0, 1, 4, 9, 11, 255] {
        assert_eq!(
            decode::<Op>(&[2, 1, 1, 0, 0, 0, tag]),
            Err(DecodeError::UnknownTag {
                type_name: std::any::type_name::<Op>(),
                tag
            })
        );
    }
    // No tag at all.
    assert!(matches!(
        decode::<Op>(&[2, 1, 0, 0, 0, 0]),
        Err(DecodeError::UnexpectedEnd { .. })
    ));
}

#[test]
fn enum_versions_are_checked() {
    // Data compat 3 > reader version 2.
    assert!(matches!(
        decode::<Op>(&[3, 3, 1, 0, 0, 0, 10]),
        Err(DecodeError::IncompatibleVersion { .. })
    ));
    // Data version 0 < reader compat 1.
    assert!(matches!(
        decode::<Op>(&[0, 0, 1, 0, 0, 0, 10]),
        Err(DecodeError::IncompatibleVersion { .. })
    ));
    // A newer, compatible writer's extra variant data is skipped.
    assert_eq!(
        decode::<Op>(&[5, 2, 3, 0, 0, 0, 10, 0xaa, 0xbb]),
        Ok(Op::Put)
    );
}

mod old {
    use super::*;

    #[derive(Encode, Decode, Debug, Clone, PartialEq)]
    pub enum Event {
        Created(u64),
        Deleted { id: u64 },
    }
}

mod new {
    use super::*;

    /// `old::Event` with a field appended to a variant and a variant appended.
    #[derive(Encode, Decode, Debug, Clone, PartialEq)]
    pub enum Event {
        Created(u64, #[codec(default)] Option<String>),
        Deleted {
            id: u64,
            #[codec(default)]
            reason: String,
        },
        Renamed {
            id: u64,
            name: String,
        },
    }
}

// Adapted from variant_append_compat in
// redpanda/src/v/serde/test/serde_test.cc (Redpanda's serde::variant is
// positional; derived enums carry the same append-only guarantees).
#[test]
fn appending_variants_and_fields() {
    // Old reader, old writer.
    round_trip(&old::Event::Created(1));
    // New reader, old writer: missing fields default.
    assert_eq!(
        reencode(&old::Event::Created(1)),
        Ok(new::Event::Created(1, None))
    );
    assert_eq!(
        reencode(&old::Event::Deleted { id: 2 }),
        Ok(new::Event::Deleted {
            id: 2,
            reason: String::new()
        })
    );
    // Old reader, new writer, known variant: extra fields skipped.
    assert_eq!(
        reencode(&new::Event::Created(1, Some("x".into()))),
        Ok(old::Event::Created(1))
    );
    assert_eq!(
        reencode(&new::Event::Deleted {
            id: 2,
            reason: "r".into()
        }),
        Ok(old::Event::Deleted { id: 2 })
    );
    // Old reader, new writer, appended variant: unknown tag.
    assert_eq!(
        reencode::<_, old::Event>(&new::Event::Renamed {
            id: 3,
            name: "n".into()
        }),
        Err(DecodeError::UnknownTag {
            type_name: std::any::type_name::<old::Event>(),
            tag: 2
        })
    );
    // New reader, new writer.
    round_trip(&new::Event::Renamed {
        id: 3,
        name: "n".into(),
    });
    // An unknown variant inside a Vec fails the whole Vec, but a known one
    // after skipped fields leaves the stream aligned.
    let mixed = vec![
        new::Event::Created(1, Some("x".into())),
        new::Event::Deleted {
            id: 2,
            reason: "r".into(),
        },
    ];
    assert_eq!(
        reencode(&mixed),
        Ok(vec![old::Event::Created(1), old::Event::Deleted { id: 2 }])
    );
}

#[test]
fn missing_variant_field_is_named() {
    // Renamed with only `id`.
    let mut bytes = vec![0, 0, 9, 0, 0, 0, 2];
    bytes.extend_from_slice(&7u64.to_le_bytes());
    assert_eq!(
        decode::<new::Event>(&bytes),
        Err(DecodeError::MissingField {
            type_name: std::any::type_name::<new::Event>(),
            field: "Renamed.name",
        })
    );
    // Tuple variant fields are named by index.
    assert_eq!(
        decode::<new::Event>(&[0, 0, 1, 0, 0, 0, 0]),
        Err(DecodeError::MissingField {
            type_name: std::any::type_name::<new::Event>(),
            field: "Created.0",
        })
    );
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
enum Either<L, R> {
    Left(L),
    Right(R),
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
enum Tree<T> {
    Leaf(T),
    Node(Box<Tree<T>>, Box<Tree<T>>),
}

#[test]
fn generic_and_recursive_enums() {
    round_trip(&Either::<u8, String>::Left(1));
    round_trip(&Either::<u8, String>::Right("r".into()));
    round_trip(&Either::<Either<u8, u16>, Vec<Shape>>::Left(Either::Right(
        3,
    )));
    round_trip(&Either::<Either<u8, u16>, Vec<Shape>>::Right(vec![
        Shape::Unit,
        Shape::Tuple(1, "t".into()),
    ]));

    let tree = Tree::Node(
        Box::new(Tree::Leaf("a".to_string())),
        Box::new(Tree::Node(
            Box::new(Tree::Leaf("b".into())),
            Box::new(Tree::Leaf("c".into())),
        )),
    );
    round_trip(&tree);
}

#[derive(Encode, Decode, Debug, PartialEq)]
enum Never {}

#[test]
fn empty_enum_decodes_nothing() {
    assert_eq!(
        decode::<Never>(&[0, 0, 1, 0, 0, 0, 0]),
        Err(DecodeError::UnknownTag {
            type_name: std::any::type_name::<Never>(),
            tag: 0
        })
    );
    // Encode exists, so it can be used as a type parameter.
    assert_eq!(
        to_bytes(&Either::<Never, u8>::Right(1)),
        to_bytes(&Either::<u8, u8>::Right(1))
    );
}

#[test]
fn enum_encoded_len_is_exact() {
    for shape in [
        Shape::Unit,
        Shape::Tuple(0, "abc".into()),
        Shape::Struct { x: 1, y: None },
    ] {
        assert_eq!(shape.encoded_len(), to_bytes(&shape).len());
    }
}
