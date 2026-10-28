use crate::io::Operation;
use crate::net::{Buffer, socket::Socket};
use std::io;

pub struct WriteHalf<'a> {
    pub(super) socket: &'a Socket,
}
impl WriteHalf<'_> {
    pub async fn write(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, true).await
    }
    /// Send the entire owned buffer without copying, returning it on success or error.
    /// Each send is bounded by Config::max_buffer_size. Errors may follow partial
    /// progress; retrying the whole buffer can duplicate bytes already sent.
    pub async fn write_all(&mut self, mut buffer: Buffer) -> (io::Result<()>, Buffer) {
        if let Err(error) = self.socket.check() {
            return (Err(error), buffer);
        }
        if self.socket.write_closed.get() {
            return (
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "write direction closed",
                )),
                buffer,
            );
        }
        let mut offset = 0;
        while offset < buffer.len() {
            let len = (buffer.len() - offset).min(self.socket.config.max_buffer_size);
            let operation = Operation::SendRange {
                socket: self.socket.file.clone(),
                buffer,
                range: offset..offset + len,
            };
            let result;
            match self.socket.run(operation).await {
                Ok(completion) => {
                    result = completion.result.map(crate::io::Value::into_count);
                    buffer = completion.operation.into_buffer().unwrap();
                }
                Err(rejected) => {
                    return (
                        Err(rejected.error),
                        rejected.operation.into_buffer().unwrap(),
                    );
                }
            }
            match result {
                Ok(0) => {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "TCP send returned zero",
                        )),
                        buffer,
                    );
                }
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return (Err(error), buffer),
            }
        }
        (Ok(()), buffer)
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
