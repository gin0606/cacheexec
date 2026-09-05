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
fn shared_output_is_private_even_with_permissive_umask() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let f = Fixture::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
    command
        .current_dir(f.root.path())
        .arg("--cache-dir")
        .arg(f.root.path().join("cache"))
        .args(["--ttl", "1h", "--", "sh", "-c", SCRIPT])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    let owner = command.spawn().unwrap();
    f.started();
    let active = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "active"))
        .unwrap();
    let mode = fs::metadata(active).unwrap().permissions().mode() & 0o777;
    f.release();
    assert_eq!(finish(owner).status.code(), Some(7));
    assert_eq!(mode, 0o600);
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    assert_eq!(f.count(), "x");
}

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
fn owner_delivery_cancellation_preserves_published_result() {
    use std::io::Read;
    for (stderr, sig) in [false, true]
        .into_iter()
        .flat_map(|stderr| [0, libc::SIGINT, libc::SIGTERM].map(|sig| (stderr, sig)))
    {
        let f = Fixture::new();
        let script = if stderr {
            "printf x >> count; while ! test -f go; do sleep 0.01; done; (dd if=/dev/zero bs=131072 count=1 2>/dev/null) >&2; printf done > finished"
        } else {
            "printf x >> count; while ! test -f go; do sleep 0.01; done; dd if=/dev/zero bs=131072 count=1 2>/dev/null; printf done > finished"
        };
        let mut owner = f.spawn(&["--include-codes", "0"], script);
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
        assert_eq!(finish(waiter).status.code(), Some(0));
        assert!(owner.try_wait().unwrap().is_none());
        if sig != 0 {
            signal(&owner, sig);
            assert_eq!(finish(owner).status.code(), Some(128 + sig));
        } else {
            let delivered = owner.wait_with_output().unwrap();
            assert_eq!(delivered.status.code(), Some(0));
            assert_eq!(
                if stderr {
                    delivered.stderr.len()
                } else {
                    delivered.stdout.len()
                },
                131072
            );
        }
        let hit = f.spawn(&[], script).wait_with_output().unwrap();
        assert_eq!(hit.status.code(), Some(0));
        assert_eq!(
            if stderr {
                hit.stderr.len()
            } else {
                hit.stdout.len()
            },
            131072
        );
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

#[test]
fn cleanup_preserves_running_refresh_and_undelivered_waiter_generation() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let clear = || {
        Command::new(env!("CARGO_BIN_EXE_cacheexec"))
            .arg("--cache-dir")
            .arg(f.root.path().join("cache"))
            .arg("--clear")
            .output()
            .unwrap()
    };
    f.release();
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    fs::remove_file(f.root.path().join("go")).unwrap();
    let owner = f.spawn(&["--refresh", "--include-codes", "7"], SCRIPT);
    wait_until(|| f.count() == "xx");
    let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
    f.joined(1);
    let gate_path = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "lock"))
        .unwrap();
    let gate = fs::OpenOptions::new().write(true).open(&gate_path).unwrap();
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) }, 0);
    signal(&waiter, libc::SIGSTOP);
    assert!(String::from_utf8_lossy(&clear().stdout).contains("skipped=1"));
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
    assert!(String::from_utf8_lossy(&clear().stdout).contains("skipped=1"));
    f.release();
    let first = finish(owner);
    assert!(String::from_utf8_lossy(&clear().stdout).contains("removed=1"));
    assert!(gate_path.exists());
    let next = finish(f.spawn(&[], SCRIPT));
    signal(&waiter, libc::SIGCONT);
    let replay = finish(waiter);
    assert_eq!(replay.stdout, first.stdout);
    assert_eq!(replay.stderr, first.stderr);
    assert_eq!(replay.status.code(), Some(7));
    assert_eq!(next.status.code(), Some(7));
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    assert_eq!(f.count(), "xxx");
}

#[test]
fn cleanup_reclaims_abandoned_marker_without_retrying_waiter() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let owner = f.spawn(&["--include-codes", "0"], SCRIPT);
    f.started();
    let waiter = f.spawn(&["--include-codes", "1"], SCRIPT);
    f.joined(1);
    let gate_path = fs::read_dir(f.root.path().join("cache"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "lock"))
        .unwrap();
    let gate = fs::OpenOptions::new().write(true).open(gate_path).unwrap();
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) }, 0);
    signal(&waiter, libc::SIGSTOP);
    assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
    signal(&owner, libc::SIGKILL);
    finish(owner);
    let clear = Command::new(env!("CARGO_BIN_EXE_cacheexec"))
        .arg("--cache-dir")
        .arg(f.root.path().join("cache"))
        .arg("--clear")
        .output()
        .unwrap();
    assert!(clear.status.success());
    assert!(String::from_utf8_lossy(&clear.stdout).contains("abandoned=1"));
    signal(&waiter, libc::SIGCONT);
    assert_eq!(finish(waiter).status.code(), Some(125));
    assert_eq!(f.count(), "x");
    f.release();
    assert_eq!(finish(f.spawn(&[], SCRIPT)).status.code(), Some(7));
    assert_eq!(f.count(), "xx");
}

