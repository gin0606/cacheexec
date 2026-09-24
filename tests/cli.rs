mod common;

use common::{CHILD, Sandbox, code, text, verbose_lines};
use std::{
    fs,
    io::Write,
    process::Output,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Counts the execution and writes non-UTF-8 bytes to both streams.
const BYTES: [&str; 3] = ["count", "outx:ff006f7574", "errx:fe657272"];

fn with(steps: &[&'static str], more: &[&'static str]) -> Vec<&'static str> {
    steps.iter().chain(more).copied().collect()
}

fn stderr(output: &Output) -> String {
    text(&output.stderr)
}

fn stdout(output: &Output) -> String {
    text(&output.stdout)
}

/// The completion time stored in a result (nanoseconds since the epoch).
fn stored_completion(result: &[u8]) -> u128 {
    u128::from_le_bytes(result[8..24].try_into().unwrap())
}

/// Rewrites the completion time stored in a result, keeping its checksum valid.
fn set_completion(result: &std::path::Path, completed: SystemTime) {
    use sha2::{Digest, Sha256};
    let mut bytes = fs::read(result).unwrap();
    let nanos = completed.duration_since(UNIX_EPOCH).unwrap().as_nanos();
    bytes[8..24].copy_from_slice(&nanos.to_le_bytes());
    let end = bytes.len() - 32;
    let checksum = Sha256::digest(&bytes[..end]);
    bytes[end..].copy_from_slice(&checksum);
    fs::write(result, &bytes).unwrap();
}

#[test]
fn missing_separator_suggests_command_syntax_without_masking_option_errors() {
    let s = Sandbox::new();
    let mut command = s.command();
    command.args(["--ttl", "5m", "echo", "hello"]);
    let output = s.spawn("missing separator", command).finish();
    assert_eq!(code(&output), Some(2));
    assert!(
        stderr(&output).contains("put -- before the command"),
        "{}",
        stderr(&output)
    );
    let mut command = s.command();
    command.args(["--ttl", "5m", "--refersh"]);
    let output = s.spawn("misspelled option", command).finish();
    assert_eq!(code(&output), Some(2));
    assert!(stderr(&output).contains("--refersh"), "{}", stderr(&output));
    assert!(!stderr(&output).contains("put -- before the command"));
}

#[test]
fn hit_replays_binary_streams_and_every_exit_code() {
    for exit in [
        "exit:0", "exit:1", "exit:2", "exit:23", "exit:125", "exit:255",
    ] {
        let s = Sandbox::new();
        let steps = with(&BYTES, &[exit]);
        let first = s.run(&["--ttl", "1h"], &steps);
        let hit = s.run(&["--ttl", "1h"], &steps);
        let expected = exit.strip_prefix("exit:").unwrap().parse().ok();
        for (name, output) in [("first run", &first), ("hit", &hit)] {
            assert_eq!(code(output), expected, "{name} of {exit}");
            assert_eq!(output.stdout, b"\xff\0out", "{name} of {exit}");
            assert_eq!(output.stderr, b"\xfeerr", "{name} of {exit}");
        }
        assert_eq!(s.count(), "x", "{exit} ran again");
    }
}

#[test]
fn ttl_is_chosen_per_call_for_the_same_result() {
    let s = Sandbox::new();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &["count"])), Some(0));
    assert_eq!(code(&s.run(&["--ttl", "1h"], &["count"])), Some(0));
    assert_eq!(s.count(), "x", "a fresh result was not reused");
    // The same key: a result of any age is too old for a zero TTL.
    assert_eq!(code(&s.run(&["--ttl", "0s"], &["count"])), Some(0));
    assert_eq!(s.count(), "xx", "a zero TTL reused the result");
    assert_eq!(code(&s.run(&["--ttl", "1h"], &["count"])), Some(0));
    assert_eq!(s.count(), "xx", "the refreshed result was not reused");
}

#[test]
fn a_result_expires_once_its_age_exceeds_the_ttl() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &["count"]);
    let result = s.cache_file("result");
    let ago = |age: Duration| SystemTime::now() - age;
    let cases = [
        (Duration::from_secs(30 * 60), "1h", true),
        (Duration::from_secs(2 * 3600), "1h", false),
        (Duration::from_secs(10), "20s", true),
        (Duration::from_secs(10), "500ms", false),
    ];
    let mut runs = 1;
    for (age, ttl, reused) in cases {
        set_completion(&s.cache_file("result"), ago(age));
        assert_eq!(code(&s.run(&["--ttl", ttl], &["count"])), Some(0));
        if !reused {
            runs += 1;
        }
        assert_eq!(
            s.count(),
            "x".repeat(runs),
            "a result {age:?} old with --ttl {ttl}"
        );
    }
    assert!(result.exists());
}

