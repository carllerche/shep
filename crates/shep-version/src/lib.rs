//! Dots and dotted version vectors (design §9).
//!
//! A [`Dot`] is a position in one partition's replicated log. A [`Dvv`] is a
//! set of dots: for each partition, a contiguous prefix of its log plus extra
//! dots beyond the prefix. Every fact Shep stores is stamped with a dot, and a
//! fact is visible at a version exactly when its stamp is in it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use shep_codec::{Decode, DecodeError, Encode};

/// A partition of a universe. Each partition has its own replicated log.
///
/// IDs are assigned in increasing order and never reused, so a partition's ID
/// is also its generation (§9.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[codec(transparent)]
pub struct PartitionId(pub u64);

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// A position in a partition's log: `(partition, index)`.
///
/// Index 0 is the log's initial entry, which every version contains.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Dot {
    pub partition: PartitionId,
    pub index: u64,
}

impl Dot {
    pub fn new(partition: PartitionId, index: u64) -> Dot {
        Dot { partition, index }
    }
}

impl fmt::Display for Dot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.partition, self.index)
    }
}

/// A dotted version vector: per partition, a prefix `0..=n` of its log plus a
/// set of extra dots beyond it.
///
/// The representation is canonical: extras never include `prefix + 1` (it is
/// folded into the prefix) or anything at or below the prefix, and partitions
/// with nothing beyond index 0 are omitted. So two `Dvv`s are equal exactly
/// when they hold the same dots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Dvv {
    parts: BTreeMap<PartitionId, Component>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct Component {
    prefix: u64,
    extras: BTreeSet<u64>,
}

impl Component {
    fn contains(&self, index: u64) -> bool {
        index <= self.prefix || self.extras.contains(&index)
    }

    fn raise_prefix(&mut self, prefix: u64) {
        if prefix > self.prefix {
            self.prefix = prefix;
            self.normalize();
        }
    }

    fn insert(&mut self, index: u64) {
        if !self.contains(index) {
            self.extras.insert(index);
            self.normalize();
        }
    }

    fn normalize(&mut self) {
        // Drop extras the prefix covers, then fold any run that continues it.
        // A prefix of `u64::MAX` covers every index.
        let Some(next) = self.prefix.checked_add(1) else {
            self.extras.clear();
            return;
        };
        self.extras = self.extras.split_off(&next);
        while let Some(next) = self.prefix.checked_add(1)
            && self.extras.first() == Some(&next)
        {
            self.extras.pop_first();
            self.prefix = next;
        }
    }

    fn is_empty(&self) -> bool {
        self.prefix == 0 && self.extras.is_empty()
    }

    fn includes(&self, other: &Component) -> bool {
        // Every dot of `other`'s prefix beyond ours must be one of our extras.
        let prefix_ok = other.prefix <= self.prefix
            || (self.prefix + 1..=other.prefix).all(|i| self.extras.contains(&i));
        prefix_ok && other.extras.iter().all(|&i| self.contains(i))
    }
}

impl Dvv {
    /// The empty version: only each partition's index 0.
    pub fn new() -> Dvv {
        Dvv::default()
    }

    /// Builder form of [`Dvv::raise_prefix`].
    pub fn with_prefix(mut self, partition: PartitionId, prefix: u64) -> Dvv {
        self.raise_prefix(partition, prefix);
        self
    }

    /// Builder form of [`Dvv::insert`].
    pub fn with_dot(mut self, dot: Dot) -> Dvv {
        self.insert(dot);
        self
    }

    /// Whether `dot` is in this version.
    pub fn contains(&self, dot: Dot) -> bool {
        match self.parts.get(&dot.partition) {
            Some(c) => c.contains(dot.index),
            None => dot.index == 0,
        }
    }

    /// The contiguous prefix this version holds of `partition`'s log.
    pub fn prefix(&self, partition: PartitionId) -> u64 {
        self.parts.get(&partition).map_or(0, |c| c.prefix)
    }

    /// The dots of `partition` beyond its prefix, in increasing order.
    pub fn extras(&self, partition: PartitionId) -> impl Iterator<Item = u64> + '_ {
        self.parts
            .get(&partition)
            .into_iter()
            .flat_map(|c| c.extras.iter().copied())
    }

    /// The partitions this version names beyond index 0, in increasing order.
    pub fn partitions(&self) -> impl Iterator<Item = PartitionId> + '_ {
        self.parts.keys().copied()
    }

    /// Whether this version holds nothing beyond each log's index 0.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Adds every index up to `prefix` of `partition`.
    pub fn raise_prefix(&mut self, partition: PartitionId, prefix: u64) {
        if prefix == 0 {
            return;
        }
        self.parts
            .entry(partition)
            .or_default()
            .raise_prefix(prefix);
    }

    /// Adds one dot.
    pub fn insert(&mut self, dot: Dot) {
        if dot.index == 0 {
            return;
        }
        self.parts
            .entry(dot.partition)
            .or_default()
            .insert(dot.index);
    }

    /// Adds every dot of `other` (set union).
    pub fn merge(&mut self, other: &Dvv) {
        for (&partition, theirs) in &other.parts {
            let ours = self.parts.entry(partition).or_default();
            ours.raise_prefix(theirs.prefix);
            for &i in &theirs.extras {
                ours.insert(i);
            }
        }
    }

    /// Whether every dot of `other` is in this version (`other ⊆ self`).
    pub fn includes(&self, other: &Dvv) -> bool {
        other
            .parts
            .iter()
            .all(|(partition, theirs)| match self.parts.get(partition) {
                Some(ours) => ours.includes(theirs),
                None => theirs.is_empty(),
            })
    }
}

impl fmt::Display for Dvv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let mut first = true;
        for (partition, c) in &self.parts {
            if !first {
                write!(f, ", ")?;
            }
            first = false;
            write!(f, "{partition} ≤ {}", c.prefix)?;
            for i in &c.extras {
                write!(f, " + {i}")?;
            }
        }
        write!(f, "}}")
    }
}

/// On the wire, a version is its partitions in order, each as its prefix and
/// its extras. Decoding normalizes, so any listing of the same dots decodes
/// to the same version.
#[derive(Encode, Decode)]
struct WirePart {
    partition: PartitionId,
    prefix: u64,
    extras: Vec<u64>,
}

impl Encode for Dvv {
    fn encode(&self, buf: &mut Vec<u8>) {
        self.wire().encode(buf)
    }

    fn encoded_len(&self) -> usize {
        self.wire().encoded_len()
    }
}

impl Decode for Dvv {
    fn decode(d: &mut shep_codec::Decoder) -> Result<Dvv, DecodeError> {
        let parts = Vec::<WirePart>::decode(d)?;
        let mut v = Dvv::new();
        for p in parts {
            v.raise_prefix(p.partition, p.prefix);
            for i in p.extras {
                v.insert(Dot::new(p.partition, i));
            }
        }
        Ok(v)
    }
}

impl Dvv {
    fn wire(&self) -> Vec<WirePart> {
        self.parts
            .iter()
            .map(|(&partition, c)| WirePart {
                partition,
                prefix: c.prefix,
                extras: c.extras.iter().copied().collect(),
            })
            .collect()
    }
}
