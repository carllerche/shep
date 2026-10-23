// Adapted from Monoio's task/utils.rs; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::cell::UnsafeCell;

pub(crate) trait UnsafeCellExt<T> {
    fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R;
    fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R;
}

impl<T> UnsafeCellExt<T> for UnsafeCell<T> {
    fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.get())
    }

    fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.get())
    }
}
