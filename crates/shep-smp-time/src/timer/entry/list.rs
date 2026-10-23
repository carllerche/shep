// Extracted from Monoio and specialized for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Non-owning intrusive lists of pinned timer entries.
//!
//! Only insertion and removal of a supplied pointer require unsafe callers.
//! Insertion establishes the liveness/pinning invariant used by safe popping.
//! The owning wheel serializes link mutations; no operation invokes user code.

use std::ptr::NonNull;

use super::{TimerHandle, TimerShared};

/// One wheel bucket or its pending queue. Does not own or drop its entries.
#[derive(Debug, Default)]
pub(in crate::timer) struct EntryList {
    head: Option<NonNull<TimerShared>>,
    tail: Option<NonNull<TimerShared>>,
}

impl EntryList {
    pub(in crate::timer) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    /// Add a detached entry at the front.
    ///
    /// # Safety
    ///
    /// The entry must be live, pinned, and detached from every list. Its owner
    /// must keep it live and immovable until removal. The caller must serialize
    /// link changes with the owning wheel; no user callbacks may run meanwhile.
    pub(in crate::timer) unsafe fn push_front(&mut self, entry: TimerHandle) {
        let node = entry.inner;
        assert_ne!(self.head, Some(node));
        // SAFETY: The caller supplies a live pinned detached node. Existing
        // neighbors satisfy the same invariant, established at their insertion.
        // Only shared Cell fields are accessed; no mutable node references form.
        unsafe {
            let links = &node.as_ref().wheel_state;
            links.next.set(self.head);
            links.prev.set(None);
            if let Some(head) = self.head {
                head.as_ref().wheel_state.prev.set(Some(node));
            }
            self.head = Some(node);
            if self.tail.is_none() {
                self.tail = Some(node);
            }
        }
    }

    /// Detach the tail, returning a non-owning handle to it.
    pub(in crate::timer) fn pop_back(&mut self) -> Option<TimerHandle> {
        let node = self.tail?;
        // SAFETY: Insertion requires owners to retain every linked entry live
        // and pinned. Detach the tail before returning its raw handle. No user
        // callback runs here, and all link writes use shared Cell access.
        unsafe {
            let links = &node.as_ref().wheel_state;
            self.tail = links.prev.get();
            if let Some(prev) = self.tail {
                prev.as_ref().wheel_state.next.set(None);
            } else {
                self.head = None;
            }
            links.prev.set(None);
            links.next.set(None);
        }
        Some(TimerHandle { inner: node })
    }

    pub(in crate::timer) fn is_empty(&self) -> bool {
        if self.head.is_some() {
            return false;
        }
        assert!(self.tail.is_none());
        true
    }

    /// Remove a supplied entry if it belongs to this list.
    ///
    /// # Safety
    ///
    /// The node must be live and pinned, and either belong to this list or be
    /// detached from every list. The caller must serialize link changes with
    /// the owning wheel. A node linked into a different list is not permitted.
    pub(in crate::timer) unsafe fn remove(
        &mut self,
        node: NonNull<TimerShared>,
    ) -> Option<TimerHandle> {
        // SAFETY: The caller guarantees liveness and membership. Linked
        // neighbors remain live and pinned by the insertion contract. Access
        // is shared and touches only Cell links, with no user callbacks.
        unsafe {
            let links = &node.as_ref().wheel_state;
            let prev = links.prev.get();
            let next = links.next.get();
            if let Some(prev) = prev {
                debug_assert_eq!(prev.as_ref().wheel_state.next.get(), Some(node));
                prev.as_ref().wheel_state.next.set(next);
            } else {
                if self.head != Some(node) {
                    return None;
                }
                self.head = next;
            }
            if let Some(next) = next {
                debug_assert_eq!(next.as_ref().wheel_state.prev.get(), Some(node));
                next.as_ref().wheel_state.prev.set(prev);
            } else {
                if self.tail != Some(node) {
                    return None;
                }
                self.tail = prev;
            }
            links.prev.set(None);
            links.next.set(None);
        }
        Some(TimerHandle { inner: node })
    }
}

