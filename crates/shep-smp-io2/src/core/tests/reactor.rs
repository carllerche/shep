#![forbid(unsafe_code)]
use crate::core::{Buffer, Completion, Operation, Reactor};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
    time::{Duration, Instant},
};

fn finish(reactor: &mut Reactor, count: usize) -> Vec<Completion> {
    let end = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    while output.len() != count {
        output.extend(reactor.poll().unwrap());
        if output.len() != count {
            assert!(Instant::now() < end, "I/O failed to complete");
            reactor.prepare_park().unwrap().wait(Some(end)).unwrap();
        }
    }
    output
}

#[test]
fn owned_write_read_sync_and_short_eof() {
    let mut original = tempfile::tempfile().unwrap();
    let file = Arc::new(original.try_clone().unwrap());
    let mut reactor = Reactor::new(4).unwrap();
    let bytes = Buffer::from_slice(b"native reactor", 4096).unwrap();
    let ptr = bytes.as_ptr();
    let id = reactor
        .push(Operation::Write {
            file: file.clone(),
            buffer: bytes,
            offset: 7,
        })
        .unwrap();
    let completion = finish(&mut reactor, 1).pop().unwrap();
    assert_eq!(completion.id, id);
    assert_eq!(completion.result.unwrap(), 14);
    assert_eq!(completion.operation.buffer().unwrap().as_ptr(), ptr);
    reactor
        .push(Operation::Sync {
            file: file.clone(),
            data_only: true,
        })
        .unwrap();
    assert!(finish(&mut reactor, 1).pop().unwrap().result.is_ok());
    reactor
        .push(Operation::Read {
            file,
            buffer: Buffer::new(32, 64).unwrap(),
            offset: 7,
        })
        .unwrap();
    let completion = finish(&mut reactor, 1).pop().unwrap();
    assert_eq!(completion.result.unwrap(), 14);
    assert_eq!(
        &completion.operation.buffer().unwrap()[..14],
        b"native reactor"
    );
    reactor.shutdown().unwrap();
    original.seek(SeekFrom::Start(7)).unwrap();
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"native reactor");
}

#[test]
fn capacity_and_offset_rejections_return_ownership() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let mut reactor = Reactor::new(1).unwrap();
    reactor
        .push(Operation::Sync {
            file: file.clone(),
            data_only: false,
        })
        .unwrap();
    let buffer = Buffer::from_slice(b"hello", 8).unwrap();
    let ptr = buffer.as_ptr();
    let rejected = reactor
        .push(Operation::Write {
            file: file.clone(),
            buffer,
            offset: 0,
        })
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
    finish(&mut reactor, 1);
    let rejected = reactor
        .push(Operation::Read {
            file,
            buffer: rejected.operation.into_buffer().unwrap(),
            offset: u64::MAX,
        })
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(rejected.operation.buffer().unwrap().as_ptr(), ptr);
}

#[test]
fn descriptor_is_retained_until_completion_and_drop_drains_work() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let weak = Arc::downgrade(&file);
    let mut reactor = Reactor::new(2).unwrap();
    reactor
        .push(Operation::Write {
            file,
            buffer: Buffer::from_slice(b"retained", 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    assert!(weak.upgrade().is_some());
    drop(reactor);
    assert!(weak.upgrade().is_none());
}

#[test]
fn kernel_errors_return_the_original_buffer() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let file = Arc::new(std::fs::File::open(temp.path()).unwrap());
    let mut reactor = Reactor::new(2).unwrap();
    let buffer = Buffer::from_slice(b"readonly", 1).unwrap();
    let ptr = buffer.as_ptr();
    reactor
        .push(Operation::Write {
            file,
            buffer,
            offset: 0,
        })
        .unwrap();
    let c = finish(&mut reactor, 1).pop().unwrap();
    assert!(c.result.is_err());
    assert_eq!(c.operation.buffer().unwrap().as_ptr(), ptr);
}

#[test]
fn file_position_is_unchanged_by_positional_io() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"abcdef").unwrap();
    file.seek(SeekFrom::Start(3)).unwrap();
    let shared = Arc::new(file.try_clone().unwrap());
    let mut reactor = Reactor::new(2).unwrap();
    reactor
        .push(Operation::Read {
            file: shared,
            buffer: Buffer::new(2, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    finish(&mut reactor, 1);
    assert_eq!(file.stream_position().unwrap(), 3);
}

#[test]
fn reserved_wake_controls_do_not_reduce_data_capacity_and_shutdown_is_idempotent() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let mut reactor = Reactor::new(1).unwrap();
    drop(reactor.prepare_park().unwrap());
    reactor
        .push(Operation::Write {
            file: file.clone(),
            buffer: Buffer::new(1, 1).unwrap(),
            offset: 0,
        })
        .unwrap();
    assert_eq!(reactor.in_flight(), 1);
    let completed = reactor.shutdown().unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result.as_ref().unwrap(), &1);
    assert_eq!(reactor.in_flight(), 0);
    assert!(reactor.shutdown().unwrap().is_empty());
    let rejected = reactor
        .push(Operation::Sync {
            file,
            data_only: false,
        })
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::BrokenPipe);
}

#[test]
fn zero_length_io_and_eof_return_initialized_owned_buffers() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let mut reactor = Reactor::new(2).unwrap();
    reactor
        .push(Operation::Write {
            file: file.clone(),
            buffer: Buffer::new(0, 4096).unwrap(),
            offset: 0,
        })
        .unwrap();
    let done = finish(&mut reactor, 1).pop().unwrap();
    assert_eq!(done.result.unwrap(), 0);
    assert!(done.operation.buffer().unwrap().is_empty());
    reactor
        .push(Operation::Read {
            file,
            buffer: Buffer::new(8, 4096).unwrap(),
            offset: 0,
        })
        .unwrap();
    let done = finish(&mut reactor, 1).pop().unwrap();
    assert_eq!(done.result.unwrap(), 0);
    assert_eq!(&**done.operation.buffer().unwrap(), &[0; 8]);
}

#[test]
fn invalid_capacity_is_rejected_before_creating_a_ring() {
    assert!(Reactor::new(0).is_err());
    assert!(Reactor::new(32767).is_err());
}

#[test]
fn slab_growth_preserves_buffers_while_results_and_submitted_io_coexist() {
    let mut original = tempfile::tempfile().unwrap();
    original.write_all(&[7; 12_288]).unwrap();
    let file = Arc::new(original);
    let mut reactor = Reactor::new(2).unwrap();
    let operations = reactor.operations();
    let read = |offset| Operation::Read {
        file: file.clone(),
        buffer: Buffer::new(4096, 4096).unwrap(),
        offset,
    };
    let first = operations.queue(read(0)).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    while operations.in_flight() != 0 {
        reactor.tick().unwrap();
        assert!(Instant::now() < end);
    }
    // Keep the first completed result in its slot. Publish a second read, then
    // queue a third: growing the arena must not move the submitted read's bytes.
    let second = operations.queue(read(4096)).unwrap();
    reactor.flush().unwrap();
    let third = operations.queue(read(8192)).unwrap();
    assert_eq!(operations.live_entries(), 3);
    while operations.in_flight() != 0 {
        reactor.tick().unwrap();
        assert!(Instant::now() < end);
    }
    for key in [first, second, third] {
        let completion = operations.take(key).unwrap();
        assert_eq!(completion.result.unwrap(), 4096);
        assert!(
            completion
                .operation
                .buffer()
                .unwrap()
                .iter()
                .all(|b| *b == 7)
        );
    }
    assert_eq!(operations.live_entries(), 0);
}
