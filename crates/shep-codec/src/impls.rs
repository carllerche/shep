//! [`Encode`] and [`Decode`] for primitives and standard containers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash};
use std::mem::size_of;
use std::sync::Arc;

use crate::{Decode, DecodeError, Decoder, Encode};

/// Size of a length prefix.
const LEN_SIZE: usize = 4;

fn put_len(buf: &mut Vec<u8>, len: usize) {
    let len = u32::try_from(len).expect("shep-codec: length exceeds u32::MAX");
    buf.extend_from_slice(&len.to_le_bytes());
}

/// A byte string: its length, then the raw bytes.
fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    put_len(buf, bytes.len());
    buf.extend_from_slice(bytes);
}

fn get_len(d: &mut Decoder<'_>) -> Result<usize, DecodeError> {
    Ok(u32::decode(d)? as usize)
}

/// What one element of `T` in a collection is charged: its size, and at
/// least a byte, so elements that encode to nothing still use up the budget.
fn element_cost<T>() -> usize {
    size_of::<T>().max(1)
}

/// Capacity to reserve for `len` elements of `T`. A hostile prefix can claim
/// up to `u32::MAX` elements, so reserve no more memory than the remaining
/// input occupies or the budget allows; growth past that is paid for by
/// input actually consumed, and charged element by element.
fn cautious_capacity<T>(len: usize, d: &Decoder<'_>) -> usize {
    len.min(d.remaining().min(d.budget()) / element_cost::<T>())
}

macro_rules! scalar {
    ($($t:ty),*) => {$(
        impl Encode for $t {
            fn encode(&self, buf: &mut Vec<u8>) {
                buf.extend_from_slice(&self.to_le_bytes());
            }

            fn encoded_len(&self) -> usize {
                size_of::<$t>()
            }
        }

        impl Decode for $t {
            fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
                Ok(<$t>::from_le_bytes(d.take_array()?))
            }
        }
    )*};
}

scalar!(u16, u32, u64, i8, i16, i32, i64, f32, f64);

/// Bytes are scalars too, but a run of them is copied in one go.
impl Encode for u8 {
    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(*self);
    }

    fn encoded_len(&self) -> usize {
        1
    }

    fn encode_slice(items: &[u8], buf: &mut Vec<u8>) {
        buf.extend_from_slice(items);
    }
}

impl Decode for u8 {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(d.take_array::<1>()?[0])
    }

    fn decode_vec(d: &mut Decoder<'_>, len: usize) -> Result<Vec<u8>, DecodeError> {
        let bytes = d.take(len)?;
        d.charge(len)?;
        Ok(bytes.to_vec())
    }
}

impl Encode for bool {
    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(u8::from(*self));
    }

    fn encoded_len(&self) -> usize {
        1
    }
}

/// Unlike Redpanda, which reads any nonzero byte as `true`, only 0 and 1 are
/// accepted, so every value has exactly one encoding.
impl Decode for bool {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match u8::decode(d)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::InvalidBool(other)),
        }
    }
}

impl Encode for str {
    fn encode(&self, buf: &mut Vec<u8>) {
        put_bytes(buf, self.as_bytes());
    }

    fn encoded_len(&self) -> usize {
        LEN_SIZE + self.len()
    }
}

impl Encode for String {
    fn encode(&self, buf: &mut Vec<u8>) {
        self.as_str().encode(buf);
    }

    fn encoded_len(&self) -> usize {
        self.as_str().encoded_len()
    }
}

impl Decode for String {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        let bytes = d.take(len)?;
        let s = std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)?;
        d.charge(len)?;
        Ok(s.to_owned())
    }
}

impl<T: Encode> Encode for [T] {
    fn encode(&self, buf: &mut Vec<u8>) {
        put_len(buf, self.len());
        T::encode_slice(self, buf);
    }

    fn encoded_len(&self) -> usize {
        LEN_SIZE + self.iter().map(Encode::encoded_len).sum::<usize>()
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(&self, buf: &mut Vec<u8>) {
        self.as_slice().encode(buf);
    }

    fn encoded_len(&self) -> usize {
        self.as_slice().encoded_len()
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        T::decode_vec(d, len)
    }
}

impl<T: Decode> Decode for Box<[T]> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Vec::decode(d).map(Vec::into_boxed_slice)
    }
}

