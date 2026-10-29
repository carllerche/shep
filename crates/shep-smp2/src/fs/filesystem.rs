use crate::fs::{File, OpenOptions};
use crate::io::{Fd, Operation, Value};
use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::PathBuf, rc::Rc};

impl File {
    /// Open in the current runtime, then discover direct-I/O constraints on the helper.
    /// Like ordinary open, cancellation or a later alignment-query error can
    /// follow creation/truncation. They do not roll back filesystem effects.
    ///
    /// # Panics
    /// Panics when first polled outside a runtime context.
    pub async fn open(path: impl Into<PathBuf>, options: OpenOptions) -> io::Result<File> {
        let io = crate::Handle::current().io();
        let path = path.into();
        let flags = options.flags()?;
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let handle = io
            .push(Operation::OpenAt {
                directory: None,
                path: name,
                flags,
                mode: 0o666,
            })
            .map_err(|r| r.error)?;
        let completion = handle.await;
        let Value::File(file) = completion.result? else {
            unreachable!("open result")
        };
        let file = Rc::new(file);
        let alignment = if options.direct {
            let query = file.as_file().try_clone()?;
            io.shared
                .worker
                .run(move || direct_alignment(&query))?
                .await?
        } else {
            1
        };
        Ok(File::new(io.clone(), file, alignment))
    }
    /// Import into the current runtime after querying its flags and, for O_DIRECT, alignment.
    /// Do not retain aliases that change flags or perform conflicting I/O.
    ///
    /// # Panics
    /// Panics when first polled outside a runtime context.
    pub async fn from_std(file: std::fs::File) -> io::Result<File> {
        let io = crate::Handle::current().io();
        let (file, alignment) = io
            .shared
            .worker
            .run(move || {
                let flags = shep_sys::file_flags(&file)?;
                let alignment = if flags & libc::O_DIRECT != 0 {
                    direct_alignment(&file)?
                } else {
                    1
                };
                Ok((file, alignment))
            })?
            .await?;
        Ok(File::new(io.clone(), Rc::new(Fd::new(file)?), alignment))
    }
}

pub async fn create_dir_all(path: impl Into<PathBuf>) -> io::Result<()> {
    let io = crate::Handle::current().io();
    let path = path.into();
    io.shared
        .worker
        .run(move || std::fs::create_dir_all(path))?
        .await
}
pub async fn read_dir(path: impl Into<PathBuf>) -> io::Result<Vec<PathBuf>> {
    let io = crate::Handle::current().io();
    let path = path.into();
    io.shared
        .worker
        .run(move || {
            std::fs::read_dir(path)?
                .map(|e| e.map(|e| e.path()))
                .collect()
        })?
        .await
}
pub async fn remove_file(path: impl Into<PathBuf>) -> io::Result<()> {
    let io = crate::Handle::current().io();
    let path = path.into();
    io.shared
        .worker
        .run(move || std::fs::remove_file(path))?
        .await
}
pub async fn rename(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> io::Result<()> {
    let io = crate::Handle::current().io();
    let (from, to) = (from.into(), to.into());
    io.shared
        .worker
        .run(move || std::fs::rename(from, to))?
        .await
}
/// Persist namespace changes; call after the dependent rename/create/unlink.
pub async fn sync_dir(path: impl Into<PathBuf>) -> io::Result<()> {
    let io = crate::Handle::current().io();
    let path = path.into();
    io.shared
        .worker
        .run(move || {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(path)?
                .sync_all()
        })?
        .await
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
