use crate::{Config, File, OpenOptions, domain::Domain, pending::finish};
use shep_smp_io2::{Fd, Io, Operation, Value};
use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::PathBuf, rc::Rc};

/// A filesystem admission/drain domain bound to one local driver.
/// Clones and opened files share its bounded helper and activity accounting.
#[derive(Clone)]
pub struct Fs {
    io: Io,
    domain: Rc<Domain>,
}
impl Fs {
    pub fn new(io: Io, config: Config) -> io::Result<Self> {
        if config.metadata_queue == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Self {
            io,
            domain: Domain::new(config),
        })
    }
    pub fn io(&self) -> &Io {
        &self.io
    }
    /// Open through io2, then discover direct-I/O constraints on the helper.
    /// Like ordinary open, cancellation or a later alignment-query error can
    /// follow creation/truncation. They do not roll back filesystem effects.
    pub async fn open(&self, path: impl Into<PathBuf>, options: OpenOptions) -> io::Result<File> {
        let path = path.into();
        let flags = options.flags()?;
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let guard = self.domain.activity.begin()?;
        let handle = self
            .io
            .push_with_guard(
                Operation::OpenAt {
                    directory: None,
                    path: name,
                    flags,
                    mode: 0o666,
                },
                guard,
            )
            .map_err(|r| r.error)?;
        let completion = finish(&self.io, handle).await;
        let Value::File(file) = completion.result? else {
            unreachable!("open result")
        };
        let file = Rc::new(file);
        let alignment = if options.direct {
            let query = file.as_file().try_clone()?;
            self.domain
                .metadata(move || direct_alignment(&query))?
                .await?
        } else {
            1
        };
        Ok(File::new(
            self.io.clone(),
            self.domain.clone(),
            file,
            path,
            alignment,
        ))
    }
    /// Import a descriptor after querying its flags and, for O_DIRECT, alignment.
    /// Do not retain aliases that change flags or perform conflicting I/O.
    pub async fn from_std(&self, file: std::fs::File) -> io::Result<File> {
        let (file, alignment) = self
            .domain
            .metadata(move || {
                let flags = shep_sys::file_flags(&file)?;
                let alignment = if flags & libc::O_DIRECT != 0 {
                    direct_alignment(&file)?
                } else {
                    1
                };
                Ok((file, alignment))
            })?
            .await?;
        Ok(File::new(
            self.io.clone(),
            self.domain.clone(),
            Rc::new(Fd::new(file)?),
            PathBuf::new(),
            alignment,
        ))
    }
    pub async fn create_dir_all(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || std::fs::create_dir_all(path))?
            .await
    }
    pub async fn read_dir(&self, path: impl Into<PathBuf>) -> io::Result<Vec<PathBuf>> {
        let path = path.into();
        self.domain
            .metadata(move || {
                std::fs::read_dir(path)?
                    .map(|e| e.map(|e| e.path()))
                    .collect()
            })?
            .await
    }
    pub async fn remove_file(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || std::fs::remove_file(path))?
            .await
    }
    pub async fn rename(&self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> io::Result<()> {
        let (from, to) = (from.into(), to.into());
        self.domain
            .metadata(move || std::fs::rename(from, to))?
            .await
    }
    /// Persist namespace changes; call after the dependent rename/create/unlink.
    pub async fn sync_dir(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY)
                    .open(path)?
                    .sync_all()
            })?
            .await
    }
    /// Wait for this domain's accepted kernel and helper work, without syncing.
    /// Quiesce producers first for a stable drain. Other driver users are unaffected.
    pub async fn drain(&self) {
        self.domain.activity.idle().await;
    }
    /// Stop domain admission (including opened files), then finish accepted work.
    /// The shared io2 driver remains open for other filesystem/network domains.
    pub async fn shutdown(&self) {
        self.domain.activity.close();
        self.drain().await;
        self.domain.helper.borrow_mut().take();
    }
}

fn direct_alignment(file: &std::fs::File) -> io::Result<usize> {
    match shep_sys::direct_io_alignment(file) {
        Ok(Some((memory, offset))) => {
            let memory = memory as usize;
            let offset = offset as usize;
            if !memory.is_power_of_two() || !offset.is_power_of_two() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported direct-I/O alignment",
                ));
            }
            Ok(memory.max(offset))
        }
        // Match the existing file layer's conservative fallback when the
        // filesystem/kernel cannot report DIOALIGN. Kernel errors remain visible.
        Ok(None) => Ok(4096),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
            ) =>
        {
            Ok(4096)
        }
        Err(error) => Err(error),
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::helper;
    use std::{
        future::Future,
        pin::Pin,
        sync::mpsc,
        task::{Context, Poll, Waker},
        time::{Duration, Instant},
    };
    fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn bounded_helper_keeps_canceled_work_in_shutdown_and_survives_panics() {
        let driver = shep_smp_io2::Driver::new(2).unwrap();
        let fs = Fs::new(driver.io().clone(), Config { metadata_queue: 1 }).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let running = fs
            .domain
            .metadata(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let queued = fs
            .domain
            .metadata(move || {
                completed_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fs.domain.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(running);
        drop(queued);
        let mut shutdown = Box::pin(fs.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert_eq!(
            fs.domain.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while poll(shutdown.as_mut()).is_pending() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        completed_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let helper = helper::Helper::new(1).unwrap();
        let mut failed = Box::pin(
            helper
                .run(|| -> io::Result<()> { panic!("helper work failed") })
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Poll::Ready(result) = poll(failed.as_mut()) {
                assert!(result.is_err());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        // The worker remains usable after a job panic.
        let mut next = Box::pin(helper.run(|| Ok(42)).unwrap());
        loop {
            if let Poll::Ready(result) = poll(next.as_mut()) {
                assert_eq!(result.unwrap(), 42);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
