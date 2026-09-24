//! Child command for the integration tests.
//!
//! Each argument is one step, run in order:
//!
//! - `count`: append `x` to `./count`, so tests can count executions.
//! - `event:LABEL`: write `LABEL PID PGID` to the FIFO named by
//!   `CACHEEXEC_TEST_EVENTS`.
//! - `wait:PATH`: read one byte from `PATH`, or its end. A FIFO blocks until the
//!   test releases it by writing to it; a regular file passes at once. A trapped
//!   signal ends the wait. A gate that is never released ends the process with
//!   exit code 124 after two minutes, so a test that dies cannot leave it behind.
//! - `wait-stdin`: read stdin until end of file.
//! - `trap:SIG,...`: record these signals instead of dying; see `wait:`.
//! - `traps:PATH`: write the number of recorded signals to PATH.
//! - `out:TEXT`, `err:TEXT`: write TEXT to stdout or stderr.
//! - `outx:HEX`, `errx:HEX`: write hex-encoded bytes to stdout or stderr.
//! - `zeros:OUT:ERR`: write OUT zero bytes to stdout and ERR to stderr at once.
//! - `stdin-empty:CODE`: exit with CODE unless stdin is empty.
//! - `env:NAME`: write the value of the environment variable NAME to stdout.
//! - `args`: write each remaining argument followed by `|`, then stop.
//! - `detach:STEP,...`: start a copy of this program running STEPs, with stdout
//!   and stderr inherited, and a pipe as stdin that closes when this process
//!   exits, then continue without waiting for it.
//! - `raise:SIG`: die from SIG with its default action.
//! - `exit:CODE`: exit with CODE.
//! - `exitenv:NAME`: exit with the code in the environment variable NAME, or 0.
//!
//! Every process also reports a `spawned` event when it starts.
//!
//! Signals are named without the `SIG` prefix, e.g. `trap:INT,TERM`.

use std::{
    ffi::CString,
    io::{Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    process::{Command, Stdio},
    sync::atomic::{AtomicI32, AtomicUsize, Ordering},
};

static TRAPPED: AtomicUsize = AtomicUsize::new(0);
/// Ends of a pipe the signal handler writes to, so a wait also wakes for a
/// signal that arrived before it started (-1 until the first `trap:`).
static SIGNALED_READ: AtomicI32 = AtomicI32::new(-1);
static SIGNALED_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record(_: libc::c_int) {
    TRAPPED.fetch_add(1, Ordering::SeqCst);
    let fd = SIGNALED_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        // Async-signal-safe; a full pipe already wakes the wait.
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

fn signal(name: &str) -> libc::c_int {
    match name {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "TERM" => libc::SIGTERM,
        "PIPE" => libc::SIGPIPE,
        _ => panic!("unknown signal {name}"),
    }
}

fn trap(names: &str) {
    if SIGNALED_WRITE.load(Ordering::SeqCst) < 0 {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
            }
        }
        SIGNALED_READ.store(fds[0], Ordering::SeqCst);
        SIGNALED_WRITE.store(fds[1], Ordering::SeqCst);
    }
    for name in names.split(',') {
        // No SA_RESTART, so a blocked read returns EINTR and the wait ends.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(signal(name), &action, std::ptr::null_mut()),
                0
            );
        }
    }
}

/// How long a gate may stay closed before the child gives up.
const GATE_LIMIT_MS: libc::c_int = 120_000;

/// Reads `fd` until end of file, or until a trapped signal interrupts it.
fn drain(fd: libc::c_int) {
    let mut buffer = [0u8; 4096];
    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read == 0 {
            return;
        }
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                if TRAPPED.load(Ordering::SeqCst) > 0 {
                    return;
                }
                continue;
            }
            panic!("wait failed: {error}");
        }
    }
}

