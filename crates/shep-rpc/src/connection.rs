//! A connection: frames read from and written to a byte stream.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Poll, ready};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::metadata::{Metadata, MetadataError};
use crate::wire::{HEADER_SIZE, Header, MAX_METADATA, payload_checksum};

/// A good buffer size for the `BufReader` and `BufWriter` under a
/// connection's halves.
pub const IO_BUFFER: usize = 64 * 1024;

/// A frame to send.
pub struct Frame {
    pub header: Header,
    pub metadata: Metadata,
    pub body: Bytes,
}

impl Frame {
    /// An uncompressed frame, both checksums filled in.
    pub fn new(
        version: u8,
        method_or_status: u32,
        correlation_id: u32,
        metadata: Metadata,
        body: Bytes,
    ) -> Frame {
        let checksum = payload_checksum(&body);
        Frame::with_checksum(
            version,
            method_or_status,
            correlation_id,
            metadata,
            body,
            checksum,
        )
    }

    /// Like [`Frame::new`], with the body's checksum already computed, so
    /// callers can hash outside a lock.
    pub fn with_checksum(
        version: u8,
        method_or_status: u32,
        correlation_id: u32,
        metadata: Metadata,
        body: Bytes,
        payload_checksum: u64,
    ) -> Frame {
        let header = Header::new(
            version,
            method_or_status,
            correlation_id,
            metadata.as_bytes(),
            body.len(),
            payload_checksum,
        );
        Frame {
            header,
            metadata,
            body,
        }
    }
}

/// A frame's header and metadata, checked; its body not yet read.
#[derive(Debug)]
pub struct Head {
    pub header: Header,
    /// Malformed metadata under a good checksum: the sizes can be trusted, so
    /// the receiver can skip the body and carry on.
    pub metadata: Result<Metadata, MetadataError>,
}

/// Bytes skipped per read, from a buffer on the stack.
const SKIP_CHUNK: usize = 8 * 1024;

/// Frames over a byte stream `T`: read when `T` is [`AsyncRead`], written
/// when it is [`AsyncWrite`], both when it is both. Give each direction its
/// own connection over a split stream to read and write at once.
///
/// **Reading:** each frame's head with [`next_head`](Connection::next_head),
/// then its body with [`read_body`](Connection::read_body) or
/// [`skip_body`](Connection::skip_body). It never holds more than one
/// frame's head (the header and at most [`MAX_METADATA`] bytes of metadata)
/// or one body (at most the limit given to `read_body`), and allocates for a
/// size only after checking it.
///
/// **Writing:** [`queue`](Connection::queue) frames, then
/// [`flush`](Connection::flush) them. Bodies are written from their `Bytes`
/// without copying; put a `BufWriter` under the connection to batch small
/// writes.
///
/// **Every async method is cancel-safe.** The connection keeps its progress
/// in itself, not in the future, so dropping a call's future loses nothing:
/// calling the same method again picks up where it stopped.
///
/// Once a read fails, the stream can't be trusted to be in step, and every
/// later read fails too; the same goes for writes.
pub struct Connection<T> {
    io: T,
    read: Read,
    write: Write,
}

/// Where reading stands.
enum Read {
    /// Reading a head: `buf` holds the header, then the header and its
    /// metadata, `filled` bytes of it read.
    Head { buf: Vec<u8>, filled: usize },
    /// A head was returned; its body is next. `buf` is empty until
    /// `read_body` checks the size and allocates for it.
    Body {
        header: Header,
        buf: Vec<u8>,
        filled: usize,
    },
    /// Skipping a body, `remaining` bytes of it left.
    Skipping { remaining: usize },
    /// A call failed; the stream is out of step.
    Broken,
}

impl Read {
    fn next_head() -> Read {
        Read::Head {
            buf: vec![0; HEADER_SIZE],
            filled: 0,
        }
    }
}

/// Where writing stands: frames queued, `pos` bytes of the first written.
#[derive(Default)]
struct Write {
    queue: VecDeque<Frame>,
    pos: usize,
    broken: bool,
}

impl<T> Connection<T> {
    pub fn new(io: T) -> Connection<T> {
        Connection {
            io,
            read: Read::next_head(),
            write: Write::default(),
        }
    }

    pub fn get_ref(&self) -> &T {
        &self.io
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }
}

impl<T: AsyncRead + Unpin> Connection<T> {
    /// The next frame's header and metadata, or `None` at a clean end of
    /// stream (one that ends between frames). A body left unread is skipped
    /// first.
    ///
    /// Fails, and the stream can't be read further, if the header claims more
    /// than [`MAX_METADATA`] bytes of metadata, its checksum doesn't match,
    /// its flags aren't zero, or the stream ends mid-frame.
    pub async fn next_head(&mut self) -> io::Result<Option<Head>> {
        let result = self.next_head_inner().await;
        self.break_on_error(result)
    }

    /// Reads the current frame's body and checks it against its checksum.
    /// The caller decides whether the body is acceptable first; a body over
    /// `max` fails here, before anything is allocated for it.
    pub async fn read_body(&mut self, max: usize) -> io::Result<Bytes> {
        let result = self.read_body_inner(max).await;
        self.break_on_error(result)
    }

    /// Reads past the current frame's body without holding it. Does nothing
    /// if there's no body to skip: the last one was already read or skipped.
    pub async fn skip_body(&mut self) -> io::Result<()> {
        let result = self.skip_body_inner().await;
        self.break_on_error(result)
    }

    fn break_on_error<U>(&mut self, result: io::Result<U>) -> io::Result<U> {
        if result.is_err() {
            self.read = Read::Broken;
        }
        result
    }

