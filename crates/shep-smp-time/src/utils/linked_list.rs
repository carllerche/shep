// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Linked list impl.
//!
//! An intrusive double linked list of data
//!
//! The data structure supports tracking pinned nodes. Most of the data
//! structure's APIs are `unsafe` as they require the caller to ensure the
//! specified node is actually contained by the list.
// Heavily borrowed from tokio.
// Copyright (c) 2021 Tokio Contributors, licensed under the MIT license.
use core::{
    cell::Cell,
    fmt,
    marker::{PhantomData, PhantomPinned},
    mem::ManuallyDrop,
    ptr::NonNull,
};

/// An intrusive linked list.
///
/// Currently, the list is not emptied on drop. It is the caller's
/// responsibility to ensure the list is empty before dropping it.
pub(crate) struct LinkedList<L, T> {
    /// Linked list head
    head: Option<NonNull<T>>,

    /// Linked list tail
    tail: Option<NonNull<T>>,

    /// Node type marker.
    _marker: PhantomData<*const L>,
}

/// Defines how a type is tracked within a linked list.
///
/// In order to support storing a single type within multiple lists, accessing
/// the list pointers is decoupled from the entry type.
///
/// # Safety
///
/// Implementations must guarantee that `Target` types are pinned in memory. In
/// other words, when a node is inserted, the value will not be moved as long as
/// it is stored in the list.
pub(crate) unsafe trait Link {
    /// Handle to the list entry.
    ///
    /// This is usually a pointer-ish type.
    type Handle;

    /// Node type
    type Target;

    /// Convert the handle to a raw pointer without consuming the handle
    #[allow(clippy::wrong_self_convention)]
    fn as_raw(handle: &Self::Handle) -> NonNull<Self::Target>;

    /// Convert the raw pointer to a handle
    unsafe fn from_raw(ptr: NonNull<Self::Target>) -> Self::Handle;

    /// Return the pointers for a node
    unsafe fn pointers(target: NonNull<Self::Target>) -> NonNull<Pointers<Self::Target>>;
}

/// Local, interior-mutable links in a pinned node.
pub(crate) struct Pointers<T> {
    prev: Cell<Option<NonNull<T>>>,
    next: Cell<Option<NonNull<T>>>,
    _pin: PhantomPinned,
}

// ===== impl LinkedList =====

impl<L, T> LinkedList<L, T> {
    /// Creates an empty linked list.
    pub(crate) const fn new() -> LinkedList<L, T> {
        LinkedList {
            head: None,
            tail: None,
            _marker: PhantomData,
        }
    }
}

impl<L: Link> LinkedList<L, L::Target> {
    /// Adds an element first in the list.
    pub(crate) fn push_front(&mut self, val: L::Handle) {
        // The value should not be dropped, it is being inserted into the list
        let val = ManuallyDrop::new(val);
        let ptr = L::as_raw(&val);
        assert_ne!(self.head, Some(ptr));
        // SAFETY: Link's handle contract keeps this node alive and pinned.
        // The caller transfers detached membership; existing neighbors remain
        // live until removed. Mutations touch only shared Cell link fields.
        unsafe {
            L::pointers(ptr).as_ref().set_next(self.head);
            L::pointers(ptr).as_ref().set_prev(None);

            if let Some(head) = self.head {
                L::pointers(head).as_ref().set_prev(Some(ptr));
            }

            self.head = Some(ptr);

            if self.tail.is_none() {
                self.tail = Some(ptr);
            }
        }
    }

    /// Removes the last element from a list and returns it, or None if it is
    /// empty.
    pub(crate) fn pop_back(&mut self) -> Option<L::Handle> {
        // SAFETY: Every linked node remains live and pinned by Link's contract.
        // Detach the tail before restoring its handle; no callback runs here.
        unsafe {
            let last = self.tail?;
            self.tail = L::pointers(last).as_ref().get_prev();

            if let Some(prev) = L::pointers(last).as_ref().get_prev() {
                L::pointers(prev).as_ref().set_next(None);
            } else {
                self.head = None
            }

            L::pointers(last).as_ref().set_prev(None);
            L::pointers(last).as_ref().set_next(None);

            Some(L::from_raw(last))
        }
    }

    /// Returns whether the linked list does not contain any node
    pub(crate) fn is_empty(&self) -> bool {
        if self.head.is_some() {
            return false;
        }

        assert!(self.tail.is_none());
        true
    }

