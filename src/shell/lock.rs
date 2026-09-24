use crate::shell::signals;
use anyhow::{Context, Result, bail};
use std::{
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::MetadataExt,
    path::Path,
    thread,
    time::Duration,
};

pub fn try_lock(file: &File, exclusive: bool) -> Result<bool> {
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error).context("cache lock failed")
}
pub fn lock(file: &File, interruptible: bool) -> Result<bool> {
    loop {
        if interruptible && signals::received() != 0 {
            return Ok(false);
        }
        if try_lock(file, true)? {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(10));
    }
}
/// Opens and locks a key's gate. Cleanup may unlink an idle `.lock` while
/// holding it, so a lock only counts while the locked inode is still linked;
/// otherwise a caller that opened the removed inode retries with a new one.
/// The link count comes from the descriptor alone, because some filesystems
/// report different identities for `fstat` and `stat` of the same file.
/// Returns `None` when interrupted, or when busy without `wait`.
pub fn acquire_gate(path: &Path, wait: bool) -> Result<Option<File>> {
    acquire_gate_with(path, wait, || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    })
}

fn acquire_gate_with(
    path: &Path,
    wait: bool,
    mut open: impl FnMut() -> std::io::Result<File>,
) -> Result<Option<File>> {
    // Each retry needs a concurrent cleanup, so a bound only guards against a
    // filesystem that never reports a link, which must fail instead of spinning.
    for _ in 0..100 {
        let gate = open().context("open key lock")?;
        let locked = if wait {
            lock(&gate, true)?
        } else {
            try_lock(&gate, true)?
        };
        if !locked {
            return Ok(None);
        }
        if gate.metadata().context("inspect key lock")?.nlink() > 0 {
            return Ok(Some(gate));
        }
    }
    bail!("key lock {path:?} kept being removed while being locked")
}
pub fn unlock(file: &File) -> Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        return Err(std::io::Error::last_os_error()).context("release cache lock");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::mpsc};

    fn identity(file: &File) -> (u64, u64) {
        let metadata = file.metadata().unwrap();
        (metadata.dev(), metadata.ino())
    }

    fn create(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_gate_unlinked_while_waiting_is_replaced_by_the_current_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.lock");
        // Cleanup unlinks the gate this caller opened, and a newer caller
        // creates and holds the current one.
        let current = create(&path);
        let stale = directory.path().join("stale.lock");
        let stale_file = create(&stale);
        assert!(try_lock(&current, true).unwrap());
        let current_identity = identity(&current);
        let (reopened, reopening) = mpsc::channel();
        let caller = thread::spawn({
            let path = path.clone();
            let mut opens = 0;
            move || {
                acquire_gate_with(&path, true, || {
                    opens += 1;
                    if opens == 1 {
                        fs::remove_file(&stale)?;
                        return stale_file.try_clone();
                    }
                    reopened.send(()).unwrap();
                    OpenOptions::new().read(true).write(true).open(&path)
                })
            }
        });
        reopening.recv().unwrap();
        // The caller now waits on the current gate, which is still held.
        assert!(!caller.is_finished());
        unlock(&current).unwrap();
        let gate = caller.join().unwrap().unwrap().unwrap();
        assert_eq!(identity(&gate), current_identity);
    }

    #[test]
    fn a_gate_unlinked_without_replacement_is_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.lock");
        let removed = create(&path);
        let removed_identity = identity(&removed);
        fs::remove_file(&path).unwrap();
        let mut opens = 0;
        let gate = acquire_gate_with(&path, false, || {
            opens += 1;
            if opens == 1 {
                removed.try_clone()
            } else {
                Ok(create(&path))
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(opens, 2);
        assert_ne!(identity(&gate), removed_identity);
        assert_eq!(gate.metadata().unwrap().nlink(), 1);
    }

    #[test]
    fn a_gate_that_is_never_linked_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.lock");
        let removed = create(&path);
        fs::remove_file(&path).unwrap();
        let error = acquire_gate_with(&path, false, || removed.try_clone()).unwrap_err();
        assert!(
            format!("{error:#}").contains("kept being removed"),
            "{error:#}"
        );
    }

    #[test]
    fn a_busy_gate_is_skipped_without_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.lock");
        let held = create(&path);
        assert!(try_lock(&held, true).unwrap());
        assert!(acquire_gate(&path, false).unwrap().is_none());
    }
}
