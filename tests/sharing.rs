mod common;

use common::{FullPipe, Proc, Sandbox, child_stderr, code, text, verbose_lines};
use std::{fs, os::fd::AsRawFd, process::Output};

/// Counts the execution, reports `started`, waits for `go`, then writes
/// non-UTF-8 bytes to both streams and exits 7.
const SCRIPT: [&str; 6] = [
    "count",
    "event:started",
    "wait:go",
    "outx:ff006f7574",
    "errx:fe657272",
    "exit:7",
];

/// Like SCRIPT, without output.
const QUIET: [&str; 4] = ["count", "event:started", "wait:go", "exit:7"];

/// Starts the execution owner and waits until its child is running.
fn owner<'a>(s: &'a Sandbox, options: &[&str], steps: &[&str]) -> Proc<'a> {
    let owner = s.spawn("owner", s.cacheexec(&with_ttl(options), steps));
    s.wait_event("started");
    owner
}

/// Starts a caller that joins the running execution, and waits until it has:
/// it adds a vote for `code`, an exit code only it allows.
fn waiter<'a>(
    s: &'a Sandbox,
    name: &str,
    options: &[&str],
    steps: &[&str],
    code: usize,
) -> Proc<'a> {
    quiet_join(s, name, s.cacheexec(&with_ttl(options), steps), code)
}

/// Spawns a caller and waits for its vote for `code`, which must be new.
///
/// A vote is written before the caller releases the key's lock, so do not
/// stop it right after; use [`verbose_waiter`] for that.
fn quiet_join<'a>(
    s: &'a Sandbox,
    name: &str,
    command: std::process::Command,
    code: usize,
) -> Proc<'a> {
    let voted = || {
        s.cache_files("active")
            .iter()
            .any(|path| fs::read(path).is_ok_and(|bytes| bytes.get(code) == Some(&1)))
    };
    assert!(
        !voted(),
        "{code} was already voted for, so it cannot show {name} joining"
    );
    let proc = s.spawn(name, command);
    s.wait_until(&format!("{name} voting for {code}"), voted);
    proc
}

/// Starts a caller with `--verbose` and waits for its `join` decision, which
/// it reports after releasing the key's lock.
fn verbose_waiter<'a>(s: &'a Sandbox, name: &str, options: &[&str], steps: &[&str]) -> Proc<'a> {
    let mut options = with_ttl(options);
    options.push("--verbose");
    let waiter = s.spawn(name, s.cacheexec(&options, steps));
    waiter.wait_stderr("cacheexec: verbose: join");
    waiter
}

fn with_ttl<'a>(options: &[&'a str]) -> Vec<&'a str> {
    let mut all = options.to_vec();
    if !all.contains(&"--ttl") {
        all.extend(["--ttl", "1h"]);
    }
    all
}

/// Waits until process `pid` has exited and been reaped.
fn wait_gone(s: &Sandbox, pid: i32) {
    s.wait_until(&format!("process {pid} ending"), || unsafe {
        libc::kill(pid, 0) != 0
    });
}

fn clear(s: &Sandbox) -> String {
    let output = s.clear(&[]);
    assert_eq!(code(&output), Some(0), "{}", text(&output.stderr));
    text(&output.stdout)
}

/// Holds the key's lock file, as a caller between deciding and publishing does.
struct Held(fs::File);

impl Held {
    fn lock(s: &Sandbox) -> Self {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(s.cache_file("lock"))
            .unwrap();
        s.wait_until("the key's lock becoming free", || unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
        });
        Self(file)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        libc::SIGKILL => "SIGKILL",
        _ => "signal",
    }
}

