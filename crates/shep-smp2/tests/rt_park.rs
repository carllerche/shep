#![forbid(unsafe_code)]
use shep_smp2::{Parking, io::WaitOutcome};
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn wake_between_final_check_and_kernel_wait_is_retained() {
    let mut reactor = Parking::new(1).unwrap().0;
    let wake = reactor.wake_handle();
    for _ in 0..100 {
        reactor.poll().unwrap();
        let park = reactor.prepare_park().unwrap();
        // Force publication after prepare/final-check but before entering wait.
        thread::scope(|s| s.spawn(|| assert!(wake.unpark().unwrap())).join().unwrap());
        assert_eq!(
            park.wait(Some(Instant::now() + Duration::from_secs(1)))
                .unwrap(),
            WaitOutcome::Ready
        );
        // PollAdd must have completed; a following expired wait never blocks.
        reactor.poll().unwrap();
    }
}

#[test]
fn prepare_then_recheck_observes_a_wake_drained_before_final_check() {
    let mut reactor = Parking::new(1).unwrap().0;
    let wake = reactor.wake_handle();
    let ready = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| {
        let ready = ready.clone();
        scope
            .spawn(move || {
                ready.store(true, Ordering::Release);
                wake.unpark().unwrap();
            })
            .join()
            .unwrap();
    });
    let park = reactor.prepare_park().unwrap();
    assert!(ready.load(Ordering::Acquire));
    drop(park); // Runtime skips waiting when its final inbox check finds work.
}

#[test]
fn remote_wake_interrupts_idle_wait_and_watch_rearms() {
    let mut reactor = Parking::new(1).unwrap().0;
    let wake = reactor.wake_handle();
    for _ in 0..20 {
        reactor.poll().unwrap();
        let park = reactor.prepare_park().unwrap();
        let gate = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            let gate_remote = gate.clone();
            let wake = wake.clone();
            scope.spawn(move || {
                gate_remote.wait();
                wake.unpark().unwrap();
            });
            gate.wait();
            assert_eq!(
                park.wait(Some(Instant::now() + Duration::from_secs(1)))
                    .unwrap(),
                WaitOutcome::Ready
            );
        });
    }
}

#[test]
fn empty_reactor_deadline_expires_and_late_wakes_are_harmless() {
    let mut reactor = Parking::new(1).unwrap().0;
    let wake = reactor.wake_handle();
    let deadline = Instant::now() + Duration::from_millis(5);
    reactor
        .prepare_park()
        .unwrap()
        .wait(Some(deadline))
        .unwrap();
    assert!(Instant::now() >= deadline);
    reactor.shutdown().unwrap();
    assert!(!wake.unpark().unwrap());
    drop(reactor);
    thread::spawn(move || {
        wake.waker().wake();
        assert!(!wake.unpark().unwrap());
    })
    .join()
    .unwrap();
}

#[test]
fn wake_storm_can_race_with_reactor_destruction() {
    let mut reactor = Parking::new(1).unwrap().0;
    drop(reactor.prepare_park().unwrap());
    let wake = reactor.wake_handle();
    thread::scope(|scope| {
        for _ in 0..4 {
            let wake = wake.clone();
            scope.spawn(move || {
                for _ in 0..1000 {
                    wake.unpark().unwrap();
                }
            });
        }
        drop(reactor);
    });
    assert!(!wake.unpark().unwrap());
}

#[test]
fn shutdown_retires_an_existing_cancel_before_canceling_its_watch() {
    use shep_smp2::io::{Buffer, Operation};
    use std::{fs::File, os::fd::OwnedFd};
    let (mut parking, io) = Parking::new(1).unwrap();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut receive = io
        .push(Operation::Recv {
            socket: std::rc::Rc::new(
                shep_smp2::io::Fd::new(File::from(OwnedFd::from(socket))).unwrap(),
            ),
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    let mut cancellation = io
        .push(Operation::Cancel(receive.cancel_target(&io)))
        .unwrap();
    parking.shutdown().unwrap();
    parking.shutdown().unwrap();
    assert!(receive.try_complete(&io).unwrap().result.is_err());
    assert!(cancellation.try_complete(&io).unwrap().result.is_ok());
    assert_eq!(io.in_flight(), 0);
}