    async fn next_head_inner(&mut self) -> io::Result<Option<Head>> {
        if matches!(self.read, Read::Body { .. } | Read::Skipping { .. }) {
            self.skip_body_inner().await?;
        }
        let Read::Head { buf, filled } = &mut self.read else {
            return Err(broken());
        };
        if buf.len() == HEADER_SIZE {
            if !fill(&mut self.io, buf, filled).await? {
                if *filled == 0 {
                    return Ok(None);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let header = Header::decode(buf[..HEADER_SIZE].try_into().unwrap());
            if header.metadata_size as usize > MAX_METADATA {
                return Err(invalid("metadata larger than MAX_METADATA"));
            }
            buf.resize(HEADER_SIZE + header.metadata_size as usize, 0);
        }
        if !fill(&mut self.io, buf, filled).await? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let header = Header::decode(buf[..HEADER_SIZE].try_into().unwrap());
        let metadata = &buf[HEADER_SIZE..];
        // Past a bad header the stream can't be trusted to be in step.
        if !header.checksum_ok(metadata) {
            return Err(invalid("header checksum mismatch"));
        }
        if header.flags != 0 {
            return Err(invalid("unknown flags"));
        }
        let metadata = Metadata::parse(Bytes::copy_from_slice(metadata));
        self.read = Read::Body {
            header,
            buf: Vec::new(),
            filled: 0,
        };
        Ok(Some(Head { header, metadata }))
    }

    async fn read_body_inner(&mut self, max: usize) -> io::Result<Bytes> {
        let Read::Body {
            header,
            buf,
            filled,
        } = &mut self.read
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no frame body to read",
            ));
        };
        let size = header.payload_size as usize;
        if buf.len() < size {
            if size > max {
                return Err(invalid("frame body larger than the limit"));
            }
            *buf = vec![0; size];
        }
        if !fill(&mut self.io, buf, filled).await? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if payload_checksum(buf) != header.payload_checksum {
            return Err(invalid("payload checksum mismatch"));
        }
        let body = Bytes::from(std::mem::take(buf));
        self.read = Read::next_head();
        Ok(body)
    }

    async fn skip_body_inner(&mut self) -> io::Result<()> {
        if let Read::Body { header, filled, .. } = &self.read {
            // A read cancelled partway through may already hold some of it.
            let remaining = header.payload_size as usize - filled;
            self.read = Read::Skipping { remaining };
        }
        let remaining = match &mut self.read {
            Read::Skipping { remaining } => remaining,
            Read::Head { .. } => return Ok(()),
            Read::Body { .. } => unreachable!("turned into Skipping above"),
            Read::Broken => return Err(broken()),
        };
        while *remaining > 0 {
            let want = (*remaining).min(SKIP_CHUNK);
            let n = std::future::poll_fn(|cx| {
                let mut scratch = [0; SKIP_CHUNK];
                let mut read = ReadBuf::new(&mut scratch[..want]);
                ready!(Pin::new(&mut self.io).poll_read(cx, &mut read))?;
                Poll::Ready(Ok::<_, io::Error>(read.filled().len()))
            })
            .await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            *remaining -= n;
        }
        self.read = Read::next_head();
        Ok(())
    }
}

impl<T: AsyncWrite + Unpin> Connection<T> {
    /// Queues `frame` to be written by the next [`flush`](Connection::flush).
    pub fn queue(&mut self, frame: Frame) {
        self.write.queue.push_back(frame);
    }

    /// Frames queued and not yet entirely written.
    pub fn queued(&self) -> usize {
        self.write.queue.len()
    }

    /// Writes every queued frame, then flushes the stream.
    pub async fn flush(&mut self) -> io::Result<()> {
        if self.write.broken {
            return Err(broken());
        }
        let result = self.flush_inner().await;
        if result.is_err() {
            self.write.broken = true;
        }
        result
    }

    /// Flushes, then shuts down the stream's write side.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.flush().await?;
        std::future::poll_fn(|cx| Pin::new(&mut self.io).poll_shutdown(cx)).await
    }

    async fn flush_inner(&mut self) -> io::Result<()> {
        let Connection { io, write, .. } = self;
        while let Some(frame) = write.queue.front() {
            let header = frame.header.encode();
            let parts: [&[u8]; 3] = [&header, frame.metadata.as_bytes(), &frame.body];
            let len: usize = parts.iter().map(|p| p.len()).sum();
            while write.pos < len {
                // The part `pos` falls in, and how far into it.
                let (mut part, mut at) = (0, write.pos);
                while at >= parts[part].len() {
                    at -= parts[part].len();
                    part += 1;
                }
                let rest = &parts[part][at..];
                let n = std::future::poll_fn(|cx| Pin::new(&mut *io).poll_write(cx, rest)).await?;
                if n == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                write.pos += n;
            }
            write.queue.pop_front();
            write.pos = 0;
        }
        std::future::poll_fn(|cx| Pin::new(&mut *io).poll_flush(cx)).await
    }
}

/// Reads into `buf` from `filled` until it's full, recording progress in
/// `filled` after every read, so a cancelled call loses nothing. False if the
/// stream ended first.
async fn fill<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
    filled: &mut usize,
) -> io::Result<bool> {
    while *filled < buf.len() {
        let n = std::future::poll_fn(|cx| {
            let mut read = ReadBuf::new(&mut buf[*filled..]);
            ready!(Pin::new(&mut *reader).poll_read(cx, &mut read))?;
            Poll::Ready(Ok::<_, io::Error>(read.filled().len()))
        })
        .await?;
        if n == 0 {
            return Ok(false);
        }
        *filled += n;
    }
    Ok(true)
}

fn broken() -> io::Error {
    io::Error::other("an earlier call failed; the stream is out of step")
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