#[test]
fn completion_time_is_when_the_child_exits() {
    let s = Sandbox::new();
    let mut go = s.gate("go");
    let mut hold = s.gate("hold");
    // After it is released, the child exits while a descendant keeps its
    // output open until `hold` is released.
    let steps = [
        "count",
        "event:started",
        "wait:go",
        "detach:wait-stdin,event:orphaned,wait:hold",
        "exit:0",
    ];
    let run = s.spawn("run", s.cacheexec(&["--ttl", "1h"], &steps));
    s.wait_event("started");
    let released = SystemTime::now();
    go.release();
    // The descendant reports once its parent, the child, has exited.
    s.wait_event("orphaned");
    // cacheexec notices the exit by polling every 10 ms, so keep the output
    // open far beyond that, even on a loaded host, before closing it.
    std::thread::sleep(Duration::from_secs(1));
    let closing = SystemTime::now();
    hold.release();
    assert_eq!(code(&run.finish()), Some(0));
    let completed = stored_completion(&fs::read(s.cache_file("result")).unwrap());
    let nanos = |time: SystemTime| time.duration_since(UNIX_EPOCH).unwrap().as_nanos();
    assert!(
        nanos(released) < completed && completed < nanos(closing),
        "completion {completed} is not after the child was released ({}) and before \
         its output closed ({})",
        nanos(released),
        nanos(closing)
    );
}

#[test]
fn refresh_and_policy_invalidate_old_results() {
    let s = Sandbox::new();
    let steps = ["count", "exitenv:CODE"];
    let run = |options: &[&str], exit: &str| {
        let mut command = s.cacheexec(options, &steps);
        command.env("CODE", exit);
        s.spawn("run", command).finish()
    };
    assert_eq!(code(&run(&["--ttl", "1h"], "0")), Some(0));
    let refreshed = run(&["--ttl", "1h", "--refresh", "--include-codes", "0,1"], "7");
    assert_eq!(code(&refreshed), Some(7));
    // The refresh invalidated 0 and did not save 7, so this runs again.
    assert_eq!(code(&run(&["--ttl", "1h"], "0")), Some(0));
    assert_eq!(s.count(), "xxx");
    // The saved 0 is excluded by this call's policy, so it runs again.
    assert_eq!(
        code(&run(&["--ttl", "1h", "--exclude-codes", "0"], "0")),
        Some(0)
    );
    assert_eq!(s.count(), "xxxx");
    // That run's 0 was not saved either, since its policy excluded it.
    assert_eq!(code(&run(&["--ttl", "1h"], "0")), Some(0));
    assert_eq!(s.count(), "xxxxx");
    let conflicting = run(
        &[
            "--ttl",
            "1h",
            "--include-codes",
            "0",
            "--exclude-codes",
            "1",
        ],
        "0",
    );
    assert_eq!(code(&conflicting), Some(2));
    for invalid in ["256", "-1", "x"] {
        let option = format!("--include-codes={invalid}");
        let output = run(&["--ttl", "1h", &option], "0");
        assert_eq!(code(&output), Some(2), "--include-codes {invalid}");
    }
    assert_eq!(s.count(), "xxxxx");
}

#[test]
fn keys_preserve_argv_cwd_and_extra_key_not_environment() {
    let s = Sandbox::new();
    let run = |key: &str, args: &[&str]| {
        let mut command = s.command();
        command
            .args(["--ttl", "1h", "--key", key, "--", CHILD, "count", "args"])
            .args(args);
        s.spawn("run", command).finish()
    };
    assert_eq!(run("one", &["a b", "c"]).stdout, b"a b|c|");
    assert_eq!(
        run("one", &["a", "b c"]).stdout,
        b"a|b c|",
        "argument boundaries"
    );
    assert_eq!(s.count(), "xx");
    run("two", &["a", "b c"]);
    run("two", &["a", "b c"]);
    assert_eq!(s.count(), "xxx", "--key");
    let sub = s.path("sub");
    fs::create_dir(&sub).unwrap();
    let mut command = s.command();
    command.current_dir(&sub).args([
        "--ttl", "1h", "--key", "two", "--", CHILD, "count", "args", "a", "b c",
    ]);
    assert_eq!(code(&s.spawn("in sub", command).finish()), Some(0));
    assert_eq!(
        fs::read_to_string(sub.join("count")).unwrap(),
        "x",
        "working directory"
    );
    let environment = |value: &str| {
        let mut command = s.cacheexec(&["--ttl", "1h"], &["env:VALUE"]);
        command.env("VALUE", value);
        s.spawn("environment", command).finish()
    };
    assert_eq!(
        environment("one").stdout,
        b"one",
        "the child inherits the environment"
    );
    assert_eq!(
        environment("two").stdout,
        b"one",
        "the environment is not keyed"
    );
}

