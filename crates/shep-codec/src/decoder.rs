//! The decoder: the input being read, and what decoding it may still use.

use crate::{DecodeError, MAX_DEPTH};

/// The memory a decode may allocate unless told otherwise: twice the largest
/// frame Shep's RPC accepts.
pub const DEFAULT_MAX_MEMORY: usize = 64 << 20;

/// Hard bounds on one decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of memory the decoded value may allocate. Decoding past it fails
    /// with [`DecodeError::LimitExceeded`] before allocating.
    pub max_memory: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_memory: DEFAULT_MAX_MEMORY,
        }
    }
}

/// The input of one decode, with the budget it may still spend.
///
/// Every [`Decode`](crate::Decode) implementation reads through it, so a
/// value nested anywhere in a frame draws on the frame's one budget:
///
/// - **Memory.** Decoding a container charges what it allocates (a string's
///   bytes, a collection's elements, a box's contents) with
///   [`charge`](Decoder::charge) before allocating it. Every element is
///   charged at least one byte, so even elements that encode to nothing
///   can't make a short input do unbounded work.
///
/// The input is borrowed, and nothing decoded borrows from it: every value
/// owns its memory, so none can keep the input alive.
/// - **Depth.** Derived types may nest at most [`MAX_DEPTH`] deep.
#[derive(Debug)]
pub struct Decoder<'a> {
    buf: &'a [u8],
    limit: usize,
    budget: usize,
    depth: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder of `buf` under the default [`Limits`].
    pub fn new(buf: &'a [u8]) -> Decoder<'a> {
        Decoder::with_limits(buf, Limits::default())
    }

    pub fn with_limits(buf: &'a [u8], limits: Limits) -> Decoder<'a> {
        Decoder {
            buf,
            limit: limits.max_memory,
            budget: limits.max_memory,
            depth: 0,
        }
    }

    /// The input bytes left to read. Inside an envelope, those left in its
    /// body.
    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The input not read yet.
    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }

    /// Bytes of memory decoding may still allocate.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Spends `bytes` of the memory budget, failing if too little is left.
    pub fn charge(&mut self, bytes: usize) -> Result<(), DecodeError> {
        match self.budget.checked_sub(bytes) {
            Some(left) => {
                self.budget = left;
                Ok(())
            }
            None => Err(DecodeError::LimitExceeded { limit: self.limit }),
        }
    }

    /// The next `n` bytes of the input, borrowed: copy what you keep.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.ensure(n)?;
        let (taken, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(taken)
    }

    /// Copies out the next `N` bytes.
    pub fn take_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.ensure(N)?;
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }

    fn ensure(&self, n: usize) -> Result<(), DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::UnexpectedEnd {
                needed: n,
                remaining: self.buf.len(),
            });
        }
        Ok(())
    }

    /// Runs `f` one level of nesting deeper.
    pub(crate) fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Decoder<'a>) -> Result<T, DecodeError>,
    ) -> Result<T, DecodeError> {
        if self.depth >= MAX_DEPTH {
            return Err(DecodeError::TooDeep);
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// Runs `f` on the next `size` bytes alone, then skips whatever it left
    /// of them.
    pub(crate) fn within<T>(
        &mut self,
        size: usize,
        f: impl FnOnce(&mut Decoder<'a>) -> Result<T, DecodeError>,
    ) -> Result<T, DecodeError> {
        let body = self.take(size)?;
        let rest = std::mem::replace(&mut self.buf, body);
        let result = f(self);
        self.buf = rest;
        result
    }
}
