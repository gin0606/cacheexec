use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
struct Fixture {
    root: TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }
    fn spawn(&self, options: &[&str], script: &str) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
        if !options.contains(&"--ttl") {
            command.args(["--ttl", "1h"]);
        }
        command
            .current_dir(self.root.path())
            .args(["--cache-dir"])
            .arg(self.root.path().join("cache"))
            .args(options)
            .args(["--", "sh", "-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
    fn release(&self) {
        fs::write(self.root.path().join("go"), "").unwrap();
    }
    fn started(&self) {
        wait_until(|| self.root.path().join("count").exists());
    }
    fn count(&self) -> String {
        fs::read_to_string(self.root.path().join("count")).unwrap()
    }
    fn joined(&self, code: usize) {
        wait_until(|| {
            fs::read_dir(self.root.path().join("cache"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "active"))
                .any(|entry| fs::read(entry.path()).is_ok_and(|bytes| bytes.get(code) == Some(&1)))
        });
    }
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < end, "test timed out");
        thread::sleep(Duration::from_millis(10));
    }
}
fn finish(mut child: Child) -> Output {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= end {
            let _ = child.kill();
            panic!("child timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn signal(child: &Child, sig: i32) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
}
const SCRIPT: &str = "printf x >> count; while ! test -f go; do sleep 0.01; done; printf '\\377\\000out'; printf '\\376err' >&2; exit 7";
#[test]
fn mixed_policies_refresh_and_ttl_share_and_any_participant_can_save() {
    let f = Fixture::new();
    let leader = f.spawn(&["--include-codes", "0"], SCRIPT);
    f.started();
    let waiter = f.spawn(&["--refresh", "--include-codes", "7"], SCRIPT);
    f.joined(7);
    let other = f.spawn(&["--ttl", "0s", "--include-codes", "2"], SCRIPT);
    f.joined(2);
    f.release();
    let a = finish(leader);
    let b = finish(waiter);
    let c = finish(other);
    assert_eq!(c.stdout, a.stdout);
    assert_eq!(c.status.code(), Some(7));
    assert_eq!(a.status.code(), Some(7));
    assert_eq!(a.stdout, b.stdout);
    assert_eq!(a.stderr, b.stderr);
    assert_eq!(a.stdout, b"\xff\0out");
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    assert_eq!(f.count(), "x");
}
#[test]
fn excluded_results_are_shared_and_next_generation_does_not_destroy_waiter_output() {
    let f = Fixture::new();
    let leader = f.spawn(&["--include-codes", "0"], SCRIPT);
    f.started();
    let waiter = f.spawn(&["--exclude-codes", "0,7"], SCRIPT);
    f.joined(1);
    // The policy write precedes gate release; wait for the critical section to end.
    {
        use std::os::fd::AsRawFd;
        let lock = fs::read_dir(f.root.path().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "lock"))
            .unwrap();
        let gate = fs::OpenOptions::new().write(true).open(lock).unwrap();
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) }, 0);
    }
    signal(&waiter, libc::SIGSTOP);
    f.release();
    let first = finish(leader);
    let next = finish(f.spawn(&["--include-codes", "0"], SCRIPT));
    assert_eq!(f.count(), "xx");
    signal(&waiter, libc::SIGCONT);
    let previous = finish(waiter);
    assert_eq!(previous.stdout, first.stdout);
    assert_eq!(previous.stderr, first.stderr);
    assert_eq!(next.status.code(), Some(7));
    assert_eq!(previous.status.code(), Some(7));
}
#[test]
fn separate_keys_progress_independently_and_refresh_hides_previous_result() {
    let f = Fixture::new();
    f.release();
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    fs::remove_file(f.root.path().join("go")).unwrap();
    let leader = f.spawn(&["--refresh", "--include-codes", "0"], SCRIPT);
    wait_until(|| f.count() == "xx");
    assert!(
        finish(f.spawn(&["--key", "separate"], "true"))
            .status
            .success()
    );
    let waiter = f.spawn(&["--include-codes", "7"], SCRIPT);
    f.joined(7);
    f.release();
    assert_eq!(finish(leader).status.code(), Some(7));
    assert_eq!(finish(waiter).status.code(), Some(7));
    assert_eq!(f.count(), "xx");
}
#[test]
fn owner_interrupt_propagates_and_is_never_saved_even_if_child_traps_it() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let f = Fixture::new();
        let script =
            "trap 'exit 0' INT TERM; printf x >> count; while ! test -f go; do sleep 0.02; done";
        let owner = f.spawn(&["--include-codes", "0"], script);
        f.started();
        let waiter = f.spawn(&["--include-codes", "1"], script);
        f.joined(1);
        signal(&owner, sig);
        assert_eq!(finish(owner).status.code(), Some(128 + sig));
        assert_eq!(finish(waiter).status.code(), Some(128 + sig));
        f.release();
        assert!(finish(f.spawn(&[], script)).status.success());
        assert_eq!(f.count(), "xx");
    }
}
#[test]
fn waiter_interrupt_does_not_stop_owner() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let f = Fixture::new();
        let owner = f.spawn(&["--include-codes", "0"], SCRIPT);
        f.started();
        let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
        f.joined(1);
        signal(&waiter, sig);
        assert_eq!(finish(waiter).status.code(), Some(128 + sig));
        f.release();
        assert_eq!(finish(owner).status.code(), Some(7));
        assert_eq!(f.count(), "x");
    }
}
#[test]
fn sudden_owner_death_fails_waiters_without_retry_and_new_call_can_run() {
    let f = Fixture::new();
    let owner = f.spawn(&["--include-codes", "0"], SCRIPT);
    f.started();
    let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
    f.joined(1);
    signal(&owner, libc::SIGKILL);
    let failed = finish(waiter);
    assert_eq!(failed.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("owner disappeared"));
    assert_eq!(f.count(), "x");
    f.release();
    finish(owner);
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    assert_eq!(f.count(), "xx");
}
#[test]
fn save_failure_is_shared_without_retry() {
    let f = Fixture::new();
    let owner = f.spawn(&["--include-codes", "7"], SCRIPT);
    f.started();
    let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
    f.joined(1);
    let active = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|s| s == "active"))
        .unwrap();
    fs::create_dir(Path::new(&active).with_extension("result")).unwrap();
    f.release();
    for result in [finish(owner), finish(waiter)] {
        assert_eq!(result.status.code(), Some(125));
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("child already completed with exit code 7")
        );
    }
    assert_eq!(f.count(), "x");
}