const QUIET: &str = "printf x >> count; while ! test -f go; do sleep 0.01; done; exit 7";

fn verbose_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn decision_before_completion(child: &mut Child, action: &str) {
    use std::io::{BufRead, BufReader};
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        while line.trim().is_empty() {
            line.clear();
            if stderr.read_line(&mut line).unwrap() == 0 {
                break;
            }
        }
        sender.send((line, stderr.into_inner())).unwrap();
    });
    let (line, stderr) = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("decision was delayed until completion");
    assert!(
        line.starts_with(&format!("cacheexec: verbose: {action}")),
        "{line}"
    );
    assert!(!line.contains(" age="));
    child.stderr = Some(stderr);
}

#[test]
fn verbose_mixed_participants_report_decisions_early_and_actual_saving() {
    for (owner_verbose, waiter_verbose) in [(true, false), (false, true), (true, true)] {
        for save in [false, true] {
            let f = Fixture::new();
            let mut owner_options = vec!["--include-codes", "0"];
            if owner_verbose {
                owner_options.push("--verbose");
            }
            let mut owner = f.spawn(&owner_options, QUIET);
            f.started();
            if owner_verbose {
                decision_before_completion(&mut owner, "run reason=missing");
            }
            let mut waiter_options = vec![
                "--refresh",
                "--ttl",
                "0s",
                "--include-codes",
                if save { "1,7" } else { "1" },
            ];
            if waiter_verbose {
                waiter_options.push("--verbose");
            }
            let mut waiter = f.spawn(&waiter_options, QUIET);
            f.joined(1);
            if waiter_verbose {
                decision_before_completion(&mut waiter, "join");
            }
            let mut excluded = f.spawn(&["--verbose", "--exclude-codes", "0,1,7"], QUIET);
            f.joined(2);
            decision_before_completion(&mut excluded, "join");
            let active = fs::read_dir(f.root.path().join("cache"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|e| e == "active"))
                .unwrap();
            let generation = fs::File::open(active).unwrap();
            f.release();
            let expected = if save {
                "completed exit=7 saved=yes"
            } else {
                "completed exit=7 saved=no reason=participant-policy"
            };
            for (output, verbose) in [
                (finish(owner), owner_verbose),
                (finish(waiter), waiter_verbose),
                (finish(excluded), true),
            ] {
                assert_eq!(output.status.code(), Some(7));
                if verbose {
                    assert!(
                        verbose_text(&output).contains(expected),
                        "{}",
                        verbose_text(&output)
                    );
                } else {
                    assert!(output.stderr.is_empty());
                }
            }
            use std::io::Read;
            let mut bytes = Vec::new();
            (&generation).read_to_end(&mut bytes).unwrap();
            assert!(!bytes.windows(18).any(|w| w == b"cacheexec: verbose:"));
            assert_eq!(f.count(), "x");
            let next = finish(f.spawn(&["--verbose"], QUIET));
            assert!(verbose_text(&next).contains(if save {
                "hit age="
            } else {
                "run reason=missing"
            }));
            assert_eq!(f.count(), if save { "x" } else { "xx" });
        }
    }
}

#[test]
fn verbose_late_waiter_keeps_its_generation_saving_status() {
    use std::os::fd::AsRawFd;
    for first_saved in [false, true] {
        let f = Fixture::new();
        let owner = f.spawn(
            &[
                "--verbose",
                "--include-codes",
                if first_saved { "7" } else { "0" },
            ],
            QUIET,
        );
        f.started();
        let mut waiter = f.spawn(&["--verbose", "--include-codes", "1"], QUIET);
        f.joined(1);
        decision_before_completion(&mut waiter, "join");
        let path = fs::read_dir(f.root.path().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "lock"))
            .unwrap();
        let gate = fs::OpenOptions::new().write(true).open(path).unwrap();
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) }, 0);
        signal(&waiter, libc::SIGSTOP);
        assert_eq!(unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_UN) }, 0);
        f.release();
        let first = finish(owner);
        let next = finish(f.spawn(
            &[
                "--verbose",
                "--refresh",
                "--include-codes",
                if first_saved { "0" } else { "7" },
            ],
            QUIET,
        ));
        signal(&waiter, libc::SIGCONT);
        let late = finish(waiter);
        let saved = if first_saved {
            "saved=yes"
        } else {
            "saved=no reason=participant-policy"
        };
        assert!(verbose_text(&first).contains(saved));
        assert!(
            verbose_text(&late).contains(saved),
            "{}",
            verbose_text(&late)
        );
        assert!(verbose_text(&next).contains(if first_saved {
            "saved=no reason=participant-policy"
        } else {
            "saved=yes"
        }));
        assert_eq!(late.status.code(), Some(7));
        assert_eq!(f.count(), "xx");
    }
}

