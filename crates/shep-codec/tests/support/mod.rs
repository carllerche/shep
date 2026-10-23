#![allow(dead_code)]

use std::fmt::Debug;

use shep_codec::{Decode, DecodeError, Decoder, Encode, from_bytes, to_bytes};

/// Encodes `value`, checks the invariants every encoding must satisfy, and
/// returns the bytes:
///
/// - `encoded_len` is exact;
/// - it decodes back to `value`;
/// - `decode` consumes exactly the encoding and leaves what follows;
/// - every strict prefix fails to decode (the format is self-delimiting), and
///   doing so doesn't panic.
pub fn round_trip<T>(value: &T) -> Vec<u8>
where
    T: Encode + Decode + PartialEq + Debug,
{
    let bytes = to_bytes(value);
    assert_eq!(value.encoded_len(), bytes.len(), "encoded_len of {value:?}");
    assert_eq!(&from_bytes::<T>(&bytes).unwrap(), value);

    let mut followed = Vec::from(&bytes[..]);
    followed.extend_from_slice(b"rest");
    let mut d = Decoder::new(&followed);
    assert_eq!(&T::decode(&mut d).unwrap(), value);
    assert_eq!(d.rest(), b"rest");

    assert_prefixes_fail::<T>(&bytes);
    bytes
}

/// Every strict prefix of `bytes` fails to decode as a `T`.
pub fn assert_prefixes_fail<T: Decode + Debug>(bytes: &[u8]) {
    for n in 0..bytes.len() {
        let result = from_bytes::<T>(&bytes[..n]);
        assert!(
            result.is_err(),
            "prefix of {n}/{} bytes decoded: {result:?}",
            bytes.len()
        );
    }
}

/// Encodes `value` and decodes the bytes as a `U`.
pub fn reencode<T: Encode + ?Sized, U: Decode>(value: &T) -> Result<U, DecodeError> {
    from_bytes(&to_bytes(value))
}

pub fn decode<T: Decode>(bytes: &[u8]) -> Result<T, DecodeError> {
    from_bytes(bytes)
}
