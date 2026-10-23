//! `Connection` against a model: reading any stream a peer could send, and
//! writing frames through any stream, however the bytes go through, and
//! however often a call is cancelled.
//!
//! Streams mix well-formed frames (empty metadata and bodies, metadata at
//! the 4 KiB limit, bodies at and over the reader's limit) with frames broken
//! every way the protocol cares about, junk, and truncation anywhere, empty
//! streams included. The reader is fed through a scripted `AsyncRead` that
//! returns `Pending`, short reads of any size, or errors, in a drawn order.
//! The driver drops a call at any `Pending` (a cancellation), and then may
//! call it again, deal with the body another way (skip it, or go on to the
//! next head), write a frame on the same connection, or try calls that
//! aren't valid at that point. Whatever happens:
//!
//! - the reader never panics;
//! - it reads exactly what a model reading the bytes in one pass reads, for
//!   the choices the driver made: the same heads, bodies, skips, clean end,
//!   or failure;
//! - an injected error fails the call it hits and every call after, and
//!   nothing before it differs from the model;
//! - calls that aren't valid change nothing, and the end stays the end;
//! - it never holds more than one frame's head and one body under the limit;
//! - writing, it puts exactly the queued frames' bytes on the stream, in
//!   order, which read back as the same frames however they arrive; an error,
//!   or a write that takes nothing, fails that flush and every one after,
//!   having written a prefix of them;
//! - reads and writes on one connection keep out of each other's way.
//!
//! This file is its own test binary, so the counting allocator sees only its
//! allocations; it counts per thread, so only the reading thread's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker, ready};

use hegel::TestCase;
use hegel::generators as gs;
use shep_rpc::Connection;
use shep_rpc::wire::{COMPRESSION_NONE, COMPRESSION_ZSTD, HEADER_SIZE, Header, payload_checksum};
use shep_rpc::{Error, Frame, MAX_METADATA, Metadata, MetadataBuilder};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The body limit the driver reads under.
const MAX_BODY: usize = 256;

/// The most the reader may hold at once: a head buffer and the metadata
/// copied out of it, and one body, with room for small bookkeeping.
const MAX_HELD: usize = 2 * (HEADER_SIZE + MAX_METADATA) + MAX_BODY + 1024;

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

/// What one `poll_read` does.
#[derive(Clone, Copy, Debug)]
enum Step {
    Pending,
    /// Return at most this many bytes.
    Chunk(usize),
    Fail(io::ErrorKind),
}

/// Errors the script injects; none of them is one the reader makes itself.
const INJECTED: [io::ErrorKind; 2] = [io::ErrorKind::ConnectionReset, io::ErrorKind::TimedOut];

/// A stream that delivers `data` as `steps` say, then all at once.
struct Scripted {
    data: Vec<u8>,
    pos: usize,
    steps: VecDeque<Step>,
}

impl AsyncRead for Scripted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let limit = match self.steps.pop_front() {
            Some(Step::Pending) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Some(Step::Fail(kind)) => return Poll::Ready(Err(kind.into())),
            Some(Step::Chunk(n)) => n,
            None => usize::MAX,
        };
        let n = limit.min(buf.remaining()).min(self.data.len() - self.pos);
        let pos = self.pos;
        buf.put_slice(&self.data[pos..pos + n]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

/// What the driver does with a frame's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    Read,
    Skip,
    /// Go straight to the next head, which skips the body.
    Leave,
}

/// What the driver saw, without holding on to any of it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Head {
        header: Header,
        /// A hash of the metadata.
        metadata: u64,
    },
    Body {
        len: usize,
        hash: u64,
    },
    Skipped,
    End,
    Failed(Failure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// The stream ended mid-frame.
    Eof,
    /// The frame broke the protocol.
    Invalid,
    /// The script's error.
    Injected,
}