#[test]
fn owner_streams_and_many_late_waiters_receive_complete_bytes() {
    use std::io::Read;
    let f = Fixture::new();
    let script = "printf x >> count; printf start; while ! test -f go; do sleep 0.01; done; printf end; printf err >&2; exit 7";
    let mut owner = f.spawn(&["--include-codes", "0"], script);
    let mut prefix = [0; 5];
    owner
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut prefix)
        .unwrap();
    assert_eq!(&prefix, b"start");
    let mut waiters = Vec::new();
    for code in 1..7 {
        waiters.push(f.spawn(&["--include-codes", &code.to_string()], script));
        f.joined(code);
    }
    f.release();
    assert_eq!(finish(owner).stdout, b"end");
    for waiter in waiters {
        let output = finish(waiter);
        assert_eq!(output.stdout, b"startend");
        assert_eq!(output.stderr, b"err");
        assert_eq!(output.status.code(), Some(7));
    }
    assert_eq!(f.count(), "x");
}

#[test]
fn lock_storage_fault_is_an_error_without_execution() {
    let f = Fixture::new();
    f.release();
    finish(f.spawn(&[], SCRIPT));
    let lock = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|s| s == "lock"))
        .unwrap();
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    let failed = finish(f.spawn(&["--refresh"], SCRIPT));
    assert_eq!(failed.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("lock"));
    assert_eq!(f.count(), "x");
}

#[test]
fn one_signal_allows_child_cleanup_to_finish() {
    let f = Fixture::new();
    let script = "trap 'printf t >> signals; sleep 0.2; printf cleaned; exit 0' TERM; printf x >> count; while ! test -f go; do sleep 0.01; done";
    let owner = f.spawn(&[], script);
    f.started();
    let start = Instant::now();
    signal(&owner, libc::SIGTERM);
    let output = finish(owner);
    assert_eq!(output.status.code(), Some(143));
    assert_eq!(output.stdout, b"cleaned");
    assert_eq!(fs::read(f.root.path().join("signals")).unwrap(), b"t");
    assert!(start.elapsed() >= Duration::from_millis(180));
}

#[test]
fn interruption_while_waiting_to_publish_is_shared_and_not_saved() {
    use std::os::fd::AsRawFd;
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let f = Fixture::new();
        let owner = f.spawn(&["--include-codes", "7"], SCRIPT);
        f.started();
        let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
        f.joined(1);
        let lock = fs::read_dir(f.root.path().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "lock"))
            .unwrap();
        let gate = fs::OpenOptions::new().write(true).open(lock).unwrap();
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) }, 0);
        f.release();
        thread::sleep(Duration::from_millis(100));
        signal(&owner, sig);
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert_eq!(finish(owner).status.code(), Some(128 + sig));
        assert_eq!(finish(waiter).status.code(), Some(128 + sig));
        assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
        assert_eq!(f.count(), "xx");
    }
}

