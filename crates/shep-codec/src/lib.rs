//! A versioned binary serialization format, copied from Redpanda's `serde`.
//!
//! Values encode compactly and without a schema: the reader must know the
//! type. Structs and enums derived with [`Encode`] and [`Decode`] are wrapped
//! in an *envelope* that carries a version and a size, so a type can grow new
//! trailing fields and old and new binaries still read each other's data.
//!
//! # Wire format
//!
//! All integers are fixed-width little-endian; there are no varints.
//!
//! | Type | Encoding |
//! |------|----------|
//! | `u8`..`u64`, `i8`..`i64`, `f32`, `f64` | fixed-width little-endian |
//! | `bool` | `u8` 0 or 1; anything else is rejected |
//! | `String` | `u32` byte length, then the bytes (UTF-8) |
//! | `Vec`, `Box<[T]>`, `BTreeMap`, `BTreeSet`, `HashMap`, `HashSet` | `u32` element count, then the elements (map entries as key, value); for bytes, the count and then the bytes |
//! | `Option<T>` | `u8` flag 0 or 1, then the value if 1 |
//! | tuples, `[T; N]` | the elements in order, no prefix |
//! | `Box<T>`, `Arc<T>` | as `T` |
//! | derived struct | envelope: `u8` version, `u8` compat version, `u32` body size, then the fields in declaration order |
//! | derived enum | envelope whose body is a `u8` variant tag, then the variant's fields |
//! | `#[codec(transparent)]` newtype | as its one field, no envelope |
//!
//! # Evolution
//!
//! A struct's attribute `#[codec(version = V, compat = C)]` (both default to
//! 0) says: this definition writes version `V`, and its data can be read by
//! any definition whose version is at least `C`. A reader rejects data whose
//! compat version is above its own version, or whose version is below its own
//! compat version (exactly Redpanda's rule).
//!
//! Within an accepted envelope, fields decode in order while body bytes
//! remain:
//!
//! - **New reader, old data.** Fields the writer didn't know about are
//!   missing. A missing field marked `#[codec(default)]` takes its
//!   [`Default`]; any other missing field is a [`DecodeError::MissingField`].
//! - **Old reader, new data.** Body bytes left after the reader's last field
//!   belong to fields it doesn't know about, and are skipped.
//!
//! So the only compatible change is appending fields (with
//! `#[codec(default)]` if old data must stay readable). Removing, reordering,
//! or retyping a field needs a version bump with `compat` raised past the old
//! version, which makes old and new definitions reject each other's data
//! instead of misreading it.
//!
//! Enum variants are identified by tag: the declaration index, or
//! `#[codec(tag = N)]`. Adding a variant is compatible for readers that never
//! see it; a reader that does see an unknown tag fails with
//! [`DecodeError::UnknownTag`]. Variant fields evolve like struct fields.
//!
//! # Robustness
//!
//! Decoding never panics, whatever the input, and is bounded in what it
//! allocates. Every decode runs on a [`Decoder`] that carries the frame's
//! remaining budget ([`Limits`]): containers charge what they allocate before
//! allocating it, and fail with [`DecodeError::LimitExceeded`] past the
//! limit, wherever in the frame they're nested. A length prefix is also
//! checked against the bytes that remain before memory is reserved. Nesting
//! of derived types is limited to [`MAX_DEPTH`] so recursive types can't
//! overflow the stack.
//!
//! Encoding panics if a length or an envelope body exceeds `u32::MAX` bytes
//! (Redpanda throws in the same place).

mod decoder;
mod error;
mod impls;

#[doc(hidden)]
pub mod __private;

pub use decoder::{DEFAULT_MAX_MEMORY, Decoder, Limits};
pub use error::DecodeError;
pub use shep_codec_derive::{Decode, Encode};

/// How deeply derived types may nest when decoding.
///
/// Every derived struct or enum (including transparent ones) counts one
/// level. Data nested deeper fails with [`DecodeError::TooDeep`].
pub const MAX_DEPTH: usize = 128;

/// A type that can be written in the codec's wire format.
pub trait Encode {
    /// Appends the encoding of `self` to `buf`.
    ///
    /// # Panics
    ///
    /// If a length prefix or envelope size would exceed `u32::MAX`.
    fn encode(&self, buf: &mut Vec<u8>);

    /// The number of bytes [`encode`](Encode::encode) appends.
    ///
    /// The default encodes into a scratch buffer; every implementation in this
    /// crate, and every derived one, computes it without encoding.
    fn encoded_len(&self) -> usize {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        buf.len()
    }

    /// Appends the encodings of `items` in order. The default encodes them one
    /// by one; `u8` copies them in one go, so byte strings are fast.
    #[doc(hidden)]
    fn encode_slice(items: &[Self], buf: &mut Vec<u8>)
    where
        Self: Sized,
    {
        for item in items {
            item.encode(buf);
        }
    }
}

/// A type that can be read from the codec's wire format.
pub trait Decode: Sized {
    /// Reads one value from the front of `d`'s input, advancing past it, and
    /// charges `d` for what it allocates.
    ///
    /// On error, how far the input advanced is unspecified.
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError>;

    /// Reads `len` values in a row, charging `d` for each. The default decodes
    /// them one by one; `u8` copies them in one go, so byte strings are fast.
    #[doc(hidden)]
    fn decode_vec(d: &mut Decoder<'_>, len: usize) -> Result<Vec<Self>, DecodeError> {
        impls::decode_each(d, len)
    }
}

/// Encodes `value` into a buffer of exactly the right size.
pub fn to_bytes<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    let mut buf = Vec::with_capacity(value.encoded_len());
    value.encode(&mut buf);
    buf
}

/// Decodes a `T` that must span all of `bytes`, under the default [`Limits`].
pub fn from_bytes<T: Decode>(bytes: &[u8]) -> Result<T, DecodeError> {
    from_bytes_with(bytes, Limits::default())
}

/// Decodes a `T` that must span all of `bytes`, under `limits`.
pub fn from_bytes_with<T: Decode>(bytes: &[u8], limits: Limits) -> Result<T, DecodeError> {
    let mut d = Decoder::with_limits(bytes, limits);
    let value = T::decode(&mut d)?;
    if !d.is_empty() {
        return Err(DecodeError::TrailingBytes(d.remaining()));
    }
    Ok(value)
}