fn failure(e: &Error) -> Failure {
    match e {
        Error::UnexpectedEof => Failure::Eof,
        Error::HeaderChecksum
        | Error::PayloadChecksum
        | Error::MetadataTooLarge(_)
        | Error::Metadata(_)
        | Error::UnknownFlags(_) => Failure::Invalid,
        Error::Io(e) if INJECTED.contains(&e.kind()) => Failure::Injected,
        e => panic!("unexpected error {e:?}"),
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Whether the driver refuses a frame's body: what a server would.
fn refused(header: &Header) -> bool {
    header.compression != COMPRESSION_NONE || header.payload_size as usize > MAX_BODY
}

/// The events of reading `bytes` in one pass.
fn model(bytes: &[u8], used: &[Policy]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut rest = bytes;
    let mut frame = 0;
    loop {
        if rest.is_empty() {
            events.push(Event::End);
            return events;
        }
        if rest.len() < HEADER_SIZE {
            events.push(Event::Failed(Failure::Eof));
            return events;
        }
        let header = Header::decode(rest[..HEADER_SIZE].try_into().unwrap());
        let md_len = header.metadata_size as usize;
        if md_len > MAX_METADATA {
            events.push(Event::Failed(Failure::Invalid));
            return events;
        }
        let Some(md) = rest.get(HEADER_SIZE..HEADER_SIZE + md_len) else {
            events.push(Event::Failed(Failure::Eof));
            return events;
        };
        if !header.checksum_ok(md) || header.flags != 0 || Metadata::parse(md.to_vec()).is_err() {
            events.push(Event::Failed(Failure::Invalid));
            return events;
        }
        events.push(Event::Head {
            header,
            metadata: hash(md),
        });
        rest = &rest[HEADER_SIZE + md_len..];
        // Past where the driver stopped (an injected error), any policy will
        // do: only the prefix is compared.
        let policy = used.get(frame).copied().unwrap_or(Policy::Skip);
        frame += 1;
        let size = header.payload_size as usize;
        let Some(body) = rest.get(..size) else {
            // Reading, skipping, or the next head skipping: all run out.
            events.push(Event::Failed(Failure::Eof));
            return events;
        };
        match policy {
            Policy::Read if payload_checksum(body) != header.payload_checksum => {
                events.push(Event::Failed(Failure::Invalid));
                return events;
            }
            Policy::Read => events.push(Event::Body {
                len: size,
                hash: hash(body),
            }),
            Policy::Skip => events.push(Event::Skipped),
            Policy::Leave => {}
        }
        rest = &rest[size..];
    }
}

/// Runs a call to completion, dropping its future at a `Pending` when
/// `cancels` says so and calling again.
macro_rules! drive {
    ($frames:expr, $cancels:expr, $method:ident $(, $arg:expr)*) => {{
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            let mut call = pin!($frames.$method($($arg),*));
            let done = loop {
                match call.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => break Some(result),
                    Poll::Pending if $cancels.pop_front().unwrap_or(false) => break None,
                    Poll::Pending => {}
                }
            };
            if let Some(result) = done {
                break result;
            }
        }
    }};
}

/// A stream that reads from a script and writes to a sink, so one
/// connection can do both.
struct Duplex {
    read: Scripted,
    write: Sink,
}

impl AsyncRead for Duplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl AsyncWrite for Duplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

/// The driver's choices, drawn up front so a run replays exactly.
#[derive(Debug)]
struct Plan {
    /// Per `Pending`: whether to cancel the call there.
    cancels: VecDeque<bool>,
    /// Per cancelled body call: what to do with the body instead, if
    /// anything.
    switches: VecDeque<Option<Policy>>,
    /// Per cancellation: whether to write a frame on the same connection
    /// before going on.
    writes: VecDeque<bool>,
    /// Per gap between frames: whether to try the calls that aren't valid
    /// there.
    misuse: VecDeque<bool>,
    /// Per frame: what to do with a body the driver would accept.
    policies: Vec<Policy>,
    /// Calls cancelled so far.
    cancelled: usize,
}