#[test]
fn shared_output_is_private_even_with_permissive_umask() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let mut command = s.cacheexec(&["--ttl", "1h"], &SCRIPT);
    unsafe {
        command.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    let owner = s.spawn("owner", command);
    s.wait_event("started");
    let mode = fs::metadata(s.cache_file("active"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    go.release();
    assert_eq!(code(&owner.finish()), Some(7));
    assert_eq!(mode, 0o600);
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    assert_eq!(s.count(), "x");
}

#[test]
fn mixed_policies_refresh_and_ttl_share_and_any_participant_can_save() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "0"], &SCRIPT);
    let refresh = waiter(
        &s,
        "refresh",
        &["--refresh", "--include-codes", "7"],
        &SCRIPT,
        7,
    );
    let expired = waiter(
        &s,
        "zero ttl",
        &["--ttl", "0s", "--include-codes", "2"],
        &SCRIPT,
        2,
    );
    go.release();
    for (name, output) in [
        ("owner", leader.finish()),
        ("refresh", refresh.finish()),
        ("zero ttl", expired.finish()),
    ] {
        assert_eq!(code(&output), Some(7), "{name}");
        assert_eq!(output.stdout, b"\xff\0out", "{name}");
        assert_eq!(child_stderr(&output.stderr), b"\xfeerr", "{name}");
    }
    // Only the refresh allowed 7, and that was enough to save it.
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    assert_eq!(s.count(), "x");
}

#[test]
fn excluded_results_are_shared_and_next_generation_does_not_destroy_waiter_output() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "0"], &SCRIPT);
    let excluded = verbose_waiter(&s, "excluded", &["--exclude-codes", "0,7"], &SCRIPT);
    // Keep the waiter from reading its generation until a new one exists.
    excluded.signal(libc::SIGSTOP);
    go.release();
    let first = leader.finish();
    let next = s.run(&["--ttl", "1h", "--include-codes", "0"], &SCRIPT);
    assert_eq!(code(&next), Some(7));
    assert_eq!(s.count(), "xx");
    excluded.signal(libc::SIGCONT);
    let previous = excluded.finish();
    assert_eq!(code(&previous), Some(7));
    assert_eq!(previous.stdout, first.stdout);
    assert_eq!(child_stderr(&previous.stderr), first.stderr);
}

#[test]
fn separate_keys_progress_independently_and_refresh_hides_previous_result() {
    let s = Sandbox::new();
    s.gate("go").release();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    let mut go = s.gate("go");
    let leader = owner(&s, &["--refresh", "--include-codes", "0"], &SCRIPT);
    let separate = s.run(&["--ttl", "1h", "--key", "separate"], &["exit:0"]);
    assert_eq!(code(&separate), Some(0), "another key waited for this one");
    // It would reuse the saved 7, but the refresh in progress takes precedence.
    let caller = waiter(&s, "caller", &["--include-codes", "7"], &SCRIPT, 7);
    go.release();
    assert_eq!(code(&leader.finish()), Some(7));
    assert_eq!(code(&caller.finish()), Some(7));
    assert_eq!(s.count(), "xx");
}

#[test]
fn owner_interrupt_propagates_and_is_never_saved_even_if_child_traps_it() {
    // The child handles the signal and exits 0, which must not be saved either.
    let steps = [
        "trap:INT,TERM",
        "count",
        "event:started",
        "wait:go",
        "exit:0",
    ];
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let name = signal_name(signal);
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let leader = owner(&s, &["--include-codes", "0"], &steps);
        let joined = waiter(&s, "waiter", &["--include-codes", "1"], &steps, 1);
        leader.signal(signal);
        assert_eq!(
            code(&leader.finish()),
            Some(128 + signal),
            "owner after {name}"
        );
        assert_eq!(
            code(&joined.finish()),
            Some(128 + signal),
            "waiter after {name}"
        );
        go.release();
        assert_eq!(code(&s.run(&["--ttl", "1h"], &steps)), Some(0));
        assert_eq!(
            s.count(),
            "xx",
            "the interrupted result was saved after {name}"
        );
    }
}

#[test]
fn waiter_interrupt_does_not_stop_owner() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let name = signal_name(signal);
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let leader = owner(&s, &["--include-codes", "0"], &SCRIPT);
        let joined = waiter(&s, "waiter", &["--include-codes", "1"], &SCRIPT, 1);
        joined.signal(signal);
        assert_eq!(
            code(&joined.finish()),
            Some(128 + signal),
            "waiter after {name}"
        );
        go.release();
        assert_eq!(
            code(&leader.finish()),
            Some(7),
            "owner after waiter's {name}"
        );
        assert_eq!(s.count(), "x");
    }
}