// Adapted from the original Monoio/Tokio list tests to concrete timer entries.
// Preserve their FIFO, repeated removal, and neighbor-link checks.
#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use super::*;

    fn entry() -> Pin<Box<TimerShared>> {
        Box::pin(TimerShared::new())
    }

    fn ptr(entry: &Pin<Box<TimerShared>>) -> NonNull<TimerShared> {
        NonNull::from(entry.as_ref().get_ref())
    }

    fn collect_list(list: &mut EntryList) -> Vec<NonNull<TimerShared>> {
        let mut entries = Vec::new();
        while let Some(entry) = list.pop_back() {
            entries.push(entry.inner);
        }
        entries
    }

    // SAFETY contract: tests retain the pinned owners until the list is drained
    // or no longer accessible. All calls below use entries from that scope.
    unsafe fn push_all(list: &mut EntryList, entries: &[Pin<&TimerShared>]) {
        for entry in entries {
            // SAFETY: The caller retains each pinned owner. Tests pass only
            // detached entries and mutate this list without callbacks.
            unsafe { list.push_front(entry.get_ref().handle()) };
        }
    }

    macro_rules! assert_clean {
        ($entry:ident) => {{
            assert!($entry.wheel_state.prev.get().is_none());
            assert!($entry.wheel_state.next.get().is_none());
        }};
    }

    macro_rules! assert_ptr_eq {
        ($entry:ident, $pointer:expr) => {
            assert_eq!(Some(ptr(&$entry)), $pointer)
        };
    }

    #[test]
    fn const_new() {
        const _: EntryList = EntryList::new();
    }

    #[test]
    fn push_and_drain() {
        let a = entry();
        let b = entry();
        let c = entry();
        let mut list = EntryList::new();
        assert!(list.is_empty());
        // SAFETY: Owners remain pinned and live until the list is drained.
        unsafe { push_all(&mut list, &[a.as_ref(), b.as_ref(), c.as_ref()]) };
        assert!(!list.is_empty());
        assert_eq!(collect_list(&mut list), [ptr(&a), ptr(&b), ptr(&c)]);
        assert!(list.is_empty());
        assert_clean!(a);
        assert_clean!(b);
        assert_clean!(c);
    }

    #[test]
    fn push_pop_push_pop() {
        let a = entry();
        let b = entry();
        let mut list = EntryList::new();
        // SAFETY: a is detached and remains pinned until popped below.
        unsafe { list.push_front(a.as_ref().get_ref().handle()) };
        assert_eq!(list.pop_back().unwrap().inner, ptr(&a));
        assert_clean!(a);
        assert!(list.is_empty());
        // SAFETY: b is detached and remains pinned until popped below.
        unsafe { list.push_front(b.as_ref().get_ref().handle()) };
        assert_eq!(list.pop_back().unwrap().inner, ptr(&b));
        assert_clean!(b);
        assert!(list.is_empty());
        assert!(list.pop_back().is_none());
    }

    #[test]
    fn remove_by_address() {
        let a = entry();
        let b = entry();
        let c = entry();

        unsafe {
            // Remove first
            let mut list = EntryList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);
            assert!(list.remove(ptr(&a)).is_some());
            assert_clean!(a);
            // `a` should be no longer there and can't be removed twice
            assert!(list.remove(ptr(&a)).is_none());
            assert!(!list.is_empty());

            assert!(list.remove(ptr(&b)).is_some());
            assert_clean!(b);
            // `b` should be no longer there and can't be removed twice
            assert!(list.remove(ptr(&b)).is_none());
            assert!(!list.is_empty());

            assert!(list.remove(ptr(&c)).is_some());
            assert_clean!(c);
            // `b` should be no longer there and can't be removed twice
            assert!(list.remove(ptr(&c)).is_none());
            assert!(list.is_empty());
        }

        unsafe {
            // Remove middle
            let mut list = EntryList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&a)).is_some());
            assert_clean!(a);

            assert_ptr_eq!(b, list.head);
            assert_ptr_eq!(c, b.wheel_state.next.get());
            assert_ptr_eq!(b, c.wheel_state.prev.get());

            let items = collect_list(&mut list);
            assert_eq!([ptr(&c), ptr(&b)].to_vec(), items);
        }

        unsafe {
            // Remove middle
            let mut list = EntryList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&b)).is_some());
            assert_clean!(b);

            assert_ptr_eq!(c, a.wheel_state.next.get());
            assert_ptr_eq!(a, c.wheel_state.prev.get());

            let items = collect_list(&mut list);
            assert_eq!([ptr(&c), ptr(&a)].to_vec(), items);
        }

        unsafe {
            // Remove last
            // Remove middle
            let mut list = EntryList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&c)).is_some());
            assert_clean!(c);

            assert!(b.wheel_state.next.get().is_none());
            assert_ptr_eq!(b, list.tail);

            let items = collect_list(&mut list);
            assert_eq!([ptr(&b), ptr(&a)].to_vec(), items);
        }

        unsafe {
            // Remove first of two
            let mut list = EntryList::new();

            push_all(&mut list, &[b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&a)).is_some());

            assert_clean!(a);

            // a should be no longer there and can't be removed twice
            assert!(list.remove(ptr(&a)).is_none());

            assert_ptr_eq!(b, list.head);
            assert_ptr_eq!(b, list.tail);

            assert!(b.wheel_state.next.get().is_none());
            assert!(b.wheel_state.prev.get().is_none());

            let items = collect_list(&mut list);
            assert_eq!([ptr(&b)].to_vec(), items);
        }

        unsafe {
            // Remove last of two
            let mut list = EntryList::new();

            push_all(&mut list, &[b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&b)).is_some());

            assert_clean!(b);

            assert_ptr_eq!(a, list.head);
            assert_ptr_eq!(a, list.tail);

            assert!(a.wheel_state.next.get().is_none());
            assert!(a.wheel_state.prev.get().is_none());

            let items = collect_list(&mut list);
            assert_eq!([ptr(&a)].to_vec(), items);
        }

        unsafe {
            // Remove last item
            let mut list = EntryList::new();

            push_all(&mut list, &[a.as_ref()]);

            assert!(list.remove(ptr(&a)).is_some());
            assert_clean!(a);

            assert!(list.head.is_none());
            assert!(list.tail.is_none());
            let items = collect_list(&mut list);
            assert!(items.is_empty());
        }

        unsafe {
            // Remove missing
            let mut list = EntryList::new();

            list.push_front(b.as_ref().get_ref().handle());
            list.push_front(a.as_ref().get_ref().handle());

            assert!(list.remove(ptr(&c)).is_none());
            assert_eq!(collect_list(&mut list), [ptr(&b), ptr(&a)]);
        }
    }
}
