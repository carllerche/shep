//! Decoding arbitrary bytes, as a peer or a corrupt disk might hand them over,
//! as every type in a matrix of shapes.
//!
//! For any input and every type, decoding never panics, and if it succeeds:
//!
//! - the bytes it read decode on their own to the same value;
//! - the value re-encodes to `encoded_len` bytes and decodes back to itself;
//! - a type with only one encoding per value re-encodes to exactly the bytes
//!   it was read from;
//! - every strict prefix of the bytes it was read from fails to decode.
//!
//! Inputs come three ways: raw bytes; bytes built from the format's own
//! pieces (length prefixes and envelopes, honest or lying, around arbitrary
//! contents), which reach deep into nested types; and valid encodings of
//! sample values with a few mutations.
//!
//! Decoding runs under a small memory limit, so types whose elements encode
//! to nothing hit the limit quickly instead of looping.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;

use bytes::Bytes;
use hegel::TestCase;
use hegel::generators as gs;
use shep_codec::{
    Decode, DecodeError, Decoder, Encode, Limits, from_bytes, from_bytes_with, to_bytes,
};

const LIMITS: Limits = Limits {
    max_memory: 64 << 10,
};

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 3, compat = 1)]
struct Record {
    id: u64,
    flag: bool,
    name: String,
    payload: Bytes,
    tags: BTreeMap<String, Vec<u16>>,
    set: BTreeSet<i64>,
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

/// Encodes to nothing.
#[derive(Encode, Decode, Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[codec(transparent)]
struct Nothing([u8; 0]);

#[derive(Encode, Decode, Debug, Clone, Default, PartialEq)]
struct ZeroByteFields {
    a: [u8; 0],
    b: Nothing,
    c: ([u8; 0], Nothing),
    #[codec(default)]
    d: Option<u8>,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
struct Empty {}

/// Collections of things that encode to nothing.
#[derive(Encode, Decode, Debug, Clone, PartialEq)]
enum ZeroByteElements {
    Vec(Vec<Nothing>),
    Nested(Vec<Vec<[u8; 0]>>),
    Set(BTreeSet<Nothing>),
    Map(BTreeMap<Nothing, ZeroByteFields>),
    Boxed(Vec<Option<Box<Nothing>>>),
}

#[derive(Encode, Decode, Debug, Clone, PartialEq)]
#[codec(version = 1)]
struct Shared {
    name: Arc<String>,
    hashed: HashMap<u16, HashSet<u8>>,
}

/// Whether `T` has exactly one encoding per value. Envelopes don't (a reader
/// accepts other versions and skips unknown trailing fields), nor do hash
/// collections (entries go in iteration order).
trait Canonical {
    const CANONICAL: bool;
}

macro_rules! canonical {
    ($c:expr => $($t:ty),* $(,)?) => {$(
        impl Canonical for $t {
            const CANONICAL: bool = $c;
        }
    )*};
}

canonical!(true =>
    u8, u64, i16, bool, String, Bytes, Vec<u8>, Vec<String>, Option<u32>,
    (u8, bool, Option<i16>), [u16; 3], [u8; 0], Vec<[u8; 0]>,
    Option<Box<Vec<Bytes>>>, Id, Nothing,
);
canonical!(false =>
    Record, Event, ZeroByteFields, Empty, ZeroByteElements, Shared,
    BTreeMap<String, Vec<u8>>, BTreeSet<u32>, HashMap<u16, bool>,
);

/// Decodes `input` as a `T` and checks what a successful decode promises.
fn check<T>(input: &Bytes)
where
    T: Encode + Decode + Canonical + PartialEq + Debug,
{
    let mut d = Decoder::with_limits(input.clone(), LIMITS);
    let Ok(value) = T::decode(&mut d) else {
        return;
    };
    let read = input.slice(..input.len() - d.remaining());
    let type_name = std::any::type_name::<T>();

    // What it read stands alone: nothing it consumed belongs to what follows.
    assert_eq!(
        from_bytes_with::<T>(read.clone(), LIMITS).as_ref(),
        Ok(&value),
        "{type_name}: the bytes read don't decode alone"
    );

    let encoded = to_bytes(&value);
    assert_eq!(value.encoded_len(), encoded.len(), "{type_name}: {value:?}");
    assert_eq!(
        from_bytes_with::<T>(encoded.clone(), LIMITS).as_ref(),
        Ok(&value),
        "{type_name} doesn't round-trip"
    );
    if T::CANONICAL {
        assert_eq!(encoded, read, "{type_name}: {value:?} has two encodings");
    }
    for n in 0..read.len() {
        let prefix = from_bytes_with::<T>(read.slice(..n), LIMITS);
        assert!(prefix.is_err(), "{type_name}: {n}-byte prefix decoded");
    }
}

fn check_all(input: &Bytes) {
    check::<u8>(input);
    check::<u64>(input);
    check::<i16>(input);
    check::<bool>(input);
    check::<String>(input);
    check::<Bytes>(input);
    check::<Vec<u8>>(input);
    check::<Vec<String>>(input);
    check::<Option<u32>>(input);
    check::<(u8, bool, Option<i16>)>(input);
    check::<[u16; 3]>(input);
    check::<[u8; 0]>(input);
    check::<Vec<[u8; 0]>>(input);
    check::<Option<Box<Vec<Bytes>>>>(input);
    check::<Id>(input);
    check::<Nothing>(input);
    check::<Record>(input);
    check::<Event>(input);
    check::<ZeroByteFields>(input);
    check::<Empty>(input);
    check::<ZeroByteElements>(input);
    check::<Shared>(input);
    check::<BTreeMap<String, Vec<u8>>>(input);
    check::<BTreeSet<u32>>(input);
    check::<HashMap<u16, bool>>(input);
}

#[hegel::test(test_cases = 300)]
fn raw_bytes(tc: TestCase) {
    let input = tc.draw(gs::binary().max_size(64));
    check_all(&Bytes::from(input));
}

/// Bytes built from the format's pieces.
#[hegel::test_helper]
fn wire(tc: &TestCase, depth: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let pieces = tc.draw(gs::integers::<usize>().max_value(4));
    for _ in 0..pieces {
        let max_kind = if depth == 0 { 3 } else { 5 };
        let kind = tc.draw(gs::integers::<u8>().max_value(max_kind));
        match kind {
            0 => {
                let raw = tc.draw(gs::binary().max_size(8));
                out.extend(raw);
            }
            1 => {
                let byte = tc.draw(gs::sampled_from(vec![0u8, 1, 2, 3, 200, 0xff]));
                out.push(byte);
            }
            // A count or length, small or hostile.
            2 | 3 => {
                let len = tc.draw(gs::sampled_from(vec![
                    0u32,
                    1,
                    2,
                    3,
                    7,
                    0xff,
                    0x0100_0000,
                    u32::MAX,
                ]));
                out.extend(len.to_le_bytes());
            }
            // A length prefix around contents, honest or not.
            4 => {
                let inner = wire(tc, depth - 1);
                let honest = tc.draw(gs::weighted_booleans(0.8));
                let len = if honest {
                    inner.len() as u32
                } else {
                    tc.draw(gs::integers::<u32>())
                };
                out.extend(len.to_le_bytes());
                out.extend(inner);
            }
            // An envelope: versions, size, body.
            _ => {
                let version = tc.draw(gs::integers::<u8>().max_value(4));
                let compat = tc.draw(gs::integers::<u8>().max_value(4));
                let body = wire(tc, depth - 1);
                let honest = tc.draw(gs::weighted_booleans(0.8));
                let size = if honest {
                    body.len() as u32
                } else {
                    tc.draw(gs::integers::<u32>())
                };
                out.extend([version, compat]);
                out.extend(size.to_le_bytes());
                out.extend(body);
            }
        }
    }
    out
}

#[hegel::test(test_cases = 300)]
fn structured_bytes(tc: TestCase) {
    let input = wire(&tc, 4);
    check_all(&Bytes::from(input));
}

fn samples() -> Vec<Bytes> {
    let leaf = Record {
        id: 9,
        flag: true,
        name: "leaf".into(),
        payload: Bytes::from_static(b"xyz"),
        tags: BTreeMap::from([("t".into(), vec![1, 2])]),
        set: BTreeSet::from([-1, 5]),
        pair: (3, Some(-4)),
        arr: [1, 2, 3],
        events: vec![Event::Ping],
        nested: None,
        extra: Some(Id(7)),
    };
    let root = Record {
        name: "root".into(),
        events: vec![
            Event::Data(Bytes::from_static(b"d")),
            Event::Move {
                from: (1, 2),
                to: (-3, 4),
            },
            Event::Batch(vec![Event::Ping, Event::Batch(vec![])]),
        ],
        nested: Some(Box::new(leaf.clone())),
        extra: None,
        ..leaf.clone()
    };
    vec![
        to_bytes(&root),
        to_bytes(&leaf),
        to_bytes(&ZeroByteFields::default()),
        to_bytes(&ZeroByteElements::Map(BTreeMap::from([(
            Nothing::default(),
            ZeroByteFields::default(),
        )]))),
        to_bytes(&ZeroByteElements::Nested(vec![vec![[]; 3]; 2])),
        to_bytes(&Shared {
            name: Arc::new("s".into()),
            hashed: HashMap::from([(1, HashSet::from([2, 3]))]),
        }),
        to_bytes(&vec!["a".to_string(), "bc".into()]),
        to_bytes(&BTreeMap::from([("k".to_string(), vec![1u8, 2])])),
    ]
}

#[hegel::test(test_cases = 300)]
fn mutated_encodings(tc: TestCase) {
    let samples = samples();
    let pick = tc.draw(gs::integers::<usize>().max_value(samples.len() - 1));
    let mut input = samples[pick].to_vec();
    let mutations = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    for _ in 0..mutations {
        let kind = tc.draw(gs::integers::<u8>().max_value(4));
        let at = tc.draw(gs::integers::<usize>().max_value(input.len()));
        match kind {
            0 if at < input.len() => {
                let byte = tc.draw(gs::integers::<u8>());
                input[at] = byte;
            }
            1 => {
                let byte = tc.draw(gs::integers::<u8>());
                input.insert(at, byte);
            }
            2 if at < input.len() => {
                input.remove(at);
            }
            3 => input.truncate(at),
            // Overwrite four bytes with a hostile length.
            _ => {
                let len = tc.draw(gs::sampled_from(vec![0u32, 1, 0xffff, u32::MAX]));
                for (i, b) in len.to_le_bytes().into_iter().enumerate() {
                    if let Some(slot) = input.get_mut(at + i) {
                        *slot = b;
                    }
                }
            }
        }
    }
    check_all(&Bytes::from(input));
}

/// An envelope is read exactly when its versions say this definition may
/// read it: its compat version is at most ours (3), and its version at least
/// our compat version (1).
#[hegel::test(test_cases = 200)]
fn envelope_versions(tc: TestCase) {
    let mut input = to_bytes(&Record {
        id: 1,
        flag: false,
        name: String::new(),
        payload: Bytes::new(),
        tags: BTreeMap::new(),
        set: BTreeSet::new(),
        pair: (0, None),
        arr: [0; 3],
        events: vec![],
        nested: None,
        extra: None,
    })
    .to_vec();
    let version = tc.draw(gs::integers::<u8>());
    let compat = tc.draw(gs::integers::<u8>());
    input[0] = version;
    input[1] = compat;
    let decoded = from_bytes_with::<Record>(Bytes::from(input), LIMITS);
    let readable = compat <= 3 && version >= 1;
    match decoded {
        Ok(_) => assert!(readable, "read data at version {version}, compat {compat}"),
        Err(DecodeError::IncompatibleVersion { .. }) => {
            assert!(!readable, "refused version {version}, compat {compat}")
        }
        Err(e) => panic!("{e}"),
    }
}

/// Four bytes claiming `u32::MAX` elements that encode to nothing are
/// rejected at the limit, wherever they're nested, rather than looped over.
#[test]
fn zero_byte_elements_are_bounded() {
    fn at_limit<T: Decode + Debug>(input: &[u8]) {
        assert_eq!(
            from_bytes_with::<T>(Bytes::copy_from_slice(input), LIMITS).map(|_| ()),
            Err(DecodeError::LimitExceeded {
                limit: LIMITS.max_memory
            }),
            "{}",
            std::any::type_name::<T>()
        );
    }
    let count = u32::MAX.to_le_bytes();
    at_limit::<Vec<[u8; 0]>>(&count);
    at_limit::<Vec<Nothing>>(&count);
    // Inside an enum's envelope: version, compat, size, tag, then the count.
    let mut enveloped = vec![0, 0, 5, 0, 0, 0, 0];
    enveloped.extend(count);
    at_limit::<ZeroByteElements>(&enveloped);
}

/// The budget is shared by everything in one decode: two strings that each
/// fit don't both fit.
#[test]
fn the_budget_spans_the_whole_value() {
    let half = "x".repeat(LIMITS.max_memory / 2 + 1);
    let pair = to_bytes(&(half.clone(), half.clone()));
    assert_eq!(
        from_bytes_with::<(String, String)>(pair.clone(), LIMITS),
        Err(DecodeError::LimitExceeded {
            limit: LIMITS.max_memory
        })
    );
    assert_eq!(
        from_bytes::<(String, String)>(pair),
        Ok((half.clone(), half))
    );
}