#[test]
fn verbose_owner_and_waiter_interruptions_preserve_uncertainty() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        for interrupt_owner in [false, true] {
            let f = Fixture::new();
            let owner = f.spawn(&["--verbose", "--include-codes", "7"], QUIET);
            f.started();
            let mut waiter = f.spawn(&["--verbose", "--include-codes", "1"], QUIET);
            f.joined(1);
            decision_before_completion(&mut waiter, "join");
            signal(if interrupt_owner { &owner } else { &waiter }, sig);
            let waited = finish(waiter);
            assert_eq!(waited.status.code(), Some(128 + sig));
            let text = verbose_text(&waited);
            assert!(text.contains(&format!("interrupted exit={}", 128 + sig)));
            assert!(
                text.contains(if interrupt_owner {
                    "saved=no reason=interrupted"
                } else {
                    "saved=unknown reason=waiter-interrupted"
                }),
                "{text}"
            );
            f.release();
            let owned = finish(owner);
            assert_eq!(
                owned.status.code(),
                Some(if interrupt_owner { 128 + sig } else { 7 })
            );
            assert!(verbose_text(&owned).contains(if interrupt_owner {
                "saved=no reason=interrupted"
            } else {
                "saved=yes"
            }));
            assert_eq!(f.count(), "x");
        }
    }
}

#[test]
fn verbose_owner_death_and_save_failure_never_report_success() {
    for death in [false, true] {
        let f = Fixture::new();
        let owner = f.spawn(&["--verbose", "--include-codes", "7"], QUIET);
        f.started();
        let mut waiter = f.spawn(&["--verbose", "--include-codes", "1"], QUIET);
        f.joined(1);
        decision_before_completion(&mut waiter, "join");
        if death {
            signal(&owner, libc::SIGKILL);
        } else {
            let active = fs::read_dir(f.root.path().join("cache"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.extension().is_some_and(|e| e == "active"))
                .unwrap();
            fs::create_dir(active.with_extension("result")).unwrap();
        }
        f.release();
        let owned = finish(owner);
        let waited = finish(waiter);
        assert_eq!(waited.status.code(), Some(125));
        for output in if death {
            vec![waited]
        } else {
            vec![owned, waited]
        } {
            let text = verbose_text(&output);
            assert!(
                text.contains("failed saved=unknown reason=failure"),
                "{text}"
            );
            assert!(!text.contains("saved=yes") && !text.contains("completed exit="));
        }
        assert_eq!(f.count(), "x");
    }
}

#[test]
fn verbose_closed_or_stalled_stderr_cannot_block_execution_saving_or_waiters() {
    use std::{
        io::Write,
        os::fd::{AsRawFd, FromRawFd},
    };
    for closed in [false, true] {
        for blocked_owner in [false, true] {
            let f = Fixture::new();
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let reader = unsafe { fs::File::from_raw_fd(fds[0]) };
            let mut writer = unsafe { fs::File::from_raw_fd(fds[1]) };
            // Fill the pipe before spawning; the child's descriptor remains blocking.
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
            if !closed {
                while writer.write(&[b'x'; 4096]).is_ok() {}
            }
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, 0) },
                0
            );
            let reader = if closed {
                drop(reader);
                None
            } else {
                Some(reader)
            };
            let spawn_blocked = |policy| {
                Command::new(env!("CARGO_BIN_EXE_cacheexec"))
                    .current_dir(f.root.path())
                    .arg("--cache-dir")
                    .arg(f.root.path().join("cache"))
                    .args([
                        "--ttl",
                        "1h",
                        "--verbose",
                        "--include-codes",
                        policy,
                        "--",
                        "sh",
                        "-c",
                        QUIET,
                    ])
                    .stderr(Stdio::from(writer.try_clone().unwrap()))
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let owner = if blocked_owner {
                spawn_blocked("7")
            } else {
                f.spawn(&["--verbose", "--include-codes", "7"], QUIET)
            };
            f.started();
            let waiter = if blocked_owner {
                f.spawn(&["--verbose", "--include-codes", "1"], QUIET)
            } else {
                spawn_blocked("1")
            };
            f.joined(1);
            f.release();
            assert_eq!(finish(owner).status.code(), Some(7));
            assert_eq!(finish(waiter).status.code(), Some(7));
            assert_eq!(finish(spawn_blocked("7")).status.code(), Some(7));
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            let hit = finish(f.spawn(&[], QUIET));
            assert_eq!(hit.status.code(), Some(7));
            assert!(hit.stderr.is_empty());
            assert_eq!(f.count(), "x");
            drop(reader);
        }
    }
}