/// A frame written while reading, to check the two sides keep apart.
fn interleaved() -> Frame {
    Frame::new(0, 9, 9, Metadata::empty(), b"interleaved".to_vec())
}

/// Polls `call` to completion, or until the plan cancels it at a `Pending`
/// (`None`).
fn poll_or_cancel<F: Future>(call: Pin<&mut F>, plan: &mut Plan) -> Option<F::Output> {
    let output = poll_or_cancel_inner(call, plan);
    if output.is_none() {
        plan.cancelled += 1;
    }
    output
}

fn poll_or_cancel_inner<F: Future>(call: Pin<&mut F>, plan: &mut Plan) -> Option<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    let mut call = call;
    loop {
        match call.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return Some(result),
            Poll::Pending if plan.cancels.pop_front().unwrap_or(false) => return None,
            Poll::Pending => {}
        }
    }
}

/// What a run read, which policy it used for each frame's body, and what it
/// wrote along the way.
struct Ran {
    events: Vec<Event>,
    used: Vec<Policy>,
    wrote: usize,
    /// Calls cancelled, body plans changed after a cancellation, and gaps
    /// where invalid calls were tried.
    cancelled: usize,
    switched: usize,
    misused: usize,
}

impl Ran {
    /// Tells the statistics report what this case exercised.
    fn report(&self, tc: &TestCase) {
        tc.event(format!("ended {:?}", self.events.last()));
        tc.event_value("frames", self.used.len() as f64);
        for (what, n) in [
            ("cancelled", self.cancelled),
            ("switched", self.switched),
            ("misused", self.misused),
            ("wrote", self.wrote),
        ] {
            if n > 0 {
                tc.event(what);
            }
        }
    }
}

/// After a cancellation: maybe write a frame on the same connection.
fn maybe_write(conn: &mut Connection<Duplex>, plan: &mut Plan, wrote: &mut usize) {
    if plan.writes.pop_front().unwrap_or(false) {
        conn.queue(interleaved());
        let mut none = VecDeque::new();
        drive!(conn, none, flush).unwrap();
        *wrote += 1;
    }
}