#[test]
fn drains_large_outputs_and_closes_stdin() {
    let s = Sandbox::new();
    // Both streams exceed a pipe buffer and are written concurrently.
    let steps = ["stdin-empty:9", "zeros:2097152:2097152"];
    // cacheexec's own stdin has data, which the child must not see.
    let (stdin, input) = common::pipe();
    fs::File::from(input).write_all(b"input").unwrap();
    let mut command = s.cacheexec(&["--ttl", "1h"], &steps);
    command.stdin(stdin);
    let first = s.spawn("run", command).finish();
    assert_eq!(code(&first), Some(0), "the child's stdin was not empty");
    assert_eq!(first.stdout.len(), 2_097_152);
    assert_eq!(first.stderr.len(), 2_097_152);
    let hit = s.run(&["--ttl", "1h"], &steps);
    assert!(hit.stdout == first.stdout && hit.stderr == first.stderr);
}

#[test]
fn failures_do_not_restore_old_results_or_repeat_child() {
    let s = Sandbox::new();
    let steps = ["count", "event:started", "wait:go", "exit:7"];
    let mut go = s.gate("go");
    go.release();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &steps)), Some(7));
    let mut go = s.gate("go");
    let refresh = s.spawn(
        "refresh",
        s.cacheexec(&["--ttl", "1h", "--refresh"], &steps),
    );
    s.wait_event("started");
    // Saving fails: the cache directory is replaced while the child runs.
    fs::remove_dir_all(s.cache()).unwrap();
    fs::write(s.cache(), "blocked").unwrap();
    go.release();
    let failed = refresh.finish();
    assert_eq!(code(&failed), Some(125));
    assert!(
        stderr(&failed).contains("child already completed with exit code 7"),
        "{}",
        stderr(&failed)
    );
    assert_eq!(s.count(), "xx", "the child was repeated");
    fs::remove_file(s.cache()).unwrap();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &steps)), Some(7));
    assert_eq!(s.count(), "xxx", "an old result was restored");
}

#[test]
fn corruption_is_an_error_and_never_executes_child() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &BYTES);
    let result = s.cache_file("result");
    fs::write(&result, "partial").unwrap();
    for options in [&["--ttl", "1h"][..], &["--ttl", "1h", "--refresh"][..]] {
        let output = s.run(options, &BYTES);
        assert_eq!(code(&output), Some(125), "{options:?}");
        let diagnostic = stderr(&output);
        for expected in [
            "corrupt cached result",
            result.to_str().unwrap(),
            "stop all cacheexec invocations",
            "remove only this .result file",
            "keep .lock and .active files",
        ] {
            assert!(diagnostic.contains(expected), "{options:?}: {diagnostic}");
        }
    }
    let cleared = s.clear(&[]);
    assert_eq!(code(&cleared), Some(125));
    assert!(
        stderr(&cleared).contains(result.to_str().unwrap()),
        "{}",
        stderr(&cleared)
    );
    assert!(
        stderr(&cleared).contains("Recovery:"),
        "{}",
        stderr(&cleared)
    );
    assert_eq!(s.count(), "x");
    fs::remove_file(&result).unwrap();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &BYTES)), Some(0));
    assert_eq!(s.count(), "xx");
}

#[test]
fn other_format_versions_are_misses_and_clearable() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &BYTES);
    let result = s.cache_file("result");
    fs::write(&result, b"CEXEC999 written by another version").unwrap();
    assert_eq!(code(&s.run(&["--ttl", "1h"], &BYTES)), Some(0));
    assert_eq!(s.count(), "xx");
    assert!(fs::read(&result).unwrap().starts_with(b"CEXEC001"));
    assert_eq!(code(&s.run(&["--ttl", "1h"], &BYTES)), Some(0));
    assert_eq!(s.count(), "xx");

    // Without a readable completion time, age-based cleanup uses the mtime.
    fs::write(&result, b"CEXEC999").unwrap();
    let clear = |options: &[&str]| {
        let output = s.clear(options);
        assert_eq!(code(&output), Some(0), "{}", stderr(&output));
        stdout(&output)
    };
    assert!(clear(&["--older-than", "24h"]).starts_with("removed=0 "));
    fs::File::options()
        .write(true)
        .open(&result)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800))
        .unwrap();
    assert!(clear(&["--older-than", "24h"]).starts_with("removed=1 "));
    fs::write(&result, b"CEXEC999").unwrap();
    assert!(clear(&[]).starts_with("removed=1 "));
}