/// Returns once `fd` has a byte or its end to read, or once a trapped signal
/// arrived, including before this call. End of file is not enough on its own:
/// macOS does not always report it to every reader of a FIFO, so gates are
/// released with data.
fn wait_readable(fd: libc::c_int) {
    let signaled = SIGNALED_READ.load(Ordering::SeqCst);
    let mut polls = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: signaled,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let count = if signaled >= 0 { 2 } else { 1 };
    loop {
        match unsafe { libc::poll(polls.as_mut_ptr(), count, GATE_LIMIT_MS) } {
            0 => {
                eprintln!("cacheexec-test-child: gate was not released; giving up");
                std::process::exit(124);
            }
            ready if ready > 0 => {
                let mut byte = 0u8;
                if polls[1].revents != 0 {
                    // Consume the wake-ups, so a later wait blocks again.
                    while unsafe { libc::read(signaled, (&mut byte as *mut u8).cast(), 1) } > 0 {}
                } else {
                    unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
                }
                return;
            }
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    panic!("wait failed: {error}");
                }
            }
        }
    }
}

fn wait(path: &str) {
    let path = CString::new(path).unwrap();
    // Non-blocking, so only `wait_readable` waits and its limit always applies,
    // even for a FIFO whose writer is already gone.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    assert!(
        fd >= 0,
        "open {path:?} failed: {}",
        std::io::Error::last_os_error()
    );
    wait_readable(fd);
    unsafe { libc::close(fd) };
}

fn event(label: &str) {
    // Tests that run cacheexec outside a sandbox's environment get no events.
    let Some(path) = std::env::var_os("CACHEEXEC_TEST_EVENTS") else {
        return;
    };
    let path = CString::new(path.as_bytes()).unwrap();
    // Never blocks: without a reader the open fails and the event is dropped.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return;
    }
    let line = format!("{label} {} {}\n", std::process::id(), unsafe {
        libc::getpgrp()
    });
    unsafe {
        libc::write(fd, line.as_ptr().cast(), line.len());
        libc::close(fd);
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn write_out(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn write_err(bytes: &[u8]) {
    let _ = std::io::stderr().lock().write_all(bytes);
}

fn detach(steps: &str) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // Only this process keeps the write end, and it is never closed, so the
    // copy sees end of file on stdin exactly when this process exits.
    unsafe { libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC) };
    let stdin = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // Not waited for: it must outlive this process, and is reaped by init.
    #[allow(clippy::zombie_processes)]
    Command::new(std::env::current_exe().unwrap())
        .args(steps.split(','))
        .stdin(Stdio::from(stdin))
        .spawn()
        .unwrap();
}

fn main() {
    // Tells the test this process group exists, so a failed test can kill it.
    event("spawned");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut steps = args.iter();
    while let Some(step) = steps.next() {
        let (name, argument) = step.split_once(':').unwrap_or((step, ""));
        match name {
            "count" => {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("count")
                    .unwrap();
                file.write_all(b"x").unwrap();
            }
            "event" => event(argument),
            "wait" => wait(argument),
            "wait-stdin" => drain(libc::STDIN_FILENO),
            "trap" => trap(argument),
            "traps" => {
                std::fs::write(argument, TRAPPED.load(Ordering::SeqCst).to_string()).unwrap()
            }
            "out" => write_out(argument.as_bytes()),
            "err" => write_err(argument.as_bytes()),
            "outx" => write_out(&hex(argument)),
            "errx" => write_err(&hex(argument)),
            "zeros" => {
                let (out, err) = argument.split_once(':').unwrap();
                let err: usize = err.parse().unwrap();
                let writer = std::thread::spawn(move || write_err(&vec![0; err]));
                write_out(&vec![0; out.parse().unwrap()]);
                writer.join().unwrap();
            }
            "stdin-empty" => {
                let mut bytes = Vec::new();
                std::io::stdin().read_to_end(&mut bytes).unwrap();
                if !bytes.is_empty() {
                    std::process::exit(argument.parse().unwrap());
                }
            }
            "env" => write_out(std::env::var_os(argument).unwrap_or_default().as_bytes()),
            "args" => {
                for rest in steps.by_ref() {
                    write_out(format!("{rest}|").as_bytes());
                }
            }
            "detach" => detach(argument),
            "raise" => unsafe {
                libc::signal(signal(argument), libc::SIG_DFL);
                libc::raise(signal(argument));
            },
            "exit" => std::process::exit(argument.parse().unwrap()),
            "exitenv" => {
                std::process::exit(std::env::var(argument).map_or(0, |code| code.parse().unwrap()))
            }
            _ => panic!("unknown step {step}"),
        }
    }
}