#[test]
fn sudden_owner_death_fails_waiters_without_retry_and_new_call_can_run() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "0"], &SCRIPT);
    let joined = waiter(&s, "waiter", &["--include-codes", "1"], &SCRIPT, 1);
    leader.signal(libc::SIGKILL);
    let failed = joined.finish();
    assert_eq!(code(&failed), Some(125));
    assert!(
        text(&failed.stderr).contains("owner disappeared"),
        "{}",
        text(&failed.stderr)
    );
    assert_eq!(s.count(), "x", "the waiter retried");
    // The orphaned child is still waiting; let it end.
    go.release();
    leader.finish();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    assert_eq!(s.count(), "xx");
}

#[test]
fn save_failure_is_shared_without_retry() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "7"], &SCRIPT);
    let joined = waiter(&s, "waiter", &["--include-codes", "1"], &SCRIPT, 1);
    fs::create_dir(s.cache_file("active").with_extension("result")).unwrap();
    go.release();
    for (name, output) in [("owner", leader.finish()), ("waiter", joined.finish())] {
        assert_eq!(code(&output), Some(125), "{name}");
        assert!(
            text(&output.stderr).contains("child already completed with exit code 7"),
            "{name}: {}",
            text(&output.stderr)
        );
    }
    assert_eq!(s.count(), "x");
}

#[test]
fn owner_streams_and_many_late_waiters_receive_complete_bytes() {
    let steps = [
        "count",
        "out:start",
        "event:started",
        "wait:go",
        "out:end",
        "err:err",
        "exit:7",
    ];
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "0"], &steps);
    leader.wait_stdout("start");
    // Odd policies join quietly, so replay without --verbose is covered too.
    let waiters: Vec<_> = (1..7)
        .map(|policy: usize| {
            let name = format!("waiter {policy}");
            let options =
                ["--ttl", "1h", "--include-codes", &policy.to_string()].map(str::to_owned);
            let options: Vec<&str> = options.iter().map(String::as_str).collect();
            if policy % 2 == 1 {
                (waiter(&s, &name, &options, &steps, policy), false)
            } else {
                (verbose_waiter(&s, &name, &options, &steps), true)
            }
        })
        .collect();
    go.release();
    assert_eq!(leader.finish().stdout, b"startend");
    for (index, (joined, verbose)) in waiters.into_iter().enumerate() {
        let output = joined.finish();
        let stderr = if verbose {
            child_stderr(&output.stderr)
        } else {
            output.stderr.clone()
        };
        assert_eq!(output.stdout, b"startend", "waiter {}", index + 1);
        assert_eq!(stderr, b"err", "waiter {}", index + 1);
        assert_eq!(code(&output), Some(7), "waiter {}", index + 1);
    }
    assert_eq!(s.count(), "x");
}

#[test]
fn lock_storage_fault_is_an_error_without_execution() {
    let s = Sandbox::new();
    s.gate("go").release();
    s.run(&["--ttl", "1h"], &SCRIPT);
    let lock = s.cache_file("lock");
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    let failed = s.run(&["--ttl", "1h", "--refresh"], &SCRIPT);
    assert_eq!(code(&failed), Some(125));
    assert!(
        text(&failed.stderr).contains("open key lock"),
        "{}",
        text(&failed.stderr)
    );
    assert_eq!(s.count(), "x");
}

#[test]
fn one_signal_allows_child_cleanup_to_finish() {
    // On SIGTERM the child waits for `cleanup`, then reports how many signals
    // it received and exits normally.
    let steps = [
        "trap:TERM",
        "count",
        "event:started",
        "wait:go",
        "event:trapped",
        "wait:cleanup",
        "traps:traps",
        "out:cleaned",
        "exit:0",
    ];
    let s = Sandbox::new();
    let _go = s.gate("go");
    let mut cleanup = s.gate("cleanup");
    let mut leader = owner(&s, &[], &steps);
    leader.signal(libc::SIGTERM);
    s.wait_event("trapped");
    // A second forwarded signal would arrive within cacheexec's 10 ms loop;
    // hold the cleanup far longer so that one would be counted below.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(leader.running(), "cacheexec did not wait for the cleanup");
    cleanup.release();
    let output = leader.finish();
    assert_eq!(code(&output), Some(143));
    assert_eq!(output.stdout, b"cleaned");
    assert_eq!(fs::read_to_string(s.path("traps")).unwrap(), "1");
}

