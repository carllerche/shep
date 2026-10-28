use crate::Buffer;
use std::io;

/// Positional file contract shared by raw files, schedulers and appenders.
/// Metadata mutation requires caller-managed exclusion. Independent requests
/// have no implicit ordering; await writes before syncing their durability.
#[allow(async_fn_in_trait)]
pub trait FileIo: Clone + 'static {
    fn alignment(&self) -> usize;
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    async fn sync_data(&self) -> io::Result<()>;
    async fn sync_all(&self) -> io::Result<()>;
    /// Reserve backing space without changing logical length.
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()>;
    async fn set_len(&self, len: u64) -> io::Result<()>;
    async fn len(&self) -> io::Result<u64>;
    async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
}
