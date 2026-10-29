//! Allocation instrumentation only; runtime ownership code remains safe Rust.
#![cfg(all(not(miri), feature = "allocation-tests"))]
use shep_smp2::io::{Buffer, Driver, Fd, Operation, ReadAt, WriteAt};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    rc::Rc,
};

struct CountAllocator;
thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}
fn record() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNT.try_with(|n| n.set(n.get() + 1));
    }
}
// SAFETY: All allocation operations delegate unchanged to System. Tracking uses
// initialized, nonallocating TLS cells and never touches the allocated memory.
unsafe impl GlobalAlloc for CountAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountAllocator = CountAllocator;

fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            TRACK.set(false);
        }
    }
    assert!(!TRACK.get());
    COUNT.set(0);
    TRACK.set(true);
    let stop = Stop;
    let result = f();
    drop(stop);
    (result, COUNT.get())
}
fn retire(driver: &mut Driver) {
    while driver.in_flight() != 0 {
        driver.submit_and_wait(None).unwrap();
        driver.tick();
    }
}

#[test]
fn normal_file_operations_allocate_nothing_and_only_unfinished_abandonment_boxes() {
    let mut driver = Driver::new(8).unwrap();
    let io = driver.io().clone();
    let file = Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
    let buffer = Buffer::from_slice(b"allocation test", 1).unwrap();
    let pointer = buffer.as_ptr();
    let (op, allocations) = measure(|| {
        io.submit(WriteAt {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap()
    });
    assert_eq!(allocations, 0);
    let mut op = op;
    let (_, allocations) = measure(|| retire(&mut driver));
    assert_eq!(allocations, 0);
    let (completion, allocations) = measure(|| op.try_complete().unwrap());
    assert_eq!(allocations, 0);
    assert_eq!(completion.operation.buffer.as_ptr(), pointer);
    assert_eq!(completion.result.unwrap().into_count(), 15);
    let buffer = completion.operation.buffer;
    let (mut op, allocations) = measure(|| {
        io.submit(ReadAt {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap()
    });
    assert_eq!(allocations, 0);
    retire(&mut driver);
    let pin = op.cancel_target();
    let (completion, allocations) = measure(|| op.try_complete().unwrap());
    assert_eq!(allocations, 0);
    drop(pin);
    let buffer = completion.operation.buffer;
    let op = io
        .submit(ReadAt {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap();
    let (_, allocations) = measure(|| drop(op));
    assert_eq!(allocations, 1, "only unfinished concrete data is boxed");
    retire(&mut driver);

    let buffer = Buffer::new(1, 1).unwrap();
    let op = io
        .submit(ReadAt {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap();
    let pin = op.cancel_target();
    retire(&mut driver);
    let (_, allocations) = measure(|| drop(op));
    assert_eq!(
        allocations, 0,
        "completed but pinned abandonment needs no box"
    );
    drop(pin);

    io.close();
    let buffer = Buffer::new(1, 1).unwrap();
    let pointer = buffer.as_ptr();
    let (rejected, allocations) = measure(|| {
        io.submit(ReadAt {
            file,
            buffer,
            offset: 0,
        })
        .unwrap_err()
    });
    assert_eq!(allocations, 0);
    assert_eq!(rejected.operation.buffer.as_ptr(), pointer);
}

#[test]
fn explicit_cancellation_and_abandoning_its_owner_do_not_allocate() {
    let mut driver = Driver::new(8).unwrap();
    let io = driver.io().clone();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let socket = Rc::new(Fd::new(socket).unwrap());
    let mut target = io
        .push(Operation::PollAdd {
            file: socket,
            events: libc::POLLIN as u32,
        })
        .unwrap();
    let (pin, allocations) = measure(|| target.cancel_target());
    assert_eq!(allocations, 0);
    let (cancel, allocations) = measure(|| io.push(Operation::Cancel(pin)).unwrap());
    assert_eq!(allocations, 0);
    let (_, allocations) = measure(|| drop(cancel));
    assert_eq!(
        allocations, 0,
        "the cancellation record owns its target pin"
    );
    retire(&mut driver);
    assert_eq!(
        target
            .try_complete()
            .unwrap()
            .result
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ECANCELED)
    );
}

#[test]
fn inline_intervals_construct_register_and_move_without_allocating() {
    use shep_smp2::{
        Runtime,
        time::{Duration, Instant, interval_at},
    };
    use std::task::{Context, Waker};
    let runtime = Runtime::new(Default::default()).unwrap();
    let _enter = runtime.enter();
    let first = Instant::now() + Duration::from_secs(86400);
    let (mut interval, allocations) = measure(|| interval_at(first, Duration::from_secs(1)));
    assert_eq!(allocations, 0);
    let (_, allocations) = measure(|| {
        assert!(
            interval
                .poll_tick(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    });
    assert_eq!(allocations, 0, "registration reuses reserved timer storage");
    let (mut moved, allocations) = measure(|| std::hint::black_box(interval));
    assert_eq!(allocations, 0);
    runtime.timer().advance(first + Duration::from_millis(1));
    let (tick, allocations) = measure(|| moved.poll_tick(&mut Context::from_waker(Waker::noop())));
    assert_eq!(tick, std::task::Poll::Ready(first));
    assert_eq!(allocations, 0);
}