#[test]
fn interruption_while_waiting_to_publish_is_shared_and_not_saved() {
    let steps = [
        "count",
        "event:started",
        "wait:go",
        "event:finished",
        "exit:7",
    ];
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let name = signal_name(signal);
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let leader = owner(&s, &["--include-codes", "7"], &steps);
        let joined = waiter(&s, "waiter", &["--include-codes", "1"], &steps, 1);
        // Publishing needs the lock, so the owner waits for it after the child.
        let held = Held::lock(&s);
        go.release();
        let child = s.wait_event("finished");
        // cacheexec reaps its child only after collecting all output, and then
        // waits for the lock to publish; signal it only once it is there.
        wait_gone(&s, child);
        leader.signal(signal);
        drop(held);
        assert_eq!(
            code(&leader.finish()),
            Some(128 + signal),
            "owner after {name}"
        );
        assert_eq!(
            code(&joined.finish()),
            Some(128 + signal),
            "waiter after {name}"
        );
        assert_eq!(code(&s.run(&["--ttl", "1h"], &steps)), Some(7));
        assert_eq!(s.count(), "xx", "saved after {name}");
    }
}

#[test]
fn replay_cancellation_exits_with_unread_stdout_or_stderr() {
    for stderr in [false, true] {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let case = format!(
                "{} to a replay on unread {}",
                signal_name(signal),
                if stderr { "stderr" } else { "stdout" }
            );
            // More than a pipe holds even with large pages, so a replay blocks on
            // a reader that stops.
            let output = if stderr {
                "zeros:0:4194304"
            } else {
                "zeros:4194304:0"
            };
            let steps = ["count", "event:started", "wait:go", output];
            let s = Sandbox::new();
            let mut go = s.gate("go");
            let leader = owner(&s, &["--include-codes", "0"], &steps);
            // A command whose stdout or stderr is a pipe the test reads itself.
            let blocked = |options: &[&str]| {
                let (reader, writer) = common::pipe();
                let mut command = s.cacheexec(&with_ttl(options), &steps);
                if stderr {
                    command.stderr(writer);
                } else {
                    command.stdout(writer);
                }
                (command, reader)
            };
            let (command, reader) = blocked(&["--include-codes", "1"]);
            let joined = quiet_join(&s, "waiter", command, 1);
            go.release();
            assert_eq!(code(&leader.finish()), Some(0));
            // The replay started, then stopped on the unread pipe.
            common::read_byte(&reader);
            joined.signal(signal);
            assert_eq!(code(&joined.finish()), Some(128 + signal), "waiter: {case}");
            let (command, reader) = blocked(&[]);
            let hit = s.spawn("hit", command);
            common::read_byte(&reader);
            hit.signal(signal);
            assert_eq!(code(&hit.finish()), Some(128 + signal), "hit: {case}");
            assert_eq!(s.count(), "x");
        }
    }
}