    /// Removes the specified node from the list
    ///
    /// # Safety
    ///
    /// The caller **must** ensure that `node` is currently contained by
    /// `self` or not contained by any other list.
    pub(crate) unsafe fn remove(&mut self, node: NonNull<L::Target>) -> Option<L::Handle> {
        // SAFETY: The caller guarantees a live pinned node in this list or
        // detached from all lists. Link's implementation supplies its shared
        // Cell links. Neighbor pointers belong to the same live list.
        unsafe {
            if let Some(prev) = L::pointers(node).as_ref().get_prev() {
                debug_assert_eq!(L::pointers(prev).as_ref().get_next(), Some(node));
                L::pointers(prev)
                    .as_ref()
                    .set_next(L::pointers(node).as_ref().get_next());
            } else {
                if self.head != Some(node) {
                    return None;
                }

                self.head = L::pointers(node).as_ref().get_next();
            }

            if let Some(next) = L::pointers(node).as_ref().get_next() {
                debug_assert_eq!(L::pointers(next).as_ref().get_prev(), Some(node));
                L::pointers(next)
                    .as_ref()
                    .set_prev(L::pointers(node).as_ref().get_prev());
            } else {
                // This might be the last item in the list
                if self.tail != Some(node) {
                    return None;
                }

                self.tail = L::pointers(node).as_ref().get_prev();
            }

            L::pointers(node).as_ref().set_next(None);
            L::pointers(node).as_ref().set_prev(None);

            Some(L::from_raw(node))
        }
    }
}

impl<L: Link> fmt::Debug for LinkedList<L, L::Target> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkedList")
            .field("head", &self.head)
            .field("tail", &self.tail)
            .finish()
    }
}

impl<L: Link> Default for LinkedList<L, L::Target> {
    fn default() -> Self {
        Self::new()
    }
}

// ===== impl Pointers =====

impl<T> Pointers<T> {
    pub(crate) fn new() -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            _pin: PhantomPinned,
        }
    }
    fn get_prev(&self) -> Option<NonNull<T>> {
        self.prev.get()
    }
    fn get_next(&self) -> Option<NonNull<T>> {
        self.next.get()
    }
    fn set_prev(&self, value: Option<NonNull<T>>) {
        self.prev.set(value);
    }
    fn set_next(&self, value: Option<NonNull<T>>) {
        self.next.set(value);
    }
}