/// `len` values decoded one by one, each charged as it's decoded.
pub(crate) fn decode_each<T: Decode>(
    d: &mut Decoder<'_>,
    len: usize,
) -> Result<Vec<T>, DecodeError> {
    let mut out = Vec::with_capacity(cautious_capacity::<T>(len, d));
    for _ in 0..len {
        d.charge(element_cost::<T>())?;
        out.push(T::decode(d)?);
    }
    Ok(out)
}

impl<T: Encode, const N: usize> Encode for [T; N] {
    fn encode(&self, buf: &mut Vec<u8>) {
        for item in self {
            item.encode(buf);
        }
    }

    fn encoded_len(&self) -> usize {
        self.iter().map(Encode::encoded_len).sum()
    }
}

impl<T: Decode, const N: usize> Decode for [T; N] {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let mut items = Vec::with_capacity(N);
        for _ in 0..N {
            items.push(T::decode(d)?);
        }
        match items.try_into() {
            Ok(array) => Ok(array),
            Err(_) => unreachable!("decoded exactly N items"),
        }
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Some(value) => {
                buf.push(1);
                value.encode(buf);
            }
            None => buf.push(0),
        }
    }

    fn encoded_len(&self) -> usize {
        1 + self.as_ref().map_or(0, Encode::encoded_len)
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        if bool::decode(d)? {
            Ok(Some(T::decode(d)?))
        } else {
            Ok(None)
        }
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn encode(&self, buf: &mut Vec<u8>) {
        (**self).encode(buf);
    }

    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
}

impl<T: Encode + ?Sized> Encode for Box<T> {
    fn encode(&self, buf: &mut Vec<u8>) {
        (**self).encode(buf);
    }

    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
}

impl<T: Decode> Decode for Box<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        d.charge(size_of::<T>())?;
        T::decode(d).map(Box::new)
    }
}

impl<T: Encode + ?Sized> Encode for Arc<T> {
    fn encode(&self, buf: &mut Vec<u8>) {
        (**self).encode(buf);
    }

    fn encoded_len(&self) -> usize {
        (**self).encoded_len()
    }
}

impl<T: Decode> Decode for Arc<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        // The value and the two reference counts.
        d.charge(size_of::<T>() + 2 * size_of::<usize>())?;
        T::decode(d).map(Arc::new)
    }
}

macro_rules! tuple {
    ($($name:ident)+) => {
        impl<$($name: Encode),+> Encode for ($($name,)+) {
            #[allow(non_snake_case)]
            fn encode(&self, buf: &mut Vec<u8>) {
                let ($($name,)+) = self;
                $($name.encode(buf);)+
            }

            #[allow(non_snake_case)]
            fn encoded_len(&self) -> usize {
                let ($($name,)+) = self;
                0 $(+ $name.encoded_len())+
            }
        }

        impl<$($name: Decode),+> Decode for ($($name,)+) {
            fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
                Ok(($($name::decode(d)?,)+))
            }
        }
    };
}

tuple!(A);
tuple!(A B);
tuple!(A B C);
tuple!(A B C D);
tuple!(A B C D E);
tuple!(A B C D E F);
tuple!(A B C D E F G);
tuple!(A B C D E F G H);
tuple!(A B C D E F G H I);
tuple!(A B C D E F G H I J);
tuple!(A B C D E F G H I J K);
tuple!(A B C D E F G H I J K L);

fn encode_map<'a, K, V>(
    buf: &mut Vec<u8>,
    len: usize,
    entries: impl Iterator<Item = (&'a K, &'a V)>,
) where
    K: Encode + 'a,
    V: Encode + 'a,
{
    put_len(buf, len);
    for (k, v) in entries {
        k.encode(buf);
        v.encode(buf);
    }
}

