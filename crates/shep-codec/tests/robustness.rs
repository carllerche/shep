//! Decoding arbitrary input never panics, and random values round-trip.
//!
//! The randomized tests play the role of redpanda/src/v/serde/test/fuzz.cc,
//! with a fixed seed so failures reproduce.

mod support;

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use shep_codec::{Decode, DecodeError, Decoder, Encode, MAX_DEPTH, from_bytes, to_bytes};
use support::round_trip;

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 3, compat = 1)]
struct Record {
    id: u64,
    flag: bool,
    delta: i32,
    name: String,
    payload: Bytes,
    tags: BTreeMap<String, Vec<u16>>,
    set: HashSet<i64>,
    pair: (u8, Option<i16>),
    arr: [u32; 3],
    events: Vec<Event>,
    nested: Option<Box<Record>>,
    #[codec(default)]
    extra: Option<Id>,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
enum Event {
    Ping,
    Data(Bytes),
    Move {
        from: (i8, i8),
        to: (i8, i8),
    },
    #[codec(tag = 200)]
    Batch(Vec<Event>),
}

#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq)]
#[codec(transparent)]
struct Id(u64);

fn string(rng: &mut StdRng) -> String {
    (0..rng.random_range(0..6))
        .map(|_| {
            if rng.random_bool(0.7) {
                rng.random_range('a'..='z')
            } else {
                rng.random::<char>()
            }
        })
        .collect()
}

fn bytes(rng: &mut StdRng) -> Bytes {
    (0..rng.random_range(0..8))
        .map(|_| rng.random::<u8>())
        .collect()
}

fn event(rng: &mut StdRng, depth: u32) -> Event {
    match rng.random_range(0..if depth == 0 { 3 } else { 4 }) {
        0 => Event::Ping,
        1 => Event::Data(bytes(rng)),
        2 => Event::Move {
            from: (rng.random(), rng.random()),
            to: (rng.random(), rng.random()),
        },
        _ => Event::Batch(
            (0..rng.random_range(0..3))
                .map(|_| event(rng, depth - 1))
                .collect(),
        ),
    }
}

fn record(rng: &mut StdRng, depth: u32) -> Record {
    Record {
        id: rng.random(),
        flag: rng.random(),
        delta: rng.random(),
        name: string(rng),
        payload: bytes(rng),
        tags: (0..rng.random_range(0..3))
            .map(|_| {
                let v = (0..rng.random_range(0..3)).map(|_| rng.random()).collect();
                (string(rng), v)
            })
            .collect(),
        set: (0..rng.random_range(0..4)).map(|_| rng.random()).collect(),
        pair: (rng.random(), rng.random_bool(0.5).then(|| rng.random())),
        arr: rng.random(),
        events: (0..rng.random_range(0..3)).map(|_| event(rng, 2)).collect(),
        nested: (depth > 0 && rng.random_bool(0.3)).then(|| Box::new(record(rng, depth - 1))),
        extra: rng.random_bool(0.5).then(|| Id(rng.random())),
    }
}

#[test]
fn random_values_round_trip() {
    let mut rng = StdRng::seed_from_u64(0x5e9_c0dec);
    for _ in 0..300 {
        // Checks encoded_len, decode, and that every prefix fails cleanly.
        round_trip(&record(&mut rng, 2));
    }
}

/// Corrupting valid encodings may decode to something else, but never panics.
#[test]
fn mutated_encodings_never_panic() {
    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..300 {
        let encoded = to_bytes(&record(&mut rng, 2));
        for _ in 0..20 {
            let mut corrupt = encoded.to_vec();
            for _ in 0..rng.random_range(1..4) {
                let at = rng.random_range(0..corrupt.len());
                corrupt[at] = match rng.random_range(0..3) {
                    0 => rng.random(),
                    1 => 0xff,
                    _ => corrupt[at].wrapping_add(1),
                };
            }
            let corrupt = Bytes::from(corrupt);
            let _ = from_bytes::<Record>(corrupt.clone());
            let _ = from_bytes::<Vec<Event>>(corrupt);
        }
    }
}

/// Random bytes decoded as assorted types never panic.
#[test]
fn garbage_never_panics() {
    let mut rng = StdRng::seed_from_u64(7);
    for _ in 0..5_000 {
        let len = rng.random_range(0..64);
        let garbage: Bytes = (0..len).map(|_| rng.random::<u8>()).collect();
        let _ = from_bytes::<Record>(garbage.clone());
        let _ = from_bytes::<Event>(garbage.clone());
        let _ = from_bytes::<String>(garbage.clone());
        let _ = from_bytes::<Vec<Option<bool>>>(garbage.clone());
        let _ = from_bytes::<BTreeMap<u8, String>>(garbage.clone());
        let _ = from_bytes::<HashSet<u16>>(garbage.clone());
        let _ = from_bytes::<(u8, [i16; 4], Bytes)>(garbage);
    }
}

/// A recursive type that costs one byte per level on the wire.
#[derive(Encode, Decode, Debug, PartialEq)]
#[codec(transparent)]
struct Chain(Option<Box<Chain>>);

fn chain_bytes(links: usize) -> Bytes {
    let mut bytes = vec![1u8; links];
    bytes.push(0);
    Bytes::from(bytes)
}

#[test]
fn nesting_is_limited() {
    // `links` ones nest `links + 1` Chains.
    assert!(from_bytes::<Chain>(chain_bytes(MAX_DEPTH - 1)).is_ok());
    assert_eq!(
        from_bytes::<Chain>(chain_bytes(MAX_DEPTH)),
        Err(DecodeError::TooDeep)
    );
    // Deep enough to overflow the stack without the limit.
    assert_eq!(
        from_bytes::<Chain>(chain_bytes(1_000_000)),
        Err(DecodeError::TooDeep)
    );
    // Failing releases every level: a maximal value still decodes after.
    assert!(from_bytes::<Chain>(chain_bytes(MAX_DEPTH - 1)).is_ok());

    // Enveloped types count too.
    let mut deep = Event::Ping;
    for _ in 0..MAX_DEPTH {
        deep = Event::Batch(vec![deep]);
    }
    assert_eq!(
        from_bytes::<Event>(to_bytes(&deep)),
        Err(DecodeError::TooDeep)
    );
    let Event::Batch(mut inner) = deep else {
        unreachable!()
    };
    let shallower = inner.pop().unwrap();
    assert_eq!(from_bytes::<Event>(to_bytes(&shallower)), Ok(shallower));
}

/// Siblings don't accumulate depth: only nesting does.
#[test]
fn breadth_is_not_depth() {
    let wide = Event::Batch(vec![Event::Ping; MAX_DEPTH * 4]);
    assert_eq!(from_bytes::<Event>(to_bytes(&wide)), Ok(wide));
}

#[test]
fn every_truncation_of_a_large_value_fails_cleanly() {
    let mut rng = StdRng::seed_from_u64(1);
    let value = record(&mut rng, 4);
    let mut bytes = to_bytes(&value);
    while !bytes.is_empty() {
        bytes = bytes.slice(..bytes.len() - 1);
        assert!(Record::decode(&mut Decoder::new(bytes.clone())).is_err());
    }
    assert_eq!(value.encoded_len(), to_bytes(&value).len());
}