impl<T> fmt::Debug for Pointers<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prev = self.get_prev();
        let next = self.get_next();
        f.debug_struct("Pointers")
            .field("prev", &prev)
            .field("next", &next)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use super::*;

    #[derive(Debug)]
    struct Entry {
        pointers: Pointers<Entry>,
        val: i32,
    }

    unsafe impl<'a> Link for &'a Entry {
        type Handle = Pin<&'a Entry>;
        type Target = Entry;

        fn as_raw(handle: &Pin<&'_ Entry>) -> NonNull<Entry> {
            NonNull::from(handle.get_ref())
        }

        unsafe fn from_raw(ptr: NonNull<Entry>) -> Pin<&'a Entry> {
            // SAFETY: The test list retains the original shared pinned entry.
            unsafe { Pin::new_unchecked(&*ptr.as_ptr()) }
        }

        unsafe fn pointers(target: NonNull<Entry>) -> NonNull<Pointers<Entry>> {
            // Test entries are held through shared pinned references. Access
            // the interior-mutable links without creating &mut to all of Entry.
            // SAFETY: The list guarantees a live test entry.
            NonNull::from(&unsafe { target.as_ref() }.pointers)
        }
    }

    fn entry(val: i32) -> Pin<Box<Entry>> {
        Box::pin(Entry {
            pointers: Pointers::new(),
            val,
        })
    }

    fn ptr(r: &Pin<Box<Entry>>) -> NonNull<Entry> {
        r.as_ref().get_ref().into()
    }

    fn collect_list(list: &mut LinkedList<&'_ Entry, <&'_ Entry as Link>::Target>) -> Vec<i32> {
        let mut ret = vec![];

        while let Some(entry) = list.pop_back() {
            ret.push(entry.val);
        }

        ret
    }

    fn push_all<'a>(
        list: &mut LinkedList<&'a Entry, <&'_ Entry as Link>::Target>,
        entries: &[Pin<&'a Entry>],
    ) {
        for entry in entries.iter() {
            list.push_front(*entry);
        }
    }

    macro_rules! assert_clean {
        ($e:ident) => {{
            assert!($e.pointers.get_next().is_none());
            assert!($e.pointers.get_prev().is_none());
        }};
    }

    macro_rules! assert_ptr_eq {
        ($a:expr, $b:expr) => {{
            // Deal with mapping a Pin<&mut T> -> Option<NonNull<T>>
            assert_eq!(Some($a.as_ref().get_ref().into()), $b)
        }};
    }

    #[test]
    fn const_new() {
        const _: LinkedList<&Entry, <&Entry as Link>::Target> = LinkedList::new();
    }

    #[test]
    fn push_and_drain() {
        let a = entry(5);
        let b = entry(7);
        let c = entry(31);

        let mut list = LinkedList::new();
        assert!(list.is_empty());

        list.push_front(a.as_ref());
        assert!(!list.is_empty());
        list.push_front(b.as_ref());
        list.push_front(c.as_ref());

        let items: Vec<i32> = collect_list(&mut list);
        assert_eq!([5, 7, 31].to_vec(), items);

        assert!(list.is_empty());
    }

    #[test]
    fn push_pop_push_pop() {
        let a = entry(5);
        let b = entry(7);

        let mut list = LinkedList::<&Entry, <&Entry as Link>::Target>::new();

        list.push_front(a.as_ref());

        let entry = list.pop_back().unwrap();
        assert_eq!(5, entry.val);
        assert!(list.is_empty());

        list.push_front(b.as_ref());

        let entry = list.pop_back().unwrap();
        assert_eq!(7, entry.val);

        assert!(list.is_empty());
        assert!(list.pop_back().is_none());
    }

    #[test]
    fn remove_by_address() {
        let a = entry(5);
        let b = entry(7);
        let c = entry(31);

        unsafe {
            // Remove first
            let mut list = LinkedList::new();

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
            let mut list = LinkedList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&a)).is_some());
            assert_clean!(a);

            assert_ptr_eq!(b, list.head);
            assert_ptr_eq!(c, b.pointers.get_next());
            assert_ptr_eq!(b, c.pointers.get_prev());

            let items = collect_list(&mut list);
            assert_eq!([31, 7].to_vec(), items);
        }

        unsafe {
            // Remove middle
            let mut list = LinkedList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&b)).is_some());
            assert_clean!(b);

            assert_ptr_eq!(c, a.pointers.get_next());
            assert_ptr_eq!(a, c.pointers.get_prev());

            let items = collect_list(&mut list);
            assert_eq!([31, 5].to_vec(), items);
        }

        unsafe {
            // Remove last
            // Remove middle
            let mut list = LinkedList::new();

            push_all(&mut list, &[c.as_ref(), b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&c)).is_some());
            assert_clean!(c);

            assert!(b.pointers.get_next().is_none());
            assert_ptr_eq!(b, list.tail);

            let items = collect_list(&mut list);
            assert_eq!([7, 5].to_vec(), items);
        }

        unsafe {
            // Remove first of two
            let mut list = LinkedList::new();

            push_all(&mut list, &[b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&a)).is_some());

            assert_clean!(a);

            // a should be no longer there and can't be removed twice
            assert!(list.remove(ptr(&a)).is_none());

            assert_ptr_eq!(b, list.head);
            assert_ptr_eq!(b, list.tail);

            assert!(b.pointers.get_next().is_none());
            assert!(b.pointers.get_prev().is_none());

            let items = collect_list(&mut list);
            assert_eq!([7].to_vec(), items);
        }

        unsafe {
            // Remove last of two
            let mut list = LinkedList::new();

            push_all(&mut list, &[b.as_ref(), a.as_ref()]);

            assert!(list.remove(ptr(&b)).is_some());

            assert_clean!(b);

            assert_ptr_eq!(a, list.head);
            assert_ptr_eq!(a, list.tail);

            assert!(a.pointers.get_next().is_none());
            assert!(a.pointers.get_prev().is_none());

            let items = collect_list(&mut list);
            assert_eq!([5].to_vec(), items);
        }

        unsafe {
            // Remove last item
            let mut list = LinkedList::new();

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
            let mut list = LinkedList::<&Entry, <&Entry as Link>::Target>::new();

            list.push_front(b.as_ref());
            list.push_front(a.as_ref());

            assert!(list.remove(ptr(&c)).is_none());
        }
    }
}