/// Reads `bytes` through `steps` as `plan` says, checking what it holds as it
/// goes.
fn run(bytes: Vec<u8>, steps: Vec<Step>, mut plan: Plan) -> Ran {
    let mut events = Vec::with_capacity(64);
    let mut used = Vec::with_capacity(64);
    let mut wrote = 0;
    let (mut switched, mut misused) = (0, 0);
    let mut conn = Connection::new(Duplex {
        read: Scripted {
            data: bytes,
            pos: 0,
            steps: steps.into(),
        },
        write: Sink::default(),
    });
    let base = LIVE.get();
    PEAK.set(base);
    let mut frame = 0;
    // Between frames, with no body pending.
    let mut between = true;
    loop {
        if between && plan.misuse.pop_front().unwrap_or(false) {
            let mut none = VecDeque::new();
            let read = drive!(conn, none, read_body, MAX_BODY);
            assert!(matches!(read, Err(Error::NoBody)), "{read:?}");
            assert!(drive!(conn, none, skip_body).is_ok());
            misused += 1;
        }
        let head = loop {
            let result = poll_or_cancel(pin!(conn.next_head()), &mut plan);
            match result {
                Some(result) => break result,
                None => maybe_write(&mut conn, &mut plan, &mut wrote),
            }
        };
        let head = match head {
            Ok(Some(head)) => head,
            Ok(None) => {
                events.push(Event::End);
                // The end stays the end, unless reading the stream again
                // fails.
                let mut none = VecDeque::new();
                match drive!(conn, none, next_head) {
                    Ok(None) => {}
                    Err(Error::Io(e)) if INJECTED.contains(&e.kind()) => {}
                    other => panic!("after the end: {other:?}"),
                }
                break;
            }
            Err(e) => {
                events.push(Event::Failed(failure(&e)));
                break;
            }
        };
        let header = head.header;
        events.push(Event::Head {
            header,
            metadata: hash(head.metadata.as_bytes()),
        });
        drop(head);
        let mut policy = if refused(&header) {
            Policy::Skip
        } else {
            plan.policies[frame % plan.policies.len()]
        };
        frame += 1;
        // Deal with the body, changing course at cancellations as planned.
        let result = loop {
            let done = match policy {
                Policy::Leave => Some(Ok(None)),
                Policy::Read => {
                    poll_or_cancel(pin!(conn.read_body(MAX_BODY)), &mut plan).map(|r| {
                        r.map(|body| {
                            Some(Event::Body {
                                len: body.len(),
                                hash: hash(&body),
                            })
                        })
                    })
                }
                Policy::Skip => poll_or_cancel(pin!(conn.skip_body()), &mut plan)
                    .map(|r| r.map(|()| Some(Event::Skipped))),
            };
            if let Some(done) = done {
                break done;
            }
            maybe_write(&mut conn, &mut plan, &mut wrote);
            if let Some(next) = plan.switches.pop_front().flatten() {
                // Reading may give way to skipping or leaving; skipping only
                // to leaving, since part of the body may be gone.
                if !(policy == Policy::Skip && next == Policy::Read) {
                    switched += usize::from(policy != next);
                    policy = next;
                }
            }
        };
        used.push(policy);
        between = policy != Policy::Leave;
        match result {
            Ok(event) => events.extend(event),
            Err(e) => {
                events.push(Event::Failed(failure(&e)));
                break;
            }
        }
    }
    let held = PEAK.get() - base;
    assert!(held <= MAX_HELD, "held {held} bytes");
    // After a failure the stream is out of step, and stays failed; asking
    // for a body says so rather than that there's none.
    if matches!(events.last(), Some(Event::Failed(_))) {
        let mut none = VecDeque::new();
        assert!(matches!(drive!(conn, none, next_head), Err(Error::Broken)));
        assert!(matches!(drive!(conn, none, skip_body), Err(Error::Broken)));
        assert!(matches!(
            drive!(conn, none, read_body, MAX_BODY),
            Err(Error::Broken)
        ));
    }
    // The writes went out whole, between the reads.
    let mut expected = Vec::new();
    for _ in 0..wrote {
        expected.extend(encoded(&interleaved()));
    }
    assert_eq!(conn.get_ref().write.written, expected);
    Ran {
        events,
        used,
        wrote,
        cancelled: plan.cancelled,
        switched,
        misused,
    }
}

