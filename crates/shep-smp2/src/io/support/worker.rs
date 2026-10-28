use super::helper;
use std::{
    cell::{Cell, RefCell},
    io,
};

pub(crate) struct Worker {
    closed: Cell<bool>,
    helper: RefCell<Option<helper::Helper>>,
}
impl Worker {
    pub(crate) fn new() -> Self {
        Self {
            closed: Cell::new(false),
            helper: RefCell::new(None),
        }
    }
    pub(crate) fn close(&self) {
        self.closed.set(true);
    }
    pub(crate) fn shutdown(&self) {
        self.close();
        let helper = self.helper.borrow_mut().take();
        drop(helper);
    }
    pub(crate) fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<helper::ResultFuture<T>> {
        if self.closed.get() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let mut helper = self.helper.borrow_mut();
        if helper.is_none() {
            *helper = Some(helper::Helper::new(64)?);
        }
        helper.as_ref().unwrap().run(work)
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn driver_drain_finishes_abandoned_worker_and_closes_admission() {
        let mut driver = crate::io::Driver::new(2).unwrap();
        let io = driver.io().clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = &io.shared.worker;
        let running = worker
            .run(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        entered_rx.recv().unwrap();
        for _ in 0..64 {
            let done = done_tx.clone();
            drop(
                worker
                    .run(move || {
                        done.send(()).unwrap();
                        Ok(())
                    })
                    .unwrap(),
            );
        }
        assert_eq!(
            worker.run(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(running);
        release_tx.send(()).unwrap();
        driver.drain().unwrap();
        assert_eq!(done_rx.try_iter().count(), 64);
        assert_eq!(
            worker.run(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(worker.helper.borrow().is_none());
    }
}