#[test]
fn spawn_failure_and_signals_are_not_cached() {
    let s = Sandbox::new();
    let program = s.path("program");
    std::os::unix::fs::symlink(CHILD, &program).unwrap();
    let run = |refresh: bool| {
        let mut command = s.command();
        command.args(["--ttl", "1h"]);
        if refresh {
            command.arg("--refresh");
        }
        command.arg("--").arg(&program).arg("count");
        s.spawn("run", command).finish()
    };
    let first = run(false);
    assert_eq!(code(&first), Some(0), "{}", stderr(&first));
    fs::remove_file(&program).unwrap();
    for refresh in [true, false] {
        let output = run(refresh);
        assert_eq!(code(&output), Some(125), "refresh={refresh}");
        assert_eq!(
            stderr(&output).matches("could not start").count(),
            1,
            "{}",
            stderr(&output)
        );
    }
    let killed = ["count", "raise:TERM"];
    assert_eq!(code(&s.run(&["--ttl", "1h"], &killed)), Some(143));
    assert_eq!(code(&s.run(&["--ttl", "1h"], &killed)), Some(143));
    assert_eq!(s.count(), "xxx", "a signal termination was cached");
}

#[test]
fn output_transfer_failure_is_reported_once() {
    let s = Sandbox::new();
    let full = common::FullPipe::new();
    let mut command = s.cacheexec(&["--ttl", "1h"], &["out:output"]);
    command.stdout(full.nonblocking());
    let output = s.spawn("full stdout", command).finish();
    assert_eq!(code(&output), Some(125));
    let diagnostic = stderr(&output);
    assert_eq!(
        diagnostic.matches("output transfer failed").count(),
        1,
        "{diagnostic}"
    );
    assert_eq!(
        diagnostic.matches("child already completed").count(),
        1,
        "{diagnostic}"
    );
}

/// Runs `cacheexec OPTIONS -- STEPS` with stdout and stderr replaced as given.
fn run_to(
    s: &Sandbox,
    options: &[&str],
    steps: &[&str],
    stdout: Option<std::os::fd::OwnedFd>,
    stderr: Option<std::os::fd::OwnedFd>,
) -> Output {
    let mut command = s.cacheexec(options, steps);
    if let Some(stdout) = stdout {
        command.stdout(stdout);
    }
    if let Some(stderr) = stderr {
        command.stderr(stderr);
    }
    s.spawn("run", command).finish()
}

#[test]
fn closed_output_exits_quietly_with_the_command_status_and_keeps_result() {
    let s = Sandbox::new();
    // A failing command, so its status is visibly kept rather than replaced.
    let steps = ["count", "out:output", "err:warning", "exit:3"];
    let options = ["--ttl", "1h", "--include-codes", "3"];
    // The first call runs and saves; the second replays the saved result.
    for saving in ["saved=yes", "saved=no reason=reused"] {
        let verbose = with(&options, &["--verbose"]);
        let output = run_to(&s, &verbose, &steps, Some(common::closed_pipe()), None);
        assert_eq!(code(&output), Some(3), "{saving}");
        assert_eq!(s.count(), "x", "{saving}");
        // Stderr is still delivered in full, and no tool diagnostic is added.
        assert_eq!(common::child_stderr(&output.stderr), b"warning", "{saving}");
        assert_eq!(
            verbose_lines(&output.stderr).last().map(String::as_str),
            Some(format!("output-closed exit=3 {saving}").as_str()),
            "{}",
            stderr(&output)
        );
    }
    let replayed = s.run(&options, &steps);
    assert_eq!(code(&replayed), Some(3));
    assert_eq!(replayed.stdout, b"output");
    assert_eq!(replayed.stderr, b"warning");
    assert_eq!(s.count(), "x");
}

#[test]
fn closed_stderr_still_delivers_all_stdout() {
    let s = Sandbox::new();
    // More than a pipe holds, so a stalled stdout would block the child.
    let steps = ["count", "zeros:4194304:0", "err:warning"];
    // The first call runs and saves; the second replays the saved result.
    for call in ["run", "replay"] {
        let output = run_to(
            &s,
            &["--ttl", "1h"],
            &steps,
            None,
            Some(common::closed_pipe()),
        );
        assert_eq!(code(&output), Some(0), "{call}");
        assert_eq!(output.stdout.len(), 4194304, "{call}");
        assert_eq!(s.count(), "x", "{call}");
    }
}

#[test]
fn closed_output_keeps_the_exit_of_a_command_killed_by_a_signal() {
    let s = Sandbox::new();
    let steps = ["count", "out:output", "raise:TERM"];
    for expected in ["x", "xx"] {
        let output = run_to(
            &s,
            &["--ttl", "1h", "--verbose"],
            &steps,
            Some(common::closed_pipe()),
            None,
        );
        assert_eq!(code(&output), Some(128 + libc::SIGTERM));
        assert!(
            stderr(&output)
                .contains("cacheexec: verbose: interrupted exit=143 saved=no reason=interrupted\n"),
            "{}",
            stderr(&output)
        );
        assert!(
            !stderr(&output).contains("cacheexec: child"),
            "{}",
            stderr(&output)
        );
        // Signal terminations are never saved, so the command runs again.
        assert_eq!(s.count(), expected);
    }
}

