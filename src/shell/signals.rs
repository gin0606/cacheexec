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
    wait_with(done, received, None)
}

/// Like [`wait`], but a signal still leaves `grace` for the work to complete,
/// counted from when the wait sees the signal. Completion within it wins, for
/// work whose result does not depend on the signal.
pub fn wait_with_grace<T>(done: &Receiver<T>, grace: Duration) -> Waited<T> {
    wait_with(done, received, Some(grace))
}

fn wait_with<T>(
    done: &Receiver<T>,
    received: impl Fn() -> i32,
    grace: Option<Duration>,
) -> Waited<T> {
    let settle = |waited| match waited {
        Ok(value) => Some(Waited::Done(value)),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(Waited::Closed),
    };
    loop {
        let signal = received();
        if signal != 0 {
            let Some(grace) = grace else {
                return Waited::Interrupted(signal);
            };
            // A later signal may have replaced the one that started the grace.
            return settle(done.recv_timeout(grace))
                .unwrap_or_else(|| Waited::Interrupted(received()));
        }
        // Completion wakes immediately; the timeout only bounds signal latency.
        let Some(waited) = settle(done.recv_timeout(Duration::from_millis(10))) else {
            continue;
        };
        // With a grace, completion wins over any signal.
        if grace.is_some() {
            return waited;
        }
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
    use std::{cell::Cell, sync::mpsc, thread, time::Instant};

    #[test]
    fn a_signal_wins_even_as_the_work_completes() {
        let (sender, done) = mpsc::channel();
        sender.send(7).unwrap();
        assert!(matches!(wait_with(&done, || 0, None), Waited::Done(7)));

        sender.send(7).unwrap();
        let calls = Cell::new(0);
        let arrives_with_completion = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 0 }
        };
        assert!(matches!(
            wait_with(&done, arrives_with_completion, None),
            Waited::Interrupted(15)
        ));

        sender.send(7).unwrap();
        assert!(matches!(
            wait_with(&done, || 2, None),
            Waited::Interrupted(2)
        ));
    }

    #[test]
    fn dropped_senders_close_the_wait_unless_a_signal_arrived() {
        let (sender, done) = mpsc::channel::<()>();
        drop(sender);
        assert!(matches!(wait_with(&done, || 0, None), Waited::Closed));
        let calls = Cell::new(0);
        let arrives_with_closing = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 0 }
        };
        assert!(matches!(
            wait_with(&done, arrives_with_closing, None),
            Waited::Interrupted(15)
        ));
    }

    #[test]
    fn a_signal_leaves_the_grace_for_the_work_to_complete() {
        let grace = Some(Duration::from_secs(5));
        let (sender, done) = mpsc::channel();
        let sending = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            sender.send(7).unwrap();
            sender
        });
        assert!(matches!(wait_with(&done, || 15, grace), Waited::Done(7)));

        let sender = sending.join().unwrap();
        sender.send(7).unwrap();
        let calls = Cell::new(0);
        let arrives_with_completion = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 0 }
        };
        assert!(matches!(
            wait_with(&done, arrives_with_completion, grace),
            Waited::Done(7)
        ));

        drop(sender);
        assert!(matches!(wait_with(&done, || 15, grace), Waited::Closed));
    }

    #[test]
    fn a_signal_interrupts_the_wait_once_the_grace_expires() {
        let grace = Duration::from_millis(30);
        let (_sender, done) = mpsc::channel::<()>();
        let started = Instant::now();
        assert!(matches!(
            wait_with(&done, || 15, Some(grace)),
            Waited::Interrupted(15)
        ));
        let waited = started.elapsed();
        assert!(
            waited >= grace && waited < Duration::from_secs(5),
            "{waited:?}"
        );

        // A signal that arrives during the wait also gets the whole grace.
        let calls = Cell::new(0);
        let arrives_later = || {
            calls.set(calls.get() + 1);
            if calls.get() > 2 { 15 } else { 0 }
        };
        let started = Instant::now();
        assert!(matches!(
            wait_with(&done, arrives_later, Some(grace)),
            Waited::Interrupted(15)
        ));
        let waited = started.elapsed();
        // The signal is seen after two 10ms polls, and the grace counts from then.
        assert!(
            waited >= grace + Duration::from_millis(20) && waited < Duration::from_secs(5),
            "{waited:?}"
        );

        // The interruption reports the signal received when the grace expires.
        let calls = Cell::new(0);
        let replaced = || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 { 15 } else { 2 }
        };
        assert!(matches!(
            wait_with(&done, replaced, Some(Duration::from_millis(1))),
            Waited::Interrupted(15)
        ));
    }

    #[test]
    fn without_a_signal_the_grace_wait_is_a_plain_wait() {
        let grace = Some(Duration::ZERO);
        let (sender, done) = mpsc::channel();
        sender.send(7).unwrap();
        assert!(matches!(wait_with(&done, || 0, grace), Waited::Done(7)));
        drop(sender);
        assert!(matches!(wait_with(&done, || 0, grace), Waited::Closed));
    }
}