#[hegel::test_helper]
fn frame(tc: &TestCase) -> Vec<u8> {
    let entries = tc.draw(gs::integers::<usize>().max_value(3));
    let mut builder = MetadataBuilder::new();
    for _ in 0..entries {
        let key = tc.draw(gs::integers::<u16>().max_value(6));
        let value = tc.draw(gs::binary().max_size(6));
        builder = builder.insert(key, value);
    }
    let mut metadata = builder.build().unwrap().as_bytes().to_vec();
    let size = tc.draw(gs::sampled_from(vec![
        0,
        1,
        17,
        MAX_BODY - 1,
        MAX_BODY,
        MAX_BODY + 1,
    ]));
    let fill = tc.draw(gs::integers::<u8>());
    let mut body = vec![fill; size];
    let method = tc.draw(gs::integers::<u32>());
    let id = tc.draw(gs::integers::<u32>());
    let mut header = Header::new(0, method, id, &metadata, size, payload_checksum(&body));

    // Mostly well-formed, so streams get several frames in before a fault.
    let fault = tc.draw(gs::integers::<u8>().max_value(48));
    match fault {
        // Malformed metadata under a good checksum.
        0 => metadata = vec![1, 0, 9, 0, 0],
        // Compression the reader's user refuses.
        1 => header.compression = COMPRESSION_ZSTD,
        // Unknown flags.
        2 => header.flags = 1,
        // Metadata claimed over the limit.
        3 => header.metadata_size = (MAX_METADATA + 1) as u16,
        // A body claimed far over the limit.
        4 => header.payload_size = u32::MAX,
        // Metadata of exactly the limit, and one byte under: both fine.
        6 | 7 => {
            let value = vec![7; MAX_METADATA - 4 - usize::from(fault == 7)];
            metadata = MetadataBuilder::new()
                .insert(1, value)
                .build()
                .unwrap()
                .as_bytes()
                .to_vec();
        }
        // Real metadata one byte over the limit, not just a claim.
        8 => {
            metadata = vec![1, 0];
            metadata.extend(((MAX_METADATA - 3) as u16).to_le_bytes());
            metadata.resize(MAX_METADATA + 1, 7);
        }
        _ => {}
    }
    header.metadata_size = match fault {
        3 => header.metadata_size,
        _ => metadata.len() as u16,
    };
    header.header_checksum = header.compute_checksum(&metadata);
    let mut bytes = header.encode().to_vec();
    bytes.extend(&metadata);
    bytes.append(&mut body);
    // Corruption after the checksums: a flipped byte anywhere.
    if fault == 5 {
        let at = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        bytes[at] ^= 1 << tc.draw(gs::integers::<u8>().max_value(7));
    }
    bytes
}

#[hegel::test_helper]
fn stream(tc: &TestCase) -> Vec<u8> {
    let mut bytes = Vec::new();
    let parts = tc.draw(gs::integers::<usize>().max_value(6));
    for _ in 0..parts {
        let raw = tc.draw(gs::weighted_booleans(0.05));
        if raw {
            let junk = tc.draw(gs::binary().max_size(40));
            bytes.extend(junk);
        } else {
            bytes.extend(frame(tc));
        }
    }
    let truncate = tc.draw(gs::weighted_booleans(0.2));
    if truncate && !bytes.is_empty() {
        let at = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        bytes.truncate(at);
    }
    bytes
}

#[hegel::test_helper]
fn steps(tc: &TestCase, faults: bool) -> Vec<Step> {
    let count = tc.draw(gs::integers::<usize>().max_value(100));
    (0..count)
        .map(|_| {
            let kind = tc.draw(gs::integers::<u8>().max_value(if faults { 9 } else { 8 }));
            match kind {
                0..=2 => Step::Pending,
                3..=8 => Step::Chunk(tc.draw(gs::integers::<usize>().min_value(1).max_value(40))),
                _ => Step::Fail(tc.draw(gs::sampled_from(INJECTED.to_vec()))),
            }
        })
        .collect()
}

#[hegel::test_helper]
fn plan(tc: &TestCase) -> Plan {
    let policies = vec![Policy::Read, Policy::Skip, Policy::Leave];
    let cancels = tc.draw(gs::vecs(gs::booleans()).max_size(60));
    let switch = gs::sampled_from(vec![
        None,
        Some(Policy::Read),
        Some(Policy::Skip),
        Some(Policy::Leave),
    ]);
    let switches = tc.draw(gs::vecs(switch).max_size(20));
    let writes = tc.draw(gs::vecs(gs::weighted_booleans(0.3)).max_size(10));
    let misuse = tc.draw(gs::vecs(gs::weighted_booleans(0.3)).max_size(8));
    let policies = tc.draw(gs::vecs(gs::sampled_from(policies)).min_size(1).max_size(4));
    Plan {
        cancels: cancels.into(),
        switches: switches.into(),
        writes: writes.into(),
        misuse: misuse.into(),
        policies,
        cancelled: 0,
    }
}