#[test]
fn owner_delivery_cancellation_preserves_published_result() {
    for stderr in [false, true] {
        for signal in [0, libc::SIGINT, libc::SIGTERM] {
            let case = format!(
                "{} on unread {}",
                if signal == 0 {
                    "no signal"
                } else {
                    signal_name(signal)
                },
                if stderr { "stderr" } else { "stdout" }
            );
            let output = if stderr {
                "zeros:0:4194304"
            } else {
                "zeros:4194304:0"
            };
            let steps = [
                "count",
                "event:started",
                "wait:go",
                output,
                "event:finished",
            ];
            let s = Sandbox::new();
            let mut go = s.gate("go");
            // The owner's own reader does not read, so its delivery blocks.
            let (reader, writer) = common::pipe();
            let mut command = s.cacheexec(&["--ttl", "1h", "--include-codes", "0"], &steps);
            if stderr {
                command.stderr(writer);
            } else {
                command.stdout(writer);
            }
            let mut leader = s.spawn("owner", command);
            s.wait_event("started");
            let joined = waiter(&s, "waiter", &["--include-codes", "1"], &steps, 1);
            go.release();
            s.wait_event("finished");
            let delivered = |output: &Output| {
                if stderr {
                    child_stderr(&output.stderr).len()
                } else {
                    output.stdout.len()
                }
            };
            // Publication did not wait for the owner's delivery.
            let waited = joined.finish();
            assert_eq!(code(&waited), Some(0), "waiter: {case}");
            assert_eq!(delivered(&waited), 4194304, "waiter: {case}");
            assert!(
                leader.running(),
                "the owner's delivery did not block: {case}"
            );
            if signal == 0 {
                let drained = common::drain(reader);
                assert_eq!(code(&leader.finish()), Some(0), "owner: {case}");
                assert_eq!(drained.bytes().len(), 4194304, "owner: {case}");
            } else {
                leader.signal(signal);
                assert_eq!(code(&leader.finish()), Some(128 + signal), "owner: {case}");
            }
            let hit = s.run(&["--ttl", "1h"], &steps);
            assert_eq!(code(&hit), Some(0), "hit: {case}");
            assert_eq!(delivered(&hit), 4194304, "hit: {case}");
            assert_eq!(s.count(), "x", "{case}");
        }
    }
}

#[test]
fn interruption_with_storage_error_does_not_block_on_stderr_diagnostic() {
    let steps = [
        "count",
        "event:started",
        "wait:go",
        "zeros:0:4194304",
        "event:finished",
    ];
    let s = Sandbox::new();
    let mut go = s.gate("go");
    // The owner's stderr is never read, so its diagnostic would block.
    let (_reader, writer) = common::pipe();
    let mut command = s.cacheexec(&["--ttl", "1h"], &steps);
    command.stderr(writer);
    let leader = s.spawn("owner", command);
    s.wait_event("started");
    fs::create_dir(s.cache_file("active").with_extension("result")).unwrap();
    go.release();
    s.wait_event("finished");
    leader.signal(libc::SIGTERM);
    assert_eq!(code(&leader.finish()), Some(125));
}

#[test]
fn cleanup_preserves_running_refresh_and_undelivered_waiter_generation() {
    let s = Sandbox::new();
    s.gate("go").release();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    {
        // A caller deciding whether to reuse the result holds the lock.
        let _held = Held::lock(&s);
        assert!(clear(&s).contains("skipped=1"), "cleanup took a held key");
        assert_eq!(
            s.cache_files("result").len(),
            1,
            "cleanup deleted a held result"
        );
    }
    let mut go = s.gate("go");
    let leader = owner(&s, &["--refresh", "--include-codes", "7"], &SCRIPT);
    let joined = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &SCRIPT);
    // Keep the waiter from reading its generation.
    joined.signal(libc::SIGSTOP);
    assert!(
        clear(&s).contains("skipped=1"),
        "cleanup took a running execution"
    );
    go.release();
    let first = leader.finish();
    assert!(clear(&s).contains("removed=1"));
    assert!(
        s.cache_file("lock").exists(),
        "cleanup removed the lock file"
    );
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    joined.signal(libc::SIGCONT);
    let replay = joined.finish();
    assert_eq!(code(&replay), Some(7));
    assert_eq!(replay.stdout, first.stdout);
    assert_eq!(child_stderr(&replay.stderr), first.stderr);
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    assert_eq!(s.count(), "xxx");
}

#[test]
fn cleanup_reclaims_abandoned_marker_without_retrying_waiter() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let leader = owner(&s, &["--include-codes", "0"], &SCRIPT);
    let joined = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &SCRIPT);
    // Keep the waiter from noticing the owner's death before cleanup does.
    joined.signal(libc::SIGSTOP);
    leader.signal(libc::SIGKILL);
    leader.finish();
    assert!(clear(&s).contains("abandoned=1"));
    joined.signal(libc::SIGCONT);
    assert_eq!(code(&joined.finish()), Some(125));
    assert_eq!(s.count(), "x", "the waiter retried");
    go.release();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &SCRIPT)), Some(7));
    assert_eq!(s.count(), "xx");
}

