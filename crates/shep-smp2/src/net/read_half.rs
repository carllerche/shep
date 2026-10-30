use crate::net::{Buffer, Read, socket::Socket};

pub struct ReadHalf<'a> {
    pub(super) socket: &'a Socket,
}
impl ReadHalf<'_> {
    pub fn read(&mut self, buffer: Buffer) -> Read<'_> {
        Read::new(self.socket, buffer)
    }
}
