/// Limits for the filesystem's lazily started metadata worker.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum queued helper jobs, in addition to one executing job.
    /// Full queues reject with WouldBlock; rejected work has no effects.
    pub metadata_queue: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self { metadata_queue: 64 }
    }
}
