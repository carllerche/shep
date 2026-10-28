use crate::fs::{activity::Activity, helper};
use std::{cell::RefCell, io, rc::Rc, sync::Arc};

pub(crate) struct Metadata {
    pub(crate) activity: Arc<Activity>,
    pub(crate) helper: RefCell<Option<helper::Helper>>,
}
impl Metadata {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            activity: Arc::default(),
            helper: RefCell::new(None),
        })
    }
    pub(crate) fn shutdown(&self) {
        self.activity.close();
        let helper = self.helper.borrow_mut().take();
        drop(helper);
    }
    pub(crate) fn metadata<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<helper::ResultFuture<T>> {
        let guard = self.activity.begin()?;
        let mut helper = self.helper.borrow_mut();
        if helper.is_none() {
            *helper = Some(helper::Helper::new(64)?);
        }
        helper.as_ref().unwrap().run(move || {
            let _guard = guard;
            work()
        })
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn driver_drain_finishes_abandoned_metadata_and_closes_admission() {
        let mut driver = crate::io::Driver::new(2).unwrap();
        let io = driver.io().clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let metadata = &io.shared.metadata;
        let running = metadata
            .metadata(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        entered_rx.recv().unwrap();
        for _ in 0..64 {
            let done = done_tx.clone();
            drop(
                metadata
                    .metadata(move || {
                        done.send(()).unwrap();
                        Ok(())
                    })
                    .unwrap(),
            );
        }
        assert_eq!(
            metadata.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(running);
        release_tx.send(()).unwrap();
        driver.drain().unwrap();
        assert_eq!(done_rx.try_iter().count(), 64);
        assert_eq!(
            metadata.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(metadata.helper.borrow().is_none());
    }
}