#[test]
fn closed_output_does_not_hide_other_stream_failures() {
    let s = Sandbox::new();
    let steps = ["count", "out:output", "err:warning"];
    assert_eq!(code(&s.run(&["--ttl", "1h"], &steps)), Some(0));
    for refresh in [true, false] {
        for closed_stdout in [true, false] {
            let case = format!("refresh={refresh} closed_stdout={closed_stdout}");
            let full = common::FullPipe::new();
            let (stdout, stderr) = if closed_stdout {
                (common::closed_pipe(), full.nonblocking())
            } else {
                (full.nonblocking(), common::closed_pipe())
            };
            let options: &[&str] = if refresh {
                &["--ttl", "1h", "--refresh"]
            } else {
                &["--ttl", "1h"]
            };
            let output = run_to(&s, options, &steps, Some(stdout), Some(stderr));
            assert_eq!(code(&output), Some(125), "{case}");
        }
    }
    assert_eq!(s.count(), "xxx");
}

#[test]
fn storage_access_errors_are_diagnosed() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &BYTES);
    let result = s.cache_file("result");
    fs::remove_file(&result).unwrap();
    fs::create_dir(&result).unwrap();
    let output = s.run(&["--ttl", "1h"], &BYTES);
    assert_eq!(code(&output), Some(125));
    assert!(
        stderr(&output).contains("read cached result"),
        "{}",
        stderr(&output)
    );
    assert_eq!(s.count(), "x");
}