/// Waits until `proc` reported `action` as its decision, before completing.
fn decided(proc: &Proc, action: &str) {
    proc.wait_stderr(&format!("cacheexec: verbose: {action}"));
    let first = verbose_lines(&proc.stderr_so_far())
        .into_iter()
        .next()
        .unwrap();
    assert!(
        first.starts_with(action),
        "first decision {first:?}, expected {action}"
    );
    assert!(!first.contains(" age="), "{first}");
}

#[test]
fn verbose_mixed_participants_report_decisions_early_and_actual_saving() {
    for (owner_verbose, waiter_verbose) in [(true, false), (false, true), (true, true)] {
        for save in [false, true] {
            let case = format!(
                "owner_verbose={owner_verbose} waiter_verbose={waiter_verbose} save={save}"
            );
            let s = Sandbox::new();
            let mut go = s.gate("go");
            let mut options = vec!["--ttl", "1h", "--include-codes", "0"];
            if owner_verbose {
                options.push("--verbose");
            }
            let leader = s.spawn("owner", s.cacheexec(&options, &QUIET));
            s.wait_event("started");
            if owner_verbose {
                decided(&leader, "run reason=missing");
            }
            let mut options = vec![
                "--refresh",
                "--ttl",
                "0s",
                "--include-codes",
                if save { "1,7" } else { "1" },
            ];
            if waiter_verbose {
                options.push("--verbose");
            }
            let command = s.cacheexec(&options, &QUIET);
            let joined = if waiter_verbose {
                let joined = s.spawn("waiter", command);
                decided(&joined, "join");
                joined
            } else {
                quiet_join(&s, "waiter", command, 1)
            };
            let excluded = s.spawn(
                "excluded",
                s.cacheexec(
                    &["--ttl", "1h", "--verbose", "--exclude-codes", "0,1,7"],
                    &QUIET,
                ),
            );
            decided(&excluded, "join");
            let generation = fs::File::open(s.cache_file("active")).unwrap();
            go.release();
            let expected = if save {
                "completed exit=7 saved=yes"
            } else {
                "completed exit=7 saved=no reason=participant-policy"
            };
            for (name, output, verbose) in [
                ("owner", leader.finish(), owner_verbose),
                ("waiter", joined.finish(), waiter_verbose),
                ("excluded", excluded.finish(), true),
            ] {
                assert_eq!(code(&output), Some(7), "{name}: {case}");
                if verbose {
                    assert!(
                        text(&output.stderr).contains(expected),
                        "{name}: {case}: {}",
                        text(&output.stderr)
                    );
                } else {
                    assert!(
                        output.stderr.is_empty(),
                        "{name}: {case}: {}",
                        text(&output.stderr)
                    );
                }
            }
            use std::io::Read;
            let mut bytes = Vec::new();
            (&generation).read_to_end(&mut bytes).unwrap();
            assert!(
                common::find(&bytes, b"cacheexec: verbose:").is_none(),
                "diagnostics were stored in the shared generation: {case}"
            );
            assert_eq!(s.count(), "x", "{case}");
            let next = s.run(&["--ttl", "1h", "--verbose"], &QUIET);
            let reused = if save {
                "hit age="
            } else {
                "run reason=missing"
            };
            assert!(
                text(&next.stderr).contains(reused),
                "{case}: {}",
                text(&next.stderr)
            );
            assert_eq!(s.count(), if save { "x" } else { "xx" }, "{case}");
        }
    }
}

