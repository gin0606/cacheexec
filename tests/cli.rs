use std::{
    fs,
    process::{Command, Output},
    thread,
    time::Duration,
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
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
        cmd.current_dir(self.root.path())
            .arg("--cache-dir")
            .arg(self.root.path().join("cache"));
        cmd
    }
    fn run(&self, options: &[&str], script: &str) -> Output {
        self.command()
            .args(options)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    }
    fn count(&self) -> String {
        fs::read_to_string(self.root.path().join("count")).unwrap()
    }
    fn result(&self) -> std::path::PathBuf {
        fs::read_dir(self.root.path().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "result"))
            .unwrap()
    }
}
const SCRIPT: &str =
    "printf x >> count; printf '\\377\\000out'; printf '\\376err' >&2; exit ${CODE:-0}";

#[test]
fn hit_replays_binary_streams_and_all_exit_codes() {
    for code in [0, 1, 23, 125, 255] {
        let f = Fixture::new();
        let run = || {
            f.command()
                .env("CODE", code.to_string())
                .args(["--ttl", "1h", "--", "sh", "-c", SCRIPT])
                .output()
                .unwrap()
        };
        let first = run();
        let hit = run();
        assert_eq!(first.status.code(), Some(code));
        assert_eq!(hit.status.code(), Some(code));
        assert_eq!(first.stdout, b"\xff\0out");
        assert_eq!(first.stderr, b"\xfeerr");
        assert_eq!(first.stdout, hit.stdout);
        assert_eq!(first.stderr, hit.stderr);
        assert_eq!(f.count(), "x");
    }
}
#[test]
fn ttl_is_a_per_call_completion_age() {
    let f = Fixture::new();
    let script = "printf x >> count; sleep 0.15";
    assert!(f.run(&["--ttl", "100ms"], script).status.success());
    assert!(f.run(&["--ttl", "100ms"], script).status.success());
    assert_eq!(f.count(), "x");
    thread::sleep(Duration::from_millis(130));
    assert!(f.run(&["--ttl", "1h"], script).status.success());
    assert_eq!(f.count(), "x");
    assert!(f.run(&["--ttl", "100ms"], script).status.success());
    assert_eq!(f.count(), "xx");
}
#[test]
fn refresh_and_policy_invalidate_old_results() {
    let f = Fixture::new();
    let script = "printf x >> count; exit ${CODE:-0}";
    assert!(f.run(&["--ttl", "1h"], script).status.success());
    let output = f
        .command()
        .env("CODE", "7")
        .args([
            "--ttl",
            "1h",
            "--refresh",
            "--include-codes",
            "0,1",
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(f.run(&["--ttl", "1h"], script).status.success());
    assert_eq!(f.count(), "xxx");
    assert_eq!(
        f.run(&["--ttl", "1h", "--exclude-codes", "0"], script)
            .status
            .code(),
        Some(0)
    );
    assert!(f.run(&["--ttl", "1h"], script).status.success());
    assert_eq!(f.count(), "xxxxx");
    assert_eq!(
        f.run(
            &[
                "--ttl",
                "1h",
                "--include-codes",
                "0",
                "--exclude-codes",
                "1"
            ],
            script
        )
        .status
        .code(),
        Some(2)
    );
}
#[test]
fn keys_preserve_argv_cwd_and_extra_key_not_environment() {
    let f = Fixture::new();
    let script = "printf x >> count; printf '%s|' \"$@\"";
    let run = |args: &[&str], key: &str| {
        f.command()
            .args(["--ttl", "1h", "--key", key, "--", "sh", "-c", script, "sh"])
            .args(args)
            .output()
            .unwrap()
    };
    assert_eq!(run(&["a b", "c"], "one").stdout, b"a b|c|");
    assert_eq!(run(&["a", "b c"], "one").stdout, b"a|b c|");
    run(&["a", "b c"], "two");
    run(&["a", "b c"], "two");
    assert_eq!(f.count(), "xxx");
    let sub = f.root.path().join("sub");
    fs::create_dir(&sub).unwrap();
    assert!(
        f.command()
            .current_dir(&sub)
            .args([
                "--ttl", "1h", "--key", "two", "--", "sh", "-c", script, "sh", "a", "b c"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(sub.join("count")).unwrap(), "x");
    let env_script = "printf %s \"$VALUE\"";
    let envrun = |value| {
        f.command()
            .env("VALUE", value)
            .args(["--ttl", "1h", "--", "sh", "-c", env_script])
            .output()
            .unwrap()
    };
    assert_eq!(envrun("one").stdout, b"one");
    assert_eq!(envrun("two").stdout, b"one");
}
#[test]
fn drains_large_outputs_and_closes_stdin() {
    let f = Fixture::new();
    let script = "test -z \"$(cat)\" || exit 9; (dd if=/dev/zero bs=65536 count=32 2>/dev/null) & dd if=/dev/zero bs=65536 count=32 1>&2 2>/dev/null; wait";
    let first = f.run(&["--ttl", "1h"], script);
    assert!(first.status.success());
    assert_eq!(first.stdout.len(), 2_097_152);
    assert_eq!(first.stderr.len(), 2_097_152);
    let hit = f.run(&["--ttl", "1h"], script);
    assert_eq!(first.stdout, hit.stdout);
    assert_eq!(first.stderr, hit.stderr);
}
#[test]
fn failures_do_not_restore_old_results_or_repeat_child() {
    let f = Fixture::new();
    let script =
        "printf x >> count; if test -f fail; then rm -r cache; printf blocked > cache; fi; exit 7";
    assert_eq!(f.run(&["--ttl", "1h"], script).status.code(), Some(7));
    fs::write(f.root.path().join("fail"), "").unwrap();
    let failed = f.run(&["--ttl", "1h", "--refresh"], script);
    assert_eq!(failed.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("child already completed with exit code 7")
    );
    assert_eq!(f.count(), "xx");
    fs::remove_file(f.root.path().join("cache")).unwrap();
    fs::remove_file(f.root.path().join("fail")).unwrap();
    assert_eq!(f.run(&["--ttl", "1h"], script).status.code(), Some(7));
    assert_eq!(f.count(), "xxx");
}
#[test]
fn corruption_is_an_error_and_never_executes_child() {
    let f = Fixture::new();
    f.run(&["--ttl", "1h"], SCRIPT);
    fs::write(f.result(), "partial").unwrap();
    for refresh in [false, true] {
        let mut opts = vec!["--ttl", "1h"];
        if refresh {
            opts.push("--refresh");
        }
        let output = f.run(&opts, SCRIPT);
        assert_eq!(output.status.code(), Some(125));
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains("corrupt cached result"));
        assert!(diagnostic.contains(f.result().to_str().unwrap()));
        assert!(diagnostic.contains("stop all cacheexec invocations"));
        assert!(diagnostic.contains("remove only this .result file"));
        assert!(diagnostic.contains("keep .lock and .active files"));
    }
    let clear = f.command().arg("--clear").output().unwrap();
    assert_eq!(clear.status.code(), Some(125));
    let diagnostic = String::from_utf8_lossy(&clear.stderr);
    assert!(diagnostic.contains(f.result().to_str().unwrap()));
    assert!(diagnostic.contains("Recovery:"));
    assert_eq!(f.count(), "x");
    fs::remove_file(f.result()).unwrap();
    assert!(f.run(&["--ttl", "1h"], SCRIPT).status.success());
    assert_eq!(f.count(), "xx");
}
#[test]
fn spawn_failure_and_signals_are_not_cached() {
    let f = Fixture::new();
    let program = f.root.path().join("program");
    fs::write(&program, "#!/bin/sh\nprintf x >> count\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |refresh| {
        let mut cmd = f.command();
        cmd.args(["--ttl", "1h"]);
        if refresh {
            cmd.arg("--refresh");
        }
        cmd.arg("--").arg(&program).output().unwrap()
    };
    assert!(run(false).status.success());
    fs::remove_file(&program).unwrap();
    for refresh in [true, false] {
        let output = run(refresh);
        assert_eq!(output.status.code(), Some(125));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("could not start")
                .count(),
            1
        );
    }
    let script = "printf x >> count; kill -TERM $$";
    assert_eq!(f.run(&["--ttl", "1h"], script).status.code(), Some(143));
    assert_eq!(f.run(&["--ttl", "1h"], script).status.code(), Some(143));
    assert_eq!(f.count(), "xxx");
}
#[test]
fn output_transfer_failure_is_reported_once() {
    use std::process::Stdio;
    let f = Fixture::new();
    let mut child = f
        .command()
        .args([
            "--ttl",
            "1h",
            "--",
            "sh",
            "-c",
            "while ! test -f go; do sleep 0.01; done; printf output",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    fs::write(f.root.path().join("go"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert_eq!(diagnostic.matches("output transfer failed").count(), 1);
    assert_eq!(diagnostic.matches("child already completed").count(), 1);
}
#[test]
fn storage_access_errors_are_diagnosed() {
    let f = Fixture::new();
    f.run(&["--ttl", "1h"], SCRIPT);
    let path = f.result();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let output = f.run(&["--ttl", "1h"], SCRIPT);
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(f.count(), "x");
}
#[test]
fn xdg_and_home_defaults_and_required_arguments() {
    let f = Fixture::new();
    for xdg in [true, false] {
        let directory = f.root.path().join(if xdg { "xdg" } else { "home" });
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cacheexec"));
        cmd.env("HOME", &directory).env_remove("XDG_CACHE_HOME");
        if xdg {
            cmd.env("XDG_CACHE_HOME", &directory);
        }
        assert!(
            cmd.args(["--ttl", "1h", "--", "true"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            directory
                .join(if xdg { "cacheexec" } else { ".cache/cacheexec" })
                .is_dir()
        );
    }
    for args in [
        vec!["--", "true"],
        vec!["--ttl", "invalid", "--", "true"],
        vec!["--ttl", "1h", "true"],
    ] {
        assert_eq!(
            f.command().args(args).output().unwrap().status.code(),
            Some(2)
        );
    }
}

#[test]
fn clear_all_age_directory_isolation_and_condition_fixture() {
    for code in [0, 1, 7] {
        let f = Fixture::new();
        let run = || {
            f.command()
                .env("CODE", code.to_string())
                .args([
                    "--ttl",
                    "5m",
                    "--include-codes",
                    "0,1",
                    "--",
                    "sh",
                    "-c",
                    SCRIPT,
                ])
                .output()
                .unwrap()
        };
        assert_eq!(run().status.code(), Some(code));
        assert_eq!(run().status.code(), Some(code));
        assert_eq!(f.count(), if code == 7 { "xx" } else { "x" });
        let recent = f
            .command()
            .args(["--clear", "--older-than", "1h"])
            .output()
            .unwrap();
        assert!(recent.status.success());
        assert!(String::from_utf8_lossy(&recent.stdout).contains("removed=0"));
        let other = Command::new(env!("CARGO_BIN_EXE_cacheexec"))
            .arg("--cache-dir")
            .arg(f.root.path().join("other"))
            .arg("--clear")
            .output()
            .unwrap();
        assert!(other.status.success());
        assert!(!f.root.path().join("other").exists());
        let clear = f.command().arg("--clear").output().unwrap();
        assert!(clear.status.success());
        assert!(
            String::from_utf8_lossy(&clear.stdout).contains(if code == 7 {
                "removed=0"
            } else {
                "removed=1"
            })
        );
        assert_eq!(run().status.code(), Some(code));
        assert_eq!(f.count(), if code == 7 { "xxx" } else { "xx" });
        let expired = f
            .command()
            .args(["--clear", "--older-than", "0s"])
            .output()
            .unwrap();
        assert!(expired.status.success());
        assert!(
            !fs::read_dir(f.root.path().join("cache"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "result"))
        );
    }
}

#[test]
fn clear_reports_partial_failure_and_preserves_corruption() {
    let f = Fixture::new();
    f.run(&["--ttl", "1h"], "true");
    let broken = f
        .root
        .path()
        .join("cache")
        .join(format!("{}.result", "0".repeat(64)));
    fs::write(&broken, "corrupt").unwrap();
    let output = f.command().arg("--clear").output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("removed=1")
            && diagnostic.contains("failed=1")
            && diagnostic.contains("corrupt")
    );
    assert!(broken.exists());
    fs::remove_file(&broken).unwrap();
    fs::create_dir(&broken).unwrap();
    assert_eq!(
        f.command().arg("--clear").output().unwrap().status.code(),
        Some(125)
    );
    for args in [
        vec!["--clear", "--ttl", "1h"],
        vec!["--older-than", "1h"],
        vec!["--clear", "--", "true"],
    ] {
        assert_eq!(
            f.command().args(args).output().unwrap().status.code(),
            Some(2)
        );
    }
}

#[test]
fn clear_permission_failure_is_explicit() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    f.run(&["--ttl", "1h"], "true");
    let directory = f.root.path().join("cache");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
    let output = f.command().arg("--clear").output().unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&output.stderr).contains("delete cached result"));
    assert!(f.result().exists());
}