#[test]
fn default_cache_directory_follows_xdg_then_home() {
    let s = Sandbox::new();
    let run = |xdg: Option<&str>, home: Option<&std::path::Path>| {
        let mut command = s.bare_command();
        command
            .env_remove("XDG_CACHE_HOME")
            .env_remove("HOME")
            .args(["--ttl", "1h", "--", CHILD, "exit:0"]);
        if let Some(xdg) = xdg {
            command.env("XDG_CACHE_HOME", xdg);
        }
        if let Some(home) = home {
            command.env("HOME", home);
        }
        s.spawn("default directory", command).finish()
    };
    let xdg = s.path("xdg");
    let home = s.path("home");
    assert_eq!(
        code(&run(Some(xdg.to_str().unwrap()), Some(&home))),
        Some(0)
    );
    assert!(
        xdg.join("cacheexec").is_dir(),
        "XDG_CACHE_HOME was not used"
    );
    assert!(!home.exists());
    assert_eq!(code(&run(None, Some(&home))), Some(0));
    assert!(home.join(".cache/cacheexec").is_dir(), "HOME was not used");
    fs::remove_dir_all(&home).unwrap();
    assert_eq!(code(&run(Some(""), Some(&home))), Some(0));
    assert!(
        home.join(".cache/cacheexec").is_dir(),
        "an empty XDG_CACHE_HOME was used"
    );
    let output = run(None, None);
    assert_eq!(code(&output), Some(125));
    assert!(
        stderr(&output).contains("supply --cache-dir"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn required_arguments_are_argument_errors() {
    let s = Sandbox::new();
    for args in [
        &["--", CHILD][..],
        &["--ttl", "invalid", "--", CHILD],
        &["--ttl", "1h", CHILD],
    ] {
        let mut command = s.command();
        command.args(args);
        assert_eq!(
            code(&s.spawn("arguments", command).finish()),
            Some(2),
            "{args:?}"
        );
    }
}

#[test]
fn clear_all_age_directory_isolation_and_condition_fixture() {
    for exit in ["0", "1", "7"] {
        let s = Sandbox::new();
        let run = || {
            let mut command = s.cacheexec(
                &["--ttl", "5m", "--include-codes", "0,1"],
                &with(&BYTES, &["exitenv:CODE"]),
            );
            command.env("CODE", exit);
            s.spawn("run", command).finish()
        };
        let clear = |options: &[&str]| {
            let output = s.clear(options);
            assert_eq!(code(&output), Some(0), "{}", stderr(&output));
            stdout(&output)
        };
        let saved = exit != "7";
        assert_eq!(code(&run()), exit.parse().ok());
        assert_eq!(code(&run()), exit.parse().ok());
        assert_eq!(s.count(), if saved { "x" } else { "xx" }, "exit {exit}");
        assert!(
            clear(&["--older-than", "1h"]).contains("removed=0"),
            "exit {exit}"
        );
        let mut other = s.bare_command();
        other.arg("--cache-dir").arg(s.path("other")).arg("--clear");
        assert_eq!(code(&s.spawn("clear other", other).finish()), Some(0));
        assert!(
            !s.path("other").exists(),
            "--clear created a missing directory"
        );
        let removed = if saved { "removed=1" } else { "removed=0" };
        assert!(clear(&[]).contains(removed), "exit {exit}");
        assert_eq!(code(&run()), exit.parse().ok());
        assert_eq!(s.count(), if saved { "xx" } else { "xxx" }, "exit {exit}");
        clear(&["--older-than", "0s"]);
        assert!(s.cache_files("result").is_empty(), "exit {exit}");
    }
}

#[test]
fn clear_removes_lock_files_of_idle_keys_only() {
    let s = Sandbox::new();
    let kinds = || {
        let mut kinds: Vec<_> = ["lock", "result", "active"]
            .into_iter()
            .filter(|kind| !s.cache_files(kind).is_empty())
            .collect();
        kinds.sort_unstable();
        kinds
    };
    let clear = |options: &[&str]| {
        let output = s.clear(options);
        assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    };
    s.run(&["--ttl", "1h"], &["exit:0"]);
    clear(&["--older-than", "24h"]);
    assert_eq!(kinds(), ["lock", "result"]);
    clear(&[]);
    assert!(kinds().is_empty());
    // A gate left without a result, as after an uncached exit code.
    s.run(&["--ttl", "1h", "--exclude-codes", "0"], &["exit:0"]);
    assert_eq!(kinds(), ["lock"]);
    clear(&["--older-than", "24h"]);
    assert!(kinds().is_empty());
}

#[test]
fn clear_summary_to_a_closed_reader_still_succeeds() {
    let s = Sandbox::new();
    let clear = |stdout: std::os::fd::OwnedFd| {
        let mut command = s.command();
        command.arg("--clear").stdout(stdout);
        s.spawn("clear", command).finish()
    };
    s.run(&["--ttl", "1h"], &["exit:0"]);
    let output = clear(common::closed_pipe());
    assert_eq!(code(&output), Some(0));
    assert_eq!(stderr(&output), "");
    assert!(s.cache_files("result").is_empty());
    s.run(&["--ttl", "1h"], &["exit:0"]);
    let full = common::FullPipe::new();
    let output = clear(full.nonblocking());
    assert_eq!(code(&output), Some(125));
    // The deletion already happened, so the counts must survive the failure.
    assert!(
        stderr(&output)
            .contains("print cleanup summary (removed=1 abandoned=0 skipped=0 failed=0)"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn clear_of_a_missing_directory_handles_summary_output_failures() {
    let s = Sandbox::new();
    let clear = |stdout: std::os::fd::OwnedFd| {
        let mut command = s.command();
        command.arg("--clear").stdout(stdout);
        s.spawn("clear", command).finish()
    };
    let output = clear(common::closed_pipe());
    assert_eq!(code(&output), Some(0));
    assert_eq!(stderr(&output), "");
    let full = common::FullPipe::new();
    let output = clear(full.nonblocking());
    assert_eq!(code(&output), Some(125));
    assert!(
        stderr(&output)
            .contains("print cleanup summary (removed=0 abandoned=0 skipped=0 failed=0)"),
        "{}",
        stderr(&output)
    );
    assert!(!s.cache().exists());
}

#[test]
fn clear_blocked_on_its_summary_stays_killable() {
    use std::os::unix::process::ExitStatusExt;
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &["exit:0"]);
    let result = s.cache_file("result");
    let full = common::FullPipe::new();
    let mut command = s.command();
    command.arg("--clear").stdout(full.blocking());
    let clear = s.spawn("clear", command);
    // Deletions come before the summary, so once the result is gone cleanup
    // blocks, or is about to block, on the full stdout.
    s.wait_until("--clear deleting the result", || !result.exists());
    clear.signal(libc::SIGTERM);
    assert_eq!(clear.finish().status.signal(), Some(libc::SIGTERM));
}

#[test]
fn clear_reports_partial_failure_and_preserves_corruption() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &["exit:0"]);
    let broken = s.cache().join(format!("{}.result", "0".repeat(64)));
    fs::write(&broken, "corrupt").unwrap();
    let clear = |args: &[&str]| {
        let mut command = s.command();
        command.args(args);
        s.spawn("clear", command).finish()
    };
    let output = clear(&["--clear"]);
    assert_eq!(code(&output), Some(125));
    for expected in ["removed=1", "failed=1", "corrupt"] {
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
    }
    assert!(broken.exists(), "a corrupt result was deleted");
    fs::remove_file(&broken).unwrap();
    fs::create_dir(&broken).unwrap();
    assert_eq!(code(&clear(&["--clear"])), Some(125));
    for args in [
        &["--clear", "--ttl", "1h"][..],
        &["--older-than", "1h"],
        &["--clear", "--", CHILD],
    ] {
        assert_eq!(code(&clear(args)), Some(2), "{args:?}");
    }
}

#[test]
fn clear_permission_failure_is_explicit() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"], &["exit:0"]);
    let cache = s.cache();
    let output = {
        let _read_only = Mode::set(&cache, 0o555);
        s.clear(&[])
    };
    if unsafe { libc::geteuid() } == 0 {
        // Root may delete from a read-only directory; nothing to check.
        return;
    }
    assert_eq!(code(&output), Some(125));
    assert!(
        stderr(&output).contains("delete cached result"),
        "{}",
        stderr(&output)
    );
    assert!(s.cache_file("result").exists());
}

#[test]
fn verbose_decisions_reasons_metadata_and_uncached_output() {
    let s = Sandbox::new();
    // `child-error\n` on stderr.
    let steps = [
        "count",
        "out:out",
        "errx:6368696c642d6572726f720a",
        "exit:7",
    ];
    let run = |options: &[&str]| {
        let mut all = vec!["--verbose", "--ttl", "1h"];
        all.extend(options);
        s.run(&all, &steps)
    };
    let first = run(&[]);
    let lines = verbose_lines(&first.stderr);
    let text = stderr(&first);
    assert!(
        lines[0].starts_with("run reason=missing ttl=1h key="),
        "{text}"
    );
    assert!(!text.contains(" age="), "{text}");
    assert!(text.contains("completed exit=7 saved=yes"), "{text}");
    assert!(
        text.contains(&format!("cache-dir={:?}", s.cache())),
        "{text}"
    );
    let result = s.cache_file("result");
    let key = result.file_stem().unwrap().to_str().unwrap().to_owned();
    assert!(text.contains(&format!("key={key}")), "{text}");
    let hit = run(&[]);
    let text = stderr(&hit);
    assert!(text.contains("hit age="), "{text}");
    assert!(text.contains("saved=no reason=reused"), "{text}");
    assert_eq!(hit.stdout, b"out");
    assert_eq!(s.count(), "x");
    let quiet = s.run(&["--ttl", "1h"], &steps);
    assert_eq!(
        quiet.stderr, b"child-error\n",
        "diagnostics without --verbose"
    );
    let bytes = fs::read(&result).unwrap();
    assert!(
        common::find(&bytes, b"cacheexec: verbose:").is_none(),
        "diagnostics were stored in the result"
    );
    assert_eq!(&bytes[..8], b"CEXEC001");
    let age: f64 = text
        .split("age=")
        .nth(1)
        .unwrap()
        .split('s')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let actual_age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - stored_completion(&bytes) as f64 / 1e9;
    assert!(
        age >= 0.0 && age <= actual_age && actual_age - age < 1.0,
        "age={age} actual={actual_age}"
    );
    let policy = run(&["--include-codes", "0"]);
    assert!(
        stderr(&policy).contains("run reason=policy age="),
        "{}",
        stderr(&policy)
    );
    assert!(stderr(&policy).contains("completed exit=7 saved=no reason=participant-policy"));
    assert_eq!(s.count(), "xx");
    assert!(stderr(&run(&[])).contains("run reason=missing"));
    let refreshed = run(&["--refresh", "--include-codes", "0"]);
    assert!(
        stderr(&refreshed).contains("run reason=refresh age="),
        "{}",
        stderr(&refreshed)
    );
    run(&[]);
    let expired = s.run(
        &["--verbose", "--ttl", "0s", "--include-codes", "0"],
        &steps,
    );
    assert!(
        stderr(&expired).contains("run reason=expired age="),
        "{}",
        stderr(&expired)
    );
    run(&[]);
    set_completion(&result, SystemTime::now() + Duration::from_secs(3600));
    let future_output = run(&["--include-codes", "0"]);
    assert!(
        stderr(&future_output).contains("run reason=future-timestamp age=-"),
        "{}",
        stderr(&future_output)
    );
    assert_eq!(s.count(), "xxxxxxxx");
    for output in [first, hit, policy, expired, future_output] {
        assert_eq!(code(&output), Some(7));
        assert!(
            stderr(&output).contains(&format!("key={key}")),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn verbose_keys_and_escaped_paths_do_not_disclose_inputs() {
    let s = Sandbox::new();
    let directory = s.path("cache\nforged\r\x1b");
    let run = |key: &str, cwd: &std::path::Path, options: &[&str]| {
        let mut command = s.bare_command();
        command
            .current_dir(cwd)
            .env("PRIVATE_VALUE", "environment-secret")
            .arg("--cache-dir")
            .arg(&directory)
            .args(["--ttl", "1h", "--verbose", "--key", key])
            .args(options)
            .args(["--", CHILD, "exit:0", "argument-secret"]);
        s.spawn("run", command).finish()
    };
    let a = run("key-secret", s.root(), &[]);
    let b = run("another-key", s.root(), &[]);
    let sub = s.path("sub");
    fs::create_dir(&sub).unwrap();
    let c = run("key-secret", &sub, &[]);
    let d = run(
        "key-secret",
        s.root(),
        &["--refresh", "--exclude-codes", "1"],
    );
    let key = |output: &Output| {
        stderr(output)
            .split("key=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    assert_ne!(key(&a), key(&b), "--key");
    assert_ne!(key(&a), key(&c), "working directory");
    assert_eq!(key(&a), key(&d), "refresh and code selection");
    for output in [a, b, c, d] {
        let text = stderr(&output);
        let lines: Vec<_> = text.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with("cacheexec: verbose: ")),
            "{text}"
        );
        assert!(text.contains("cache\\nforged\\r\\u{1b}"), "{text}");
        for secret in [
            "key-secret",
            "another-key",
            "environment-secret",
            "argument-secret",
        ] {
            assert!(!text.contains(secret), "{secret} disclosed: {text}");
        }
    }
}

#[test]
fn verbose_failure_interrupt_and_cli_contracts() {
    let s = Sandbox::new();
    let mut command = s.command();
    command.args(["--ttl", "1h", "--verbose", "--", "./does-not-exist"]);
    let missing = s.spawn("missing program", command).finish();
    assert_eq!(code(&missing), Some(125));
    assert!(
        stderr(&missing).contains("failed saved=no reason=failure"),
        "{}",
        stderr(&missing)
    );
    assert!(!stderr(&missing).contains("completed exit="));
    let interrupted = s.run(&["--ttl", "1h", "--verbose"], &["raise:TERM"]);
    assert_eq!(code(&interrupted), Some(143));
    assert!(
        stderr(&interrupted).contains("interrupted exit=143 saved=no reason=interrupted"),
        "{}",
        stderr(&interrupted)
    );
    let mut command = s.command();
    command.args(["--clear", "--verbose"]);
    assert_eq!(code(&s.spawn("clear verbose", command).finish()), Some(2));
    let mut command = s.command();
    command.arg("--help");
    let help = stdout(&s.spawn("help", command).finish());
    for expected in [
        "cacheexec --ttl <TTL> [OPTIONS] -- <COMMAND>...",
        "cacheexec --clear [--older-than <DURATION>] [--cache-dir <PATH>]",
        "--verbose",
        "stderr",
        "best effort",
        "not a stable format",
    ] {
        assert!(
            help.contains(expected),
            "{expected:?} missing from help:\n{help}"
        );
    }
}

#[test]
fn verbose_completion_has_its_own_line_after_unterminated_child_stderr() {
    let s = Sandbox::new();
    let steps = ["count", "err:child"];
    for name in ["run", "hit"] {
        let output = s.run(&["--verbose", "--ttl", "1h"], &steps);
        let text = stderr(&output);
        assert!(
            text.contains("child\n") && text.contains("\ncacheexec: verbose: completed exit=0"),
            "{name}: {text}"
        );
        assert_eq!(verbose_lines(&output.stderr).len(), 2, "{name}: {text}");
    }
    assert_eq!(s.run(&["--ttl", "1h"], &steps).stderr, b"child");
    assert_eq!(s.count(), "x");
}

#[test]
fn verbose_delayed_decisions_keep_line_boundaries_during_concurrent_replay() {
    let s = Sandbox::new();
    let steps = ["count", "err:child"];
    s.run(&["--ttl", "1h"], &steps);
    let mut decisions = 0;
    for _ in 0..4 {
        let callers: Vec<_> = (0..16)
            .map(|_| s.spawn("hit", s.cacheexec(&["--verbose", "--ttl", "1h"], &steps)))
            .collect();
        for caller in callers {
            let output = caller.finish();
            let text = stderr(&output);
            assert_eq!(code(&output), Some(0), "{text}");
            // Diagnostics are best effort and may be dropped under load, but
            // whatever is written starts on its own line and is complete.
            assert_eq!(common::child_stderr(&output.stderr), b"child", "{text}");
            let lines = verbose_lines(&output.stderr);
            assert!(lines.len() <= 2, "{text}");
            assert_eq!(
                text.matches("cacheexec: verbose:").count(),
                lines.len(),
                "{text}"
            );
            for line in lines {
                assert!(
                    line.starts_with("hit age=") || line.starts_with("completed exit=0 "),
                    "malformed diagnostic {line:?} in {text}"
                );
                decisions += usize::from(line.starts_with("hit age="));
            }
        }
    }
    assert!(decisions > 0, "no caller reported its decision");
    assert_eq!(s.count(), "x");
}

/// Changes a path's mode and restores 0o755 when dropped, even on failure, so
/// the sandbox stays removable.
struct Mode<'a>(&'a std::path::Path);

impl<'a> Mode<'a> {
    fn set(path: &'a std::path::Path, mode: u32) -> Self {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        Self(path)
    }
}

impl Drop for Mode<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}
