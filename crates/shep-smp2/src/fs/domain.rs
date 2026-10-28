use crate::fs::{Config, activity::Activity, helper};
use std::{cell::RefCell, io, rc::Rc, sync::Arc};

pub(super) struct Domain {
    pub(super) activity: Arc<Activity>,
    pub(super) helper: RefCell<Option<helper::Helper>>,
    config: Config,
}
impl Domain {
    pub(super) fn new(config: Config) -> Rc<Self> {
        Rc::new(Self {
            activity: Arc::default(),
            helper: RefCell::new(None),
            config,
        })
    }
    pub(super) fn metadata<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<helper::ResultFuture<T>> {
        let guard = self.activity.begin()?;
        let mut helper = self.helper.borrow_mut();
        if helper.is_none() {
            *helper = Some(helper::Helper::new(self.config.metadata_queue)?);
        }
        helper.as_ref().unwrap().run(move || {
            let _guard = guard;
            work()
        })
    }
}
