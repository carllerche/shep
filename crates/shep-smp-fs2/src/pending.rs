use shep_smp_io2::{Completion, Io, OperationHandle};
use std::future::poll_fn;

pub(super) async fn finish(io: &Io, handle: OperationHandle) -> Completion {
    let mut pending = Pending { io, handle };
    poll_fn(|cx| pending.handle.poll_complete(pending.io, cx)).await
}
struct Pending<'a> {
    io: &'a Io,
    handle: OperationHandle,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.handle.release(self.io);
    }
}