/// However the bytes arrive, however often calls are cancelled, whatever
/// the driver does with a body after a cancellation, and with writes on the
/// same connection in between, the reader reads what the model reads.
#[hegel::test(test_cases = 5000)]
fn reads_what_the_model_reads(tc: TestCase) {
    let bytes = stream(&tc);
    let steps = steps(&tc, false);
    let plan = plan(&tc);
    let ran = run(bytes.clone(), steps, plan);
    tc.note(&format!("used {:?}, wrote {}", ran.used, ran.wrote));
    ran.report(&tc);
    assert_eq!(ran.events, model(&bytes, &ran.used));
}

/// An error from the stream fails the call it hits and every call after;
/// up to it, the reader agrees with the model.
#[hegel::test(test_cases = 5000)]
fn errors_fail_cleanly(tc: TestCase) {
    let bytes = stream(&tc);
    let steps = steps(&tc, true);
    let plan = plan(&tc);
    let ran = run(bytes.clone(), steps, plan);
    ran.report(&tc);
    let want = model(&bytes, &ran.used);
    match ran.events.last() {
        Some(Event::Failed(Failure::Injected)) => {
            let before = &ran.events[..ran.events.len() - 1];
            assert_eq!(before, &want[..before.len()]);
        }
        _ => assert_eq!(ran.events, want),
    }
}

/// A call cancelled partway through each part of a frame resumes where it
/// stopped.
#[test]
fn cancelled_calls_resume() {
    let metadata = MetadataBuilder::new().insert(1, "route").build().unwrap();
    let body = b"the body";
    let h = Header::new(
        0,
        7,
        1,
        metadata.as_bytes(),
        body.len(),
        payload_checksum(body),
    );
    let mut bytes = h.encode().to_vec();
    bytes.extend(metadata.as_bytes());
    bytes.extend(body);
    let twice = [bytes.clone(), bytes].concat();
    // One byte at a time, cancelling at every chance.
    let steps = (0..twice.len())
        .flat_map(|_| [Step::Pending, Step::Chunk(1)])
        .collect();
    let plan = Plan {
        cancels: vec![true; twice.len()].into(),
        switches: VecDeque::new(),
        writes: VecDeque::new(),
        misuse: VecDeque::new(),
        policies: vec![Policy::Read, Policy::Skip],
        cancelled: 0,
    };
    let ran = run(twice.clone(), steps, plan);
    let got = ran.events;
    assert_eq!(got, model(&twice, &ran.used));
    assert!(matches!(got[1], Event::Body { len: 8, .. }));
    assert_eq!(got[3], Event::Skipped);
    assert_eq!(got[4], Event::End);
}

/// One good frame with `body`, and its bytes.
fn good_frame(body: &[u8]) -> Vec<u8> {
    let h = Header::new(0, 7, 1, &[], body.len(), payload_checksum(body));
    [&h.encode()[..], body].concat()
}

/// A body read cancelled partway can be abandoned: skipping it, or going on
/// to the next head, skips only what the cancelled read didn't consume.
#[test]
fn a_cancelled_read_can_be_abandoned() {
    let body = vec![9; 100];
    let next = good_frame(b"next");
    let bytes = [good_frame(&body), next].concat();
    for skip_explicitly in [true, false] {
        let mut cx = Context::from_waker(Waker::noop());
        // The head, then 40 bytes of body, then a pause.
        let steps = vec![Step::Chunk(HEADER_SIZE), Step::Chunk(40), Step::Pending];
        let mut frames = Connection::new(Scripted {
            data: bytes.clone(),
            pos: 0,
            steps: steps.into(),
        });
        let mut none = VecDeque::new();
        let head = drive!(frames, none, next_head).unwrap().unwrap();
        assert_eq!(head.header.payload_size, 100);
        {
            let mut read = pin!(frames.read_body(MAX_BODY));
            assert!(read.as_mut().poll(&mut cx).is_pending());
        }
        if skip_explicitly {
            drive!(frames, none, skip_body).unwrap();
        }
        let head = drive!(frames, none, next_head).unwrap().unwrap();
        assert_eq!(head.header.payload_size, 4);
        assert_eq!(drive!(frames, none, read_body, MAX_BODY).unwrap(), b"next");
        assert!(drive!(frames, none, next_head).unwrap().is_none());
    }
}

