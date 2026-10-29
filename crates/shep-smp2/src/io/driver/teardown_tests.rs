#![forbid(unsafe_code)]
use super::*;
use crate::io::Buffer;

fn pending_receive() -> (Driver, Op, std::os::unix::net::UnixStream) {
    let driver = Driver::new(2).unwrap();
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let operation = driver
        .push(Operation::Recv {
            socket: Rc::new(Fd::new(socket).unwrap()),
            buffer: Buffer::new(8, 1).unwrap(),
        })
        .unwrap();
    (driver, operation, peer)
}

#[test]
fn teardown_retries_transient_submission_errors_before_retiring_inputs() {
    let (driver, mut receive, _peer) = pending_receive();
    let io = driver.io().clone();
    io.shared
        .submit_faults
        .borrow_mut()
        .extend(
            [libc::EINTR, libc::EAGAIN, libc::EBUSY].map(|error| SubmitFault {
                count: 0,
                error: Some(error),
            }),
        );
    drop(driver);
    assert!(io.shared.submit_faults.borrow().is_empty());
    assert_eq!(io.in_flight(), 0);
    assert!(io.shared.state.borrow().ring.is_none());
    assert_eq!(
        receive
            .try_complete(&io)
            .unwrap()
            .result
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ECANCELED)
    );
}

// A subprocess is necessary: abort must terminate, not unwind into the test.
#[test]
#[ignore = "requires native subprocesses; run with cargo nextest run --run-ignored all"]
fn teardown_aborts_on_fatal_error_or_panic_before_retirement() {
    const CHILD: &str = "SHEP_SMP2_TEARDOWN_ABORT_CASE";
    if let Some(case) = std::env::var_os(CHILD) {
        let (driver, _receive, _peer) = pending_receive();
        let io = driver.io().clone();
        match case.to_str().unwrap() {
            "error" => {
                io.shared.submit_faults.borrow_mut().push_back(SubmitFault {
                    count: 0,
                    error: Some(libc::EIO),
                });
                eprintln!("teardown abort case armed: error");
                drop(driver);
            }
            "panic" => {
                // Force a teardown invariant panic while the request is still live.
                let _borrow = io.shared.state.borrow();
                eprintln!("teardown abort case armed: panic");
                drop(driver);
            }
            _ => panic!("unknown teardown abort case"),
        }
        panic!("teardown returned without aborting");
    }

    use std::os::unix::process::ExitStatusExt;
    for case in ["error", "panic"] {
        // Keep any core dump out of the source tree and remove it with the fixture.
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "io::driver::teardown_tests::teardown_aborts_on_fatal_error_or_panic_before_retirement",
                "--nocapture",
                "--ignored",
            ])
            .env(CHILD, case)
            .current_dir(directory.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("teardown abort case armed: {case}")),
            "{stderr}"
        );
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{stderr}");
        let expected = match case {
            "error" => "io_uring teardown failed before request retirement",
            "panic" => "already borrowed",
            _ => unreachable!(),
        };
        assert!(stderr.contains(expected), "{stderr}");
    }
}