#[test]
fn verbose_late_waiter_keeps_its_generation_saving_status() {
    for first_saved in [false, true] {
        let case = format!("first_saved={first_saved}");
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let first_policy = if first_saved { "7" } else { "0" };
        let leader = owner(&s, &["--verbose", "--include-codes", first_policy], &QUIET);
        let late = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &QUIET);
        // Keep the waiter from reading its generation until the next one ends.
        late.signal(libc::SIGSTOP);
        go.release();
        let first = leader.finish();
        let next_policy = if first_saved { "0" } else { "7" };
        let next = s.run(
            &[
                "--ttl",
                "1h",
                "--verbose",
                "--refresh",
                "--include-codes",
                next_policy,
            ],
            &QUIET,
        );
        late.signal(libc::SIGCONT);
        let late = late.finish();
        let saved = if first_saved {
            "saved=yes"
        } else {
            "saved=no reason=participant-policy"
        };
        let next_saved = if first_saved {
            "saved=no reason=participant-policy"
        } else {
            "saved=yes"
        };
        assert!(
            text(&first.stderr).contains(saved),
            "owner: {case}: {}",
            text(&first.stderr)
        );
        assert!(
            text(&late.stderr).contains(saved),
            "late waiter: {case}: {}",
            text(&late.stderr)
        );
        assert!(
            text(&next.stderr).contains(next_saved),
            "next: {case}: {}",
            text(&next.stderr)
        );
        assert_eq!(code(&late), Some(7), "{case}");
        assert_eq!(s.count(), "xx", "{case}");
    }
}

#[test]
fn verbose_owner_and_waiter_interruptions_preserve_uncertainty() {
    let steps = [
        "trap:INT,TERM",
        "count",
        "event:started",
        "wait:go",
        "exit:7",
    ];
    for signal in [libc::SIGINT, libc::SIGTERM] {
        for interrupt_owner in [false, true] {
            let case = format!(
                "{} to the {}",
                signal_name(signal),
                if interrupt_owner { "owner" } else { "waiter" }
            );
            let s = Sandbox::new();
            let mut go = s.gate("go");
            let leader = owner(&s, &["--verbose", "--include-codes", "7"], &steps);
            let joined = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &steps);
            if interrupt_owner {
                leader.signal(signal);
            } else {
                joined.signal(signal);
            }
            let waited = joined.finish();
            assert_eq!(code(&waited), Some(128 + signal), "waiter: {case}");
            let waited = text(&waited.stderr);
            assert!(
                waited.contains(&format!("interrupted exit={}", 128 + signal)),
                "waiter: {case}: {waited}"
            );
            let saving = if interrupt_owner {
                "saved=no reason=interrupted"
            } else {
                "saved=unknown reason=waiter-interrupted"
            };
            assert!(waited.contains(saving), "waiter: {case}: {waited}");
            go.release();
            let owned = leader.finish();
            assert_eq!(
                code(&owned),
                Some(if interrupt_owner { 128 + signal } else { 7 }),
                "owner: {case}"
            );
            let saving = if interrupt_owner {
                "saved=no reason=interrupted"
            } else {
                "saved=yes"
            };
            assert!(
                text(&owned.stderr).contains(saving),
                "owner: {case}: {}",
                text(&owned.stderr)
            );
            assert_eq!(s.count(), "x", "{case}");
        }
    }
}

#[test]
fn verbose_owner_death_and_save_failure_never_report_success() {
    for death in [false, true] {
        let case = if death { "owner death" } else { "save failure" };
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let leader = owner(&s, &["--verbose", "--include-codes", "7"], &QUIET);
        let joined = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &QUIET);
        if death {
            leader.signal(libc::SIGKILL);
        } else {
            fs::create_dir(s.cache_file("active").with_extension("result")).unwrap();
        }
        go.release();
        let owned = leader.finish();
        let waited = joined.finish();
        assert_eq!(code(&waited), Some(125), "{case}");
        let outputs = if death {
            vec![waited]
        } else {
            vec![owned, waited]
        };
        for output in outputs {
            let text = text(&output.stderr);
            assert!(
                text.contains("failed saved=unknown reason=failure"),
                "{case}: {text}"
            );
            assert!(
                !text.contains("saved=yes") && !text.contains("completed exit="),
                "{case}: {text}"
            );
        }
        assert_eq!(s.count(), "x", "{case}");
    }
}