#[test]
fn shared_write_failure_never_publishes_success() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    let script = "printf x >> count; while ! test -f go; do sleep 0.01; done; dd if=/dev/zero bs=1000 count=1 2>/dev/null";
    let mut command = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
    command
        .current_dir(f.root.path())
        .arg("--cache-dir")
        .arg(f.root.path().join("cache"))
        .args([
            "--ttl",
            "1h",
            "--include-codes",
            "0",
            "--",
            "sh",
            "-c",
            script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let limit = libc::rlimit {
                rlim_cur: 1200,
                rlim_max: 1200,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let owner = command.spawn().unwrap();
    f.started();
    let waiter = f.spawn(&["--include-codes", "1"], script);
    f.joined(1);
    f.release();
    for output in [finish(owner), finish(waiter)] {
        assert_eq!(output.status.code(), Some(125));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("child already completed with exit code 0")
        );
    }
    assert_eq!(f.count(), "x");
    assert!(finish(f.spawn(&[], script)).status.success());
    assert_eq!(f.count(), "xx");
}

#[test]
fn ttl_begins_at_child_exit_even_when_descendant_holds_output_open() {
    let f = Fixture::new();
    let script = "printf x >> count; sleep 0.3 & exit 0";
    assert!(
        finish(f.spawn(&["--ttl", "100ms"], script))
            .status
            .success()
    );
    assert!(
        finish(f.spawn(&["--ttl", "100ms"], script))
            .status
            .success()
    );
    assert_eq!(f.count(), "xx");
}

#[test]
fn replay_cancellation_exits_with_unread_stdout_or_stderr() {
    use std::io::Read;
    for stderr in [false, true] {
        for sig in [libc::SIGINT, libc::SIGTERM] {
            let f = Fixture::new();
            let script = if stderr {
                "printf x >> count; while ! test -f go; do sleep 0.01; done; (dd if=/dev/zero bs=131072 count=1 2>/dev/null) >&2"
            } else {
                "printf x >> count; while ! test -f go; do sleep 0.01; done; dd if=/dev/zero bs=131072 count=1 2>/dev/null"
            };
            let mut owner = f.spawn(&["--include-codes", "0"], script);
            f.started();
            let mut owner_out = owner.stdout.take().unwrap();
            let mut owner_err = owner.stderr.take().unwrap();
            let drain_out = thread::spawn(move || {
                let mut bytes = Vec::new();
                owner_out.read_to_end(&mut bytes).unwrap();
            });
            let drain_err = thread::spawn(move || {
                let mut bytes = Vec::new();
                owner_err.read_to_end(&mut bytes).unwrap();
            });
            let mut waiter = f.spawn(&["--include-codes", "1"], script);
            f.joined(1);
            f.release();
            assert!(finish(owner).status.success());
            drain_out.join().unwrap();
            drain_err.join().unwrap();
            let mut byte = [0];
            if stderr {
                waiter
                    .stderr
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut byte)
                    .unwrap();
            } else {
                waiter
                    .stdout
                    .as_mut()
                    .unwrap()
                    .read_exact(&mut byte)
                    .unwrap();
            }
            signal(&waiter, sig);
            assert_eq!(finish(waiter).status.code(), Some(128 + sig));
            let mut hit = f.spawn(&[], script);
            if stderr {
                hit.stderr.as_mut().unwrap().read_exact(&mut byte).unwrap();
            } else {
                hit.stdout.as_mut().unwrap().read_exact(&mut byte).unwrap();
            }
            signal(&hit, sig);
            assert_eq!(finish(hit).status.code(), Some(128 + sig));
            assert_eq!(f.count(), "x");
        }
    }
}

#[test]
fn owner_cancellation_publishes_even_with_blocked_output_consumer() {
    use std::io::Read;
    for stderr in [false, true] {
        let f = Fixture::new();
        let script = if stderr {
            "printf x >> count; while ! test -f go; do sleep 0.01; done; (dd if=/dev/zero bs=131072 count=1 2>/dev/null) >&2; printf done > finished"
        } else {
            "printf x >> count; while ! test -f go; do sleep 0.01; done; dd if=/dev/zero bs=131072 count=1 2>/dev/null; printf done > finished"
        };
        let owner = f.spawn(&["--include-codes", "0"], script);
        f.started();
        let mut waiter = f.spawn(&["--include-codes", "1"], script);
        f.joined(1);
        let mut waiter_out = waiter.stdout.take().unwrap();
        let mut waiter_err = waiter.stderr.take().unwrap();
        let drain_out = thread::spawn(move || {
            let mut bytes = Vec::new();
            waiter_out.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let drain_err = thread::spawn(move || {
            let mut bytes = Vec::new();
            waiter_err.read_to_end(&mut bytes).unwrap();
            bytes
        });
        f.release();
        wait_until(|| f.root.path().join("finished").exists());
        signal(&owner, libc::SIGTERM);
        assert_eq!(finish(owner).status.code(), Some(143));
        assert_eq!(finish(waiter).status.code(), Some(143));
        let out = drain_out.join().unwrap();
        let err = drain_err.join().unwrap();
        assert_eq!(if stderr { err.len() } else { out.len() }, 131072);
        assert_eq!(f.count(), "x");
    }
}

#[test]
fn interruption_with_storage_error_does_not_block_on_stderr_diagnostic() {
    let f = Fixture::new();
    let script = "printf x >> count; (dd if=/dev/zero bs=131072 count=1 2>/dev/null) >&2; printf done > finished";
    let owner = f.spawn(&[], script);
    f.started();
    wait_until(|| f.root.path().join("finished").exists());
    let active = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|s| s == "active"))
        .unwrap();
    fs::create_dir(active.with_extension("result")).unwrap();
    signal(&owner, libc::SIGTERM);
    assert_eq!(finish(owner).status.code(), Some(125));
}
