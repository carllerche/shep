//! What a decode allocates stays within a constant factor of its memory
//! limit, whatever the input, including shapes where the memory a value
//! takes is many times the bytes that encode it.
//!
//! This file is its own test binary, so the counting allocator sees only its
//! allocations; it counts per thread, so only the decoding thread's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use hegel::TestCase;
use hegel::generators as gs;
use shep_codec::{Decode, Decoder, Encode, Limits};

const LIMIT: usize = 16 << 10;

/// A decode holds at most this many times its limit at once. The limit
/// charges each element its size; the rest is the collections' own overhead
/// (B-tree nodes, hash table control bytes, growth slack).
const FACTOR: usize = 8;

struct Counting;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn grew(by: usize) {
    let live = LIVE.get() + by;
    LIVE.set(live);
    PEAK.set(PEAK.get().max(live));
}

fn shrank(by: usize) {
    LIVE.set(LIVE.get().saturating_sub(by));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        shrank(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grew(new_size);
        shrank(layout.size());
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[derive(Encode, Decode, Debug)]
#[codec(transparent)]
struct Nothing([u8; 0]);

/// Shapes whose values take far more memory than their encodings.
#[derive(Encode, Decode, Debug)]
enum Amplified {
    Strings(Vec<String>),
    Vecs(Vec<Vec<u64>>),
    Maps(Vec<BTreeMap<u8, u8>>),
    Sets(Vec<BTreeSet<u8>>),
    Hashes(Vec<HashMap<u8, u8>>),
    Boxes(Vec<Option<Box<[u64; 8]>>>),
    Shared(Vec<Arc<String>>),
    Nothing(Vec<Vec<Nothing>>),
}

/// Decodes `input` as a `T` under the limit and checks the most it held at
/// once.
fn check<T: Decode>(input: &Bytes) {
    let before = LIVE.get();
    PEAK.set(before);
    let mut d = Decoder::with_limits(input.clone(), Limits { max_memory: LIMIT });
    let decoded = T::decode(&mut d);
    let held = PEAK.get() - before;
    drop(decoded);
    assert!(
        held <= FACTOR * LIMIT,
        "{} held {held} bytes under a limit of {LIMIT}",
        std::any::type_name::<T>()
    );
}

/// An enum envelope around a tag, a count, and repetitions of one element's
/// encoding.
#[hegel::test(test_cases = 300)]
fn decoding_stays_within_the_limit(tc: TestCase) {
    let tag = tc.draw(gs::integers::<u8>().max_value(7));
    let count = tc.draw(gs::sampled_from(vec![0u32, 1, 100, 10_000, u32::MAX]));
    // The smallest encodings of each shape's elements, or arbitrary bytes.
    let small: Vec<Vec<u8>> = vec![
        vec![0, 0, 0, 0],
        vec![1, 0, 0, 0, 7],
        vec![1, 0, 0, 0, 7, 9],
        vec![0],
        vec![0xff, 0xff, 0xff, 0xff],
    ];
    let element = if tc.draw(gs::weighted_booleans(0.8)) {
        tc.draw(gs::sampled_from(small))
    } else {
        tc.draw(gs::binary().max_size(12))
    };
    let repeats = tc.draw(gs::integers::<usize>().max_value(4096));

    let mut body = vec![tag];
    body.extend(count.to_le_bytes());
    for _ in 0..repeats {
        body.extend(&element);
    }
    let mut input = vec![0, 0];
    input.extend((body.len() as u32).to_le_bytes());
    input.extend(body);
    let input = Bytes::from(input);

    check::<Amplified>(&input);
    check::<Vec<String>>(&input.slice(7..));
    check::<Vec<BTreeMap<u8, u8>>>(&input.slice(7..));
    check::<Vec<Vec<Nothing>>>(&input.slice(7..));
}
