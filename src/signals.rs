use anyhow::{Context, Result};
use std::sync::atomic::{AtomicI32, Ordering};

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
pub fn install() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // The handler only stores an atomic; all process and file operations stay outside it.
        unsafe {
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