#[test]
fn verbose_file_size_write_failure_does_not_deliver_sigxfsz() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    f.release();
    let log = fs::File::create(f.root.path().join("diagnostic")).unwrap();
    log.set_len(4096).unwrap();
    let mut log = log;
    use std::io::{Seek, SeekFrom};
    log.seek(SeekFrom::End(0)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
    command
        .current_dir(f.root.path())
        .arg("--cache-dir")
        .arg(f.root.path().join("cache"))
        .args(["--ttl", "1h", "--verbose", "--", "sh", "-c", QUIET])
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 4096,
                rlim_max: 4096,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert_eq!(finish(command.spawn().unwrap()).status.code(), Some(7));
    assert_eq!(finish(f.spawn(&[], QUIET)).status.code(), Some(7));
    assert_eq!(f.count(), "x");
}

#[test]
fn verbose_background_terminal_does_not_suspend_execution() {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    };
    const HELPER: &str = "CACHEEXEC_TEST_BACKGROUND_TERMINAL";
    if std::env::var_os(HELPER).is_none() {
        let output = finish(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "verbose_background_terminal_does_not_suspend_execution",
                    "--nocapture",
                ])
                .env(HELPER, "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // A subprocess owns this controlling terminal so job control cannot affect
    // the test harness. Keep its master open until the subprocess exits (SIGHUP).
    assert!(unsafe { libc::setsid() } >= 0);
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let _master = std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_fd(master) });
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    assert_eq!(
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) },
        0
    );
    let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) },
        0
    );
    attributes.c_lflag |= libc::TOSTOP;
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) },
        0
    );
    assert_eq!(
        unsafe { libc::tcsetpgrp(slave.as_raw_fd(), libc::getpgrp()) },
        0
    );
    let f = Fixture::new();
    for options in [&[][..], &["--verbose", "--refresh"][..], &["--verbose"][..]] {
        // waitpid below reaps the child and also observes job-control stops.
        #[allow(clippy::zombie_processes)]
        let child = Command::new(env!("CARGO_BIN_EXE_cacheexec"))
            .current_dir(f.root.path())
            .process_group(0)
            .arg("--cache-dir")
            .arg(f.root.path().join("cache"))
            .args(["--ttl", "1h"])
            .args(options)
            .args(["--", "sh", "-c", "printf x >> count; exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = 0;
        loop {
            let found = unsafe {
                libc::waitpid(
                    child.id() as i32,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED,
                )
            };
            assert!(found >= 0);
            if (found > 0 && libc::WIFSTOPPED(status)) || Instant::now() >= deadline {
                unsafe {
                    libc::killpg(child.id() as i32, libc::SIGKILL);
                    libc::waitpid(child.id() as i32, std::ptr::null_mut(), 0);
                }
                panic!("background execution stopped or timed out: status={status}");
            }
            if found > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 7);
    }
    assert_eq!(f.count(), "xx");
}

#[test]
fn owner_delivery_failure_does_not_fail_waiters_or_discard_cache() {
    for stderr in [false, true] {
        let f = Fixture::new();
        let script = "printf x >> count; while ! test -f go; do sleep 0.01; done; printf out; printf err >&2";
        let mut owner = f.spawn(&["--include-codes", "0"], script);
        f.started();
        let waiter = f.spawn(&["--include-codes", "1"], script);
        f.joined(1);
        if stderr {
            drop(owner.stderr.take());
        } else {
            drop(owner.stdout.take());
        }
        f.release();
        assert_eq!(finish(owner).status.code(), Some(125));
        for result in [finish(waiter), finish(f.spawn(&[], script))] {
            assert_eq!(result.status.code(), Some(0));
            assert_eq!(result.stdout, b"out");
            assert_eq!(result.stderr, b"err");
        }
        assert_eq!(f.count(), "x");
    }
}