/// A body over the limit fails before anything is allocated for it, even if
/// the caller asks to read it.
#[test]
fn an_oversized_body_fails_before_allocating() {
    let mut h = Header::new(0, 7, 1, &[], 0, payload_checksum(b""));
    h.payload_size = u32::MAX;
    h.header_checksum = h.compute_checksum(&[]);
    let mut frames = Connection::new(Scripted {
        data: h.encode().to_vec(),
        pos: 0,
        steps: VecDeque::new(),
    });
    let mut none = VecDeque::new();
    drive!(frames, none, next_head).unwrap().unwrap();
    let base = LIVE.get();
    PEAK.set(base);
    let e = drive!(frames, none, read_body, MAX_BODY).unwrap_err();
    assert!(matches!(e, Error::BodyTooLarge { .. }), "{e}");
    assert!(PEAK.get() - base < 1024, "allocated {}", PEAK.get() - base);
}

/// What one `poll_write` or `poll_flush` does.
#[derive(Clone, Copy, Debug)]
enum WriteStep {
    Pending,
    /// Take at most this many bytes (a flush just succeeds).
    Accept(usize),
    Fail(io::ErrorKind),
}

/// A stream that takes writes as `steps` say, then whole.
#[derive(Default)]
struct Sink {
    written: Vec<u8>,
    steps: VecDeque<WriteStep>,
}

impl Sink {
    fn step(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        match self.steps.pop_front() {
            Some(WriteStep::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(WriteStep::Fail(kind)) => Poll::Ready(Err(kind.into())),
            Some(WriteStep::Accept(n)) => Poll::Ready(Ok(n)),
            None => Poll::Ready(Ok(usize::MAX)),
        }
    }
}

impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = ready!(this.step(cx))?.min(buf.len());
        this.written.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.get_mut().step(cx))?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A valid frame to send, small enough to read back under the limit.
#[hegel::test_helper]
fn outgoing(tc: &TestCase) -> Frame {
    let key = tc.draw(gs::integers::<u16>().max_value(4));
    let kind = tc.draw(gs::integers::<u8>().max_value(4));
    let metadata = match kind {
        0 | 1 => Metadata::empty(),
        2 | 3 => {
            let value = tc.draw(gs::binary().max_size(8));
            MetadataBuilder::new().insert(key, value).build().unwrap()
        }
        // As much as fits.
        _ => MetadataBuilder::new()
            .insert(key, vec![9; MAX_METADATA - 4])
            .build()
            .unwrap(),
    };
    let body = tc.draw(gs::binary().max_size(MAX_BODY));
    let id = tc.draw(gs::integers::<u32>());
    Frame::new(0, 7, id, metadata, body)
}

fn encoded(frame: &Frame) -> Vec<u8> {
    [
        &frame.header.encode()[..],
        frame.metadata.as_bytes(),
        &frame.body,
    ]
    .concat()
}

/// Flushes `frames`, cancelling at `Pending`s as `cancels` says and queuing
/// the next batch between attempts. The bytes written, what was expected,
/// and whether a flush failed.
fn write_all(
    batches: Vec<Vec<Frame>>,
    steps: Vec<WriteStep>,
    cancels: Vec<bool>,
) -> (Vec<u8>, Vec<u8>, Option<Error>) {
    let mut cancels = VecDeque::from(cancels);
    let mut conn = Connection::new(Sink {
        written: Vec::new(),
        steps: steps.into(),
    });
    let mut expected = Vec::new();
    let mut failed = None;
    for batch in batches {
        for frame in batch {
            expected.extend(encoded(&frame));
            conn.queue(frame);
        }
        if let Err(e) = drive!(conn, cancels, flush) {
            failed = Some(e);
            break;
        }
    }
    if failed.is_some() {
        // Every later flush fails too, whatever is queued.
        conn.queue(interleaved());
        assert!(matches!(drive!(conn, cancels, flush), Err(Error::Broken)));
    }
    (conn.get_ref().written.clone(), expected, failed)
}