#[test]
fn verbose_closed_or_stalled_stderr_cannot_block_execution_saving_or_waiters() {
    for closed in [false, true] {
        for blocked_owner in [false, true] {
            let case = format!(
                "{} stderr for the {}",
                if closed { "closed" } else { "stalled" },
                if blocked_owner { "owner" } else { "waiter" }
            );
            let s = Sandbox::new();
            let mut go = s.gate("go");
            // The stderr every blocked participant shares; the full pipe's
            // reader stays open, but nothing reads it.
            let full = (!closed).then(FullPipe::new);
            let shared = match &full {
                Some(full) => full.blocking(),
                None => common::closed_pipe(),
            };
            let blocked = |policy: &str| {
                let mut command = s.cacheexec(
                    &["--ttl", "1h", "--verbose", "--include-codes", policy],
                    &QUIET,
                );
                command.stderr(shared.try_clone().unwrap());
                command
            };
            let (leader, joined) = if blocked_owner {
                let leader = s.spawn("owner", blocked("7"));
                s.wait_event("started");
                // The waiter's own diagnostics show when it joins.
                let joined = verbose_waiter(&s, "waiter", &["--include-codes", "1"], &QUIET);
                (leader, joined)
            } else {
                let leader = owner(&s, &["--verbose", "--include-codes", "7"], &QUIET);
                let joined = quiet_join(&s, "waiter", blocked("1"), 1);
                (leader, joined)
            };
            go.release();
            assert_eq!(code(&leader.finish()), Some(7), "owner: {case}");
            assert_eq!(code(&joined.finish()), Some(7), "waiter: {case}");
            assert_eq!(
                code(&s.spawn("hit", blocked("7")).finish()),
                Some(7),
                "blocked hit: {case}"
            );
            assert!(
                !common::is_nonblocking(&shared),
                "O_NONBLOCK was left on the shared stderr: {case}"
            );
            let hit = s.run(&["--ttl", "1h"], &QUIET);
            assert_eq!(code(&hit), Some(7), "{case}");
            assert!(hit.stderr.is_empty(), "{case}");
            assert_eq!(s.count(), "x", "{case}");
        }
    }
}

#[test]
fn verbose_file_size_write_failure_does_not_deliver_sigxfsz() {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::process::CommandExt;
    let s = Sandbox::new();
    s.gate("go").release();
    // Diagnostics go to a file already at the size limit.
    let mut log = fs::File::create(s.path("diagnostic")).unwrap();
    log.set_len(4096).unwrap();
    log.seek(SeekFrom::End(0)).unwrap();
    let mut command = s.cacheexec(&["--ttl", "1h", "--verbose"], &QUIET);
    command.stderr(log);
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
    assert_eq!(code(&s.spawn("limited", command).finish()), Some(7));
    assert_eq!(code(&s.run(&["--ttl", "1h"], &QUIET)), Some(7));
    assert_eq!(s.count(), "x");
}

#[test]
fn owner_delivery_failure_does_not_fail_waiters_or_discard_cache() {
    let steps = ["count", "event:started", "wait:go", "out:out", "err:err"];
    for stderr in [false, true] {
        let case = if stderr {
            "closed stderr"
        } else {
            "closed stdout"
        };
        let s = Sandbox::new();
        let mut go = s.gate("go");
        let mut command = s.cacheexec(&["--ttl", "1h", "--include-codes", "0"], &steps);
        if stderr {
            command.stderr(common::closed_pipe());
        } else {
            command.stdout(common::closed_pipe());
        }
        let leader = s.spawn("owner", command);
        s.wait_event("started");
        let joined = waiter(&s, "waiter", &["--include-codes", "1"], &steps, 1);
        go.release();
        assert_eq!(code(&leader.finish()), Some(125), "owner: {case}");
        for (name, output) in [
            ("waiter", joined.finish()),
            ("hit", s.run(&["--ttl", "1h"], &steps)),
        ] {
            assert_eq!(code(&output), Some(0), "{name}: {case}");
            assert_eq!(output.stdout, b"out", "{name}: {case}");
            assert_eq!(child_stderr(&output.stderr), b"err", "{name}: {case}");
        }
        assert_eq!(s.count(), "x", "{case}");
    }
}
