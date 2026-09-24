use anyhow::{Context, Result};
use std::{
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::Duration,
};

static PENDING: AtomicI32 = AtomicI32::new(0);
const SEALED: i32 = 1 << 16;

static SIGNAL: AtomicI32 = AtomicI32::new(0);
extern "C" fn handler(signal: libc::c_int) {
    let _ = SIGNAL.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
        Some((state & SEALED) | signal)
    });
    PENDING.store(signal, Ordering::Relaxed);
}
pub fn take_pending() -> i32 {
    PENDING.swap(0, Ordering::Relaxed)
}
pub fn received() -> i32 {
    SIGNAL.load(Ordering::SeqCst) & !SEALED
}
// This atomic operation orders cancellation against the decision to publish.
// Later signals affect only this caller's delivery, even while publication finishes.
pub fn seal_execution() -> i32 {
    SIGNAL.fetch_or(SEALED, Ordering::SeqCst) & !SEALED
}
pub enum Waited<T> {
    Done(T),
    Interrupted(i32),
    /// Every sender went away without sending.
    Closed,
}

/// Waits for `done` unless a signal arrives first. A signal that arrives by
/// the time the work completes still wins, so a caller never reports a result
/// with an exit status that ignores it. The caller returns right after an
/// interruption, so a worker blocked on an output consumer that stopped
/// reading must not prevent cancellation or keep the process alive.
pub fn wait<T>(done: &Receiver<T>) -> Waited<T> {
    wait_with(done, received)
}

fn wait_with<T>(done: &Receiver<T>, received: impl Fn() -> i32) -> Waited<T> {
    loop {
        let signal = received();
        if signal != 0 {
            return Waited::Interrupted(signal);
        }
        // Completion wakes immediately; the timeout only bounds signal latency.
        let waited = match done.recv_timeout(Duration::from_millis(10)) {
            Ok(value) => Waited::Done(value),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => Waited::Closed,
        };
        let signal = received();
        return if signal != 0 {
            Waited::Interrupted(signal)
        } else {
            waited
        };
    }
}

pub fn install() -> Result<()> {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        // The handler only stores an atomic; all process and file operations stay outside it.
        unsafe {
            let mut inherited: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &mut inherited) != 0 {
                return Err(std::io::Error::last_os_error()).context("read signal disposition");
            }
            // Keep signals the caller ignored (nohup, background jobs) ignored,
            // for this process and, through exec, for the child.
            if inherited.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error()).context("install signal handler");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::mpsc};

    #[test]
    fn a_signal_wins_even_as_the_work_completes() {
        let (sender, done) = mpsc::channel();
        sender.send(7).unwrap();
        assert!(matches!(wait_with(&done, || 0), Waited::Done(7)));

        sender.send(7).unwrap();
        let calls = Cell::new(0);
        let arrives_with_completion = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 0 }
        };
        assert!(matches!(
            wait_with(&done, arrives_with_completion),
            Waited::Interrupted(15)
        ));

        sender.send(7).unwrap();
        assert!(matches!(wait_with(&done, || 2), Waited::Interrupted(2)));
    }

    #[test]
    fn dropped_senders_close_the_wait_unless_a_signal_arrived() {
        let (sender, done) = mpsc::channel::<()>();
        drop(sender);
        assert!(matches!(wait_with(&done, || 0), Waited::Closed));
        let calls = Cell::new(0);
        let arrives_with_closing = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 0 }
        };
        assert!(matches!(
            wait_with(&done, arrives_with_closing),
            Waited::Interrupted(15)
        ));
    }
}
