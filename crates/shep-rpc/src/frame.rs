//! Reading and writing frames, shared by both ends.

use std::io;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::wire::{HEADER_SIZE, Header, payload_checksum};

/// Socket buffer size for both directions.
pub(crate) const IO_BUFFER: usize = 64 * 1024;

/// The most frames written between flushes, so one busy connection still
/// flushes regularly.
const MAX_BATCH: usize = 512;

/// One frame queued for the writer.
pub(crate) struct Frame {
    pub(crate) header: Header,
    pub(crate) body: Bytes,
    /// Server memory reserved for the request this answers; released once the
    /// response is on the socket.
    pub(crate) _permit: Option<OwnedSemaphorePermit>,
}

/// Writes frames in queue order until every sender is gone, flushing after
/// each batch of whatever was queued. `keep` drops a frame unwritten (the
/// client uses it to skip calls that gave up before their turn).
pub(crate) async fn write_frames<W: AsyncWrite + Unpin>(
    mut rx: mpsc::UnboundedReceiver<Frame>,
    writer: W,
    mut keep: impl FnMut(&Frame) -> bool,
) -> io::Result<()> {
    let mut writer = BufWriter::with_capacity(IO_BUFFER, writer);
    let mut batch = Vec::new();
    while let Some(frame) = rx.recv().await {
        batch.push(frame);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(frame) => batch.push(frame),
                Err(_) => break,
            }
        }
        for frame in &batch {
            if keep(frame) {
                writer.write_all(&frame.header.encode()).await?;
                writer.write_all(&frame.body).await?;
            }
        }
        writer.flush().await?;
        batch.clear();
    }
    writer.shutdown().await
}

/// The next header, or `None` at a clean end of stream.
pub(crate) async fn read_header<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Header>> {
    let mut buf = [0; HEADER_SIZE];
    match reader.read_exact(&mut buf).await {
        Ok(_) => Ok(Some(Header::decode(&buf))),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads `header`'s body and checks it against `payload_checksum`.
pub(crate) async fn read_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    header: &Header,
) -> io::Result<Bytes> {
    let mut body = BytesMut::zeroed(header.payload_size as usize);
    reader.read_exact(&mut body).await?;
    if payload_checksum(&body) != header.payload_checksum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload checksum mismatch",
        ));
    }
    Ok(body.freeze())
}

/// Reads and discards `len` bytes.
pub(crate) async fn skip<R: AsyncRead + Unpin>(reader: &mut R, len: u32) -> io::Result<()> {
    let skipped = tokio::io::copy(&mut reader.take(len as u64), &mut tokio::io::sink()).await?;
    if skipped < len as u64 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}
