use crate::{Buffer, socket::Socket};
use std::io;

pub struct ReadHalf<'a> {
    pub(super) socket: &'a Socket,
}
impl ReadHalf<'_> {
    pub async fn read(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, false).await
    }
}