#[hegel::test_helper]
fn batches(tc: &TestCase) -> Vec<Vec<Frame>> {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    (0..count)
        .map(|_| {
            let frames = tc.draw(gs::integers::<usize>().max_value(3));
            (0..frames).map(|_| outgoing(tc)).collect()
        })
        .collect()
}

#[hegel::test_helper]
fn write_steps(tc: &TestCase, faults: bool) -> Vec<WriteStep> {
    let count = tc.draw(gs::integers::<usize>().max_value(40));
    (0..count)
        .map(|_| {
            let kind = tc.draw(gs::integers::<u8>().max_value(if faults { 9 } else { 8 }));
            match kind {
                0..=2 => WriteStep::Pending,
                3..=8 => {
                    WriteStep::Accept(tc.draw(gs::integers::<usize>().min_value(1).max_value(40)))
                }
                // An error, or a write that takes nothing.
                _ if tc.draw(gs::booleans()) => WriteStep::Accept(0),
                _ => WriteStep::Fail(tc.draw(gs::sampled_from(INJECTED.to_vec()))),
            }
        })
        .collect()
}

/// However the stream takes the bytes and however often a flush is
/// cancelled, exactly the queued frames are written, in order; and however
/// those bytes then arrive and however often reading them is cancelled,
/// they read back as the same frames.
#[hegel::test(test_cases = 1000)]
fn round_trips_through_any_stream(tc: TestCase) {
    let batches = batches(&tc);
    let sent: Vec<(Header, Vec<u8>, Vec<u8>)> = batches
        .iter()
        .flatten()
        .map(|f| (f.header, f.metadata.as_bytes().to_vec(), f.body.clone()))
        .collect();
    let writing = write_steps(&tc, false);
    let cancels = tc.draw(gs::vecs(gs::booleans()).max_size(40));
    let (written, expected, failed) = write_all(batches, writing, cancels);
    assert!(failed.is_none());
    assert_eq!(written, expected);

    let reading = steps(&tc, false);
    let cancels = tc.draw(gs::vecs(gs::booleans()).max_size(40));
    let mut cancels = VecDeque::from(cancels);
    let mut conn = Connection::new(Scripted {
        data: written,
        pos: 0,
        steps: reading.into(),
    });
    for (header, metadata, body) in sent {
        let head = drive!(conn, cancels, next_head).unwrap().unwrap();
        assert_eq!(head.header, header);
        assert_eq!(head.metadata.as_bytes(), &metadata[..]);
        assert_eq!(drive!(conn, cancels, read_body, MAX_BODY).unwrap(), body);
    }
    assert!(drive!(conn, cancels, next_head).unwrap().is_none());
}

/// An error from the stream, or a write that takes nothing, fails the flush
/// it hits and every one after, having written a prefix of the queued
/// frames.
#[hegel::test(test_cases = 1000)]
fn write_errors_fail_cleanly(tc: TestCase) {
    let batches = batches(&tc);
    let steps = write_steps(&tc, true);
    let cancels = tc.draw(gs::vecs(gs::booleans()).max_size(40));
    let (written, expected, failed) = write_all(batches, steps, cancels);
    assert!(expected.starts_with(&written));
    match failed {
        None => assert_eq!(written, expected),
        Some(Error::WriteZero) => {}
        Some(Error::Io(e)) => assert!(INJECTED.contains(&e.kind()), "{e}"),
        Some(e) => panic!("unexpected error {e:?}"),
    }
}
