use crate::io::Shutdown;
use crate::net::{Buffer, Write, WriteAll, socket::Socket};
use std::io;

pub struct WriteHalf<'a> {
    pub(super) socket: &'a Socket,
}
impl WriteHalf<'_> {
    pub fn write(&mut self, buffer: Buffer) -> Write<'_> {
        Write::new(self.socket, buffer)
    }
    /// Send the full buffer. Errors/cancellation may follow partial sends.
    pub fn write_all(&mut self, buffer: Buffer) -> WriteAll<'_> {
        WriteAll::new(self.socket, buffer)
    }
    /// Check connection state; does not wait for abandoned writes.
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
            .run(Shutdown {
                socket: self.socket.file.clone(),
                how: std::net::Shutdown::Write,
            })
            .map_err(|r| r.error)?
            .await
            .result?;
        self.socket.write_closed.set(true);
        Ok(())
    }
}