fn map_len<'a, K, V>(entries: impl Iterator<Item = (&'a K, &'a V)>) -> usize
where
    K: Encode + 'a,
    V: Encode + 'a,
{
    LEN_SIZE
        + entries
            .map(|(k, v)| k.encoded_len() + v.encoded_len())
            .sum::<usize>()
}

fn encode_set<'a, T: Encode + 'a>(
    buf: &mut Vec<u8>,
    len: usize,
    items: impl Iterator<Item = &'a T>,
) {
    put_len(buf, len);
    for item in items {
        item.encode(buf);
    }
}

fn set_len<'a, T: Encode + 'a>(items: impl Iterator<Item = &'a T>) -> usize {
    LEN_SIZE + items.map(Encode::encoded_len).sum::<usize>()
}

fn duplicate<T>() -> DecodeError {
    DecodeError::DuplicateEntry {
        type_name: std::any::type_name::<T>(),
    }
}

impl<K: Encode, V: Encode> Encode for BTreeMap<K, V> {
    fn encode(&self, buf: &mut Vec<u8>) {
        encode_map(buf, self.len(), self.iter());
    }

    fn encoded_len(&self) -> usize {
        map_len(self.iter())
    }
}

/// A key repeated in the input is an error rather than silently dropping one
/// of the values.
impl<K: Decode + Ord, V: Decode> Decode for BTreeMap<K, V> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        let mut out = BTreeMap::new();
        for _ in 0..len {
            d.charge(element_cost::<(K, V)>())?;
            let k = K::decode(d)?;
            let v = V::decode(d)?;
            if out.insert(k, v).is_some() {
                return Err(duplicate::<Self>());
            }
        }
        Ok(out)
    }
}

impl<T: Encode> Encode for BTreeSet<T> {
    fn encode(&self, buf: &mut Vec<u8>) {
        encode_set(buf, self.len(), self.iter());
    }

    fn encoded_len(&self) -> usize {
        set_len(self.iter())
    }
}

impl<T: Decode + Ord> Decode for BTreeSet<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        let mut out = BTreeSet::new();
        for _ in 0..len {
            d.charge(element_cost::<T>())?;
            if !out.insert(T::decode(d)?) {
                return Err(duplicate::<Self>());
            }
        }
        Ok(out)
    }
}

/// Entries are written in iteration order, so equal maps can encode
/// differently. Use a `BTreeMap` where the bytes must be canonical.
impl<K: Encode, V: Encode, S> Encode for HashMap<K, V, S> {
    fn encode(&self, buf: &mut Vec<u8>) {
        encode_map(buf, self.len(), self.iter());
    }

    fn encoded_len(&self) -> usize {
        map_len(self.iter())
    }
}

impl<K, V, S> Decode for HashMap<K, V, S>
where
    K: Decode + Eq + Hash,
    V: Decode,
    S: BuildHasher + Default,
{
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        let capacity = cautious_capacity::<(K, V)>(len, d);
        let mut out = HashMap::with_capacity_and_hasher(capacity, S::default());
        for _ in 0..len {
            d.charge(element_cost::<(K, V)>())?;
            let k = K::decode(d)?;
            let v = V::decode(d)?;
            if out.insert(k, v).is_some() {
                return Err(duplicate::<Self>());
            }
        }
        Ok(out)
    }
}

/// Items are written in iteration order, so equal sets can encode
/// differently. Use a `BTreeSet` where the bytes must be canonical.
impl<T: Encode, S> Encode for HashSet<T, S> {
    fn encode(&self, buf: &mut Vec<u8>) {
        encode_set(buf, self.len(), self.iter());
    }

    fn encoded_len(&self) -> usize {
        set_len(self.iter())
    }
}

impl<T, S> Decode for HashSet<T, S>
where
    T: Decode + Eq + Hash,
    S: BuildHasher + Default,
{
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = get_len(d)?;
        let capacity = cautious_capacity::<T>(len, d);
        let mut out = HashSet::with_capacity_and_hasher(capacity, S::default());
        for _ in 0..len {
            d.charge(element_cost::<T>())?;
            if !out.insert(T::decode(d)?) {
                return Err(duplicate::<Self>());
            }
        }
        Ok(out)
    }
}
