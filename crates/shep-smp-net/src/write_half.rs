use crate::{Buffer, socket::Socket};
use shep_smp_io2::Operation;
use std::io;

pub struct WriteHalf<'a> {
    pub(super) socket: &'a Socket,
}
impl WriteHalf<'_> {
    pub async fn write(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, true).await
    }
    pub async fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        self.socket.check()?;
        while !bytes.is_empty() {
            let len = bytes.len().min(self.socket.config.max_buffer_size);
            let buffer = Buffer::from_slice(&bytes[..len], 1)?;
            let (result, _) = self.write(buffer).await;
            let count = result?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "TCP send returned zero",
                ));
            }
            bytes = &bytes[count..];
        }
        Ok(())
    }
    /// There is no adapter backlog; each write awaits its own completion.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.socket.check()
    }
    /// Send FIN after prior writes complete, retaining the read direction.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.socket.check()?;
        if self.socket.write_closed.get() {
            return Ok(());
        }
        self.socket
            .run(Operation::Shutdown {
                socket: self.socket.file.clone(),
                how: std::net::Shutdown::Write,
            })
            .await
            .map_err(|r| r.error)?
            .result?;
        self.socket.write_closed.set(true);
        Ok(())
    }
}
