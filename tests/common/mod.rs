//! Shared fixtures for the integration tests.
//!
//! Child commands are `cacheexec-test-child` step lists (see tests/bin/cacheexec-test-child.rs).
//! Tests synchronize with them through FIFOs instead of polling:
//! - a [`Gate`] blocks children until the test releases it;
//! - children report progress with `event:LABEL`, read by [`Sandbox::wait_event`].
//!
//! Every wait has a deadline, reports the sandbox state when it expires, and
//! processes left running by a failed test are killed when their [`Proc`] or
//! [`Sandbox`] is dropped.
#![allow(dead_code)]

use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// How long any single wait may take before the test fails.
pub const TIMEOUT: Duration = Duration::from_secs(20);

pub const CACHEEXEC: &str = env!("CARGO_BIN_EXE_cacheexec");
pub const CHILD: &str = env!("CARGO_BIN_EXE_cacheexec-test-child");

/// The exit code, or `None` when the process was killed by a signal.
pub fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `--verbose` lines in `stderr`, without their prefix.
pub fn verbose_lines(stderr: &[u8]) -> Vec<String> {
    text(stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("cacheexec: verbose: "))
        .map(str::to_owned)
        .collect()
}

/// `stderr` with the `--verbose` diagnostics removed, i.e. the child's own bytes.
pub fn child_stderr(stderr: &[u8]) -> Vec<u8> {
    const MARK: &[u8] = b"\ncacheexec: verbose: ";
    let mut rest = stderr;
    let mut kept = Vec::new();
    while let Some(start) = find(rest, MARK) {
        kept.extend_from_slice(&rest[..start]);
        let line = &rest[start + 1..];
        let end = line
            .iter()
            .position(|&b| b == b'\n')
            .map_or(line.len(), |i| i + 1);
        rest = &line[end..];
    }
    kept.extend_from_slice(rest);
    kept
}

/// The position of `needle` in `haystack`.
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Runs a test and its temporary directory.
///
/// Field order matters: fields drop in declaration order, and the directory
/// must outlive the FIFOs and processes that use it.
pub struct Sandbox {
    events: Events,
    root: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let events = Events::new(root.path().join("events"));
        Self { events, root }
    }

    pub fn path(&self, name: impl AsRef<Path>) -> PathBuf {
        self.root.path().join(name)
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn cache(&self) -> PathBuf {
        self.path("cache")
    }

    /// `cacheexec --cache-dir <cache>` run in the sandbox, with piped output.
    pub fn command(&self) -> Command {
        let mut command = self.bare_command();
        command.arg("--cache-dir").arg(self.cache());
        command
    }

    /// `cacheexec` run in the sandbox with piped output, without `--cache-dir`.
    pub fn bare_command(&self) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(CACHEEXEC);
        // cacheexec keeps signals ignored at startup ignored, so do not pass
        // on a test runner's own ignored signals; tests that want some ignored
        // add their own `pre_exec`, which runs after this one.
        unsafe {
            command.pre_exec(|| {
                for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        command
            .current_dir(self.root())
            .env("CACHEEXEC_TEST_EVENTS", self.events.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `cacheexec --clear ARGS` for the sandbox's cache.
    pub fn clear(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.arg("--clear").args(args);
        self.spawn("clear", command).finish()
    }

    /// `cacheexec OPTIONS -- cacheexec-test-child STEPS`.
    pub fn cacheexec(&self, options: &[&str], steps: &[&str]) -> Command {
        let mut command = self.command();
        command.args(options).arg("--").arg(CHILD).args(steps);
        command
    }

    /// Runs to completion and returns the output. The events it reported are
    /// discarded, so later waits only see events of commands still running.
    pub fn run(&self, options: &[&str], steps: &[&str]) -> Output {
        let output = self.spawn("run", self.cacheexec(options, steps)).finish();
        self.events.discard_finished();
        output
    }

    pub fn spawn(&self, name: &str, mut command: Command) -> Proc<'_> {
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("could not start {name}: {error}"));
        let out = child.stdout.take().map(Capture::new);
        let err = child.stderr.take().map(Capture::new);
        Proc {
            name: name.to_owned(),
            child: Some(child),
            out,
            err,
            sandbox: self,
        }
    }

    /// A FIFO at `name` that blocks `wait:name` until released.
    pub fn gate(&self, name: &str) -> Gate {
        Gate::new(self.path(name))
    }

    /// Waits for the next `event:LABEL` and returns the reporting process id.
    pub fn wait_event(&self, label: &str) -> i32 {
        match self.events.wait(label, Instant::now() + TIMEOUT) {
            Some(event) => event.pid,
            None => panic!("no `{label}` event within {TIMEOUT:?}\n{}", self.state()),
        }
    }

    /// Polls `ready` until it holds, failing the test after [`TIMEOUT`].
    pub fn wait_until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "{what} did not happen within {TIMEOUT:?}\n{}",
                self.state()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The execution counter written by the child's `count` step.
    pub fn count(&self) -> String {
        fs::read_to_string(self.path("count")).unwrap_or_default()
    }

    /// Cache files with this extension, e.g. `result`, `lock` or `active`.
    pub fn cache_files(&self, extension: &str) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(self.cache())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.extension().is_some_and(|e| e == extension))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// The only cache file with this extension.
    pub fn cache_file(&self, extension: &str) -> PathBuf {
        let files = self.cache_files(extension);
        assert_eq!(
            files.len(),
            1,
            "expected one .{extension} file\n{}",
            self.state()
        );
        files.into_iter().next().unwrap()
    }

    /// Processes, cache files and events, for failure messages.
    pub fn state(&self) -> String {
        let mut state = String::new();
        state.push_str("processes:\n");
        state.push_str(&processes(self.root()));
        state.push_str("cache files:\n");
        if let Ok(entries) = fs::read_dir(self.cache()) {
            let mut entries: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
            entries.sort();
            for path in entries {
                let bytes = fs::read(&path).unwrap_or_default();
                let tag = if path.extension().is_some_and(|e| e == "active") {
                    format!(" tag={:?}", bytes.get(256))
                } else {
                    String::new()
                };
                state.push_str(&format!(
                    "  {} len={}{tag}\n",
                    path.file_name().unwrap().to_string_lossy(),
                    bytes.len()
                ));
            }
        }
        state.push_str(&format!("events: {:?}\n", self.events.all()));
        state
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // A passing test leaves no process behind, and a group id may since have
        // been reused by another test running in parallel, so only clean up
        // after a failure. Every child reports its group when it starts.
        if !thread::panicking() {
            return;
        }
        // A second panic here would abort every test in the binary.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let own = unsafe { libc::getpgrp() };
            let events = self.events.all();
            let ended: Vec<i32> = events.iter().filter(|e| e.ended).map(|e| e.group).collect();
            let mut groups: Vec<i32> = events.iter().map(|event| event.group).collect();
            groups.sort_unstable();
            groups.dedup();
            for group in groups {
                if group != own && !ended.contains(&group) {
                    unsafe { libc::killpg(group, libc::SIGKILL) };
                }
            }
        }));
    }
}

/// Best-effort process listing for failure messages. `ps` is missing or cannot
/// list processes in some build sandboxes, where /proc is scanned instead.
fn processes(root: &Path) -> String {
    let root = root.to_string_lossy().into_owned();
    // cacheexec runs name the sandbox; children name the child command.
    let relevant = |line: &str| line.contains(&root) || line.contains(CHILD);
    let listed = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,command="])
        .output()
        .ok()
        .filter(|output| output.status.success() && !output.stdout.is_empty());
    if let Some(output) = listed {
        return text(&output.stdout)
            .lines()
            .filter(|line| relevant(line))
            .map(|line| format!("  {}\n", line.trim()))
            .collect();
    }
    let mut listing = String::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let command = fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let command = text(&command).replace('\0', " ");
        if relevant(&command) {
            listing.push_str(&format!(
                "  {} {command}\n",
                entry.file_name().to_string_lossy()
            ));
        }
    }
    listing
}

/// A running cacheexec process. Its piped output is read continuously, so a
/// full pipe never blocks it; dropping it before [`Proc::finish`] kills it.
pub struct Proc<'a> {
    name: String,
    child: Option<Child>,
    out: Option<Capture>,
    err: Option<Capture>,
    sandbox: &'a Sandbox,
}

impl Proc<'_> {
    pub fn pid(&self) -> i32 {
        self.child.as_ref().unwrap().id() as i32
    }

    pub fn signal(&self, signal: i32) {
        assert_eq!(unsafe { libc::kill(self.pid(), signal) }, 0);
    }

    /// Whether the process is still running.
    pub fn running(&mut self) -> bool {
        self.child.as_mut().unwrap().try_wait().unwrap().is_none()
    }

    /// Waits until stdout contains `needle`.
    pub fn wait_stdout(&self, needle: &str) {
        self.wait_capture(self.out.as_ref(), "stdout", needle);
    }

    /// Waits until stderr contains `needle`.
    pub fn wait_stderr(&self, needle: &str) {
        self.wait_capture(self.err.as_ref(), "stderr", needle);
    }

    /// The stderr read so far.
    pub fn stderr_so_far(&self) -> Vec<u8> {
        self.err.as_ref().map(Capture::snapshot).unwrap_or_default()
    }

    fn wait_capture(&self, capture: Option<&Capture>, stream: &str, needle: &str) {
        let capture = capture.unwrap_or_else(|| panic!("{} {stream} is not captured", self.name));
        if !capture.wait_for(needle.as_bytes(), Instant::now() + TIMEOUT) {
            panic!(
                "{} (pid {}) did not write {needle:?} to {stream} within {TIMEOUT:?}\n{stream}: {:?}\n{}",
                self.name,
                self.pid(),
                text(&capture.snapshot()),
                self.sandbox.state()
            );
        }
    }

    /// Waits for the process to exit and returns its output.
    pub fn finish(mut self) -> Output {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let state = self.sandbox.state();
                self.kill();
                panic!(
                    "{} did not exit within {TIMEOUT:?}\nstdout: {:?}\nstderr: {:?}\n{state}",
                    self.name,
                    text(&self.out.as_ref().map(Capture::snapshot).unwrap_or_default()),
                    text(&self.stderr_so_far()),
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        self.child = None;
        let collect = |capture: Option<Capture>, stream: &str| {
            let Some(capture) = capture else {
                return Vec::new();
            };
            // A descendant may still hold the pipe open; it must end in time too.
            if !capture.wait_done(Instant::now() + TIMEOUT) {
                panic!(
                    "{} exited but its {stream} stayed open for {TIMEOUT:?}\n{}",
                    self.name,
                    self.sandbox.state()
                );
            }
            capture.snapshot()
        };
        let stdout = collect(self.out.take(), "stdout");
        let stderr = collect(self.err.take(), "stderr");
        self.sandbox.events.note_ended();
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Proc<'_> {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads a pipe to its end in the background.
struct Capture {
    shared: Arc<Captured>,
}

#[derive(Default)]
struct Captured {
    state: Mutex<CapturedState>,
    changed: Condvar,
}

#[derive(Default)]
struct CapturedState {
    bytes: Vec<u8>,
    done: bool,
}

impl Capture {
    fn new(mut pipe: impl Read + Send + 'static) -> Self {
        let shared = Arc::new(Captured::default());
        let writer = Arc::clone(&shared);
        thread::spawn(move || {
            let mut buffer = [0; 65536];
            loop {
                let read = loop {
                    match pipe.read(&mut buffer) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        result => break result.unwrap_or(0),
                    }
                };
                let mut state = writer.state.lock().unwrap();
                if read == 0 {
                    state.done = true;
                } else {
                    state.bytes.extend_from_slice(&buffer[..read]);
                }
                writer.changed.notify_all();
                if read == 0 {
                    return;
                }
            }
        });
        Self { shared }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.shared.state.lock().unwrap().bytes.clone()
    }

    fn wait_for(&self, needle: &[u8], deadline: Instant) -> bool {
        self.wait_until(deadline, |state| find(&state.bytes, needle).is_some())
    }

    fn wait_done(&self, deadline: Instant) -> bool {
        self.wait_until(deadline, |state| state.done)
    }

    fn wait_until(&self, deadline: Instant, ready: impl Fn(&CapturedState) -> bool) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if ready(&state) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }
}

fn mkfifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::mkfifo(name.as_ptr(), 0o600) },
        0,
        "mkfifo {path:?}: {}",
        std::io::Error::last_os_error()
    );
}

/// A FIFO that children wait on with `wait:NAME`.
///
/// The test holds both ends, so a child's open returns at once and its read
/// blocks until [`Gate::release`] writes to it. Release also replaces the FIFO
/// with a regular file, so later runs of the same command pass through.
pub struct Gate {
    path: PathBuf,
    identity: (u64, u64),
    released: bool,
    // Kept open until drop: a child that looked the FIFO up just before the
    // release still finds a writer and the bytes when it opens it.
    _reader: File,
    writer: File,
}

impl Gate {
    fn new(path: PathBuf) -> Self {
        // A released gate is a regular file; arm it again.
        let _ = fs::remove_file(&path);
        mkfifo(&path);
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        // Does not block, because a reader exists.
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let metadata = writer.metadata().unwrap();
        Self {
            path,
            identity: (metadata.dev(), metadata.ino()),
            released: false,
            _reader: reader,
            writer,
        }
    }

    pub fn release(&mut self) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        // Only replace the name while it is still this FIFO; a later gate may
        // have armed the same name again.
        let current = fs::symlink_metadata(&self.path).ok();
        if current.is_some_and(|m| (m.dev(), m.ino()) == self.identity) {
            let open = self.path.with_extension("released");
            fs::write(&open, "").unwrap();
            fs::rename(&open, &self.path).unwrap();
        }
        // One byte wakes one blocked child; macOS does not reliably wake every
        // reader of a FIFO when its writer closes. The test's own reader keeps
        // the write from failing, and the pipe holds far more than this.
        (&self.writer).write_all(&[0; 256]).unwrap();
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.release()));
    }
}

/// The FIFO children report `event:LABEL` lines to.
struct Events {
    path: PathBuf,
    reader: File,
    // Held so the reader never sees end of file between writers.
    _writer: File,
    log: Mutex<EventLog>,
}

#[derive(Default)]
struct EventLog {
    partial: Vec<u8>,
    events: Vec<Event>,
}

#[derive(Clone, Debug)]
struct Event {
    label: String,
    pid: i32,
    group: i32,
    seen: bool,
    /// The group was seen to have ended; its id may since have been reused.
    ended: bool,
}

impl Events {
    fn new(path: PathBuf) -> Self {
        mkfifo(&path);
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        Self {
            path,
            reader,
            _writer: writer,
            log: Mutex::new(EventLog::default()),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn all(&self) -> Vec<Event> {
        self.read_available();
        self.log.lock().unwrap().events.clone()
    }

    fn read_available(&self) {
        let mut log = self.log.lock().unwrap();
        let mut buffer = [0; 4096];
        loop {
            match (&self.reader).read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => log.partial.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("read events: {error}"),
            }
        }
        while let Some(end) = log.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = log.partial.drain(..=end).collect();
            let line = text(&line[..end]);
            let mut fields = line.rsplitn(3, ' ');
            let group = fields.next().unwrap().parse().unwrap();
            let pid = fields.next().unwrap().parse().unwrap();
            let label = fields.next().unwrap().to_owned();
            log.events.push(Event {
                label,
                pid,
                group,
                seen: false,
                ended: false,
            });
        }
    }

    /// Records which reported process groups have ended, before their ids can
    /// be reused.
    fn note_ended(&self) {
        self.read_available();
        for event in &mut self.log.lock().unwrap().events {
            if unsafe { libc::killpg(event.group, 0) } != 0 {
                event.ended = true;
            }
        }
    }

    /// Marks the events of process groups that have ended as seen, so later
    /// waits only find events from commands still running or yet to start.
    fn discard_finished(&self) {
        self.note_ended();
        for event in &mut self.log.lock().unwrap().events {
            if event.ended {
                event.seen = true;
            }
        }
    }

    fn wait(&self, label: &str, deadline: Instant) -> Option<Event> {
        loop {
            self.read_available();
            {
                let mut log = self.log.lock().unwrap();
                if let Some(event) = log
                    .events
                    .iter_mut()
                    .find(|event| !event.seen && event.label == label)
                {
                    event.seen = true;
                    return Some(event.clone());
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let mut poll = libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let wait = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
            unsafe { libc::poll(&mut poll, 1, wait) };
        }
    }
}

/// A close-on-exec pipe as (read end, write end).
pub fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // Atomically close-on-exec where the platform allows it. Elsewhere a
    // process another test spawns in between can inherit an end for its whole
    // life; waits on these pipes have deadlines, so that fails instead of hangs.
    #[cfg(target_os = "linux")]
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// The write end of a pipe whose every read end is closed, so writes fail with
/// EPIPE. A process forked concurrently by another test can briefly inherit the
/// read end before close-on-exec is set, so closure is confirmed by a probe
/// write; once no read end remains, none can reappear.
pub fn closed_pipe() -> OwnedFd {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let (reader, writer) = pipe();
        drop(reader);
        let probe = unsafe { libc::write(writer.as_raw_fd(), b"x".as_ptr().cast(), 1) };
        if probe < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::BrokenPipe {
            return writer;
        }
        assert!(Instant::now() < deadline, "could not create a closed pipe");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A pipe that is already full and whose reader never reads.
pub struct FullPipe {
    _reader: OwnedFd,
    writer: OwnedFd,
}

impl FullPipe {
    pub fn new() -> Self {
        let (reader, writer) = pipe();
        set_nonblocking(&writer, true);
        let mut file = File::from(writer.try_clone().unwrap());
        while file.write(&[0; 4096]).is_ok() {}
        Self {
            _reader: reader,
            writer,
        }
    }

    /// A write end whose writes fail with EAGAIN, a failure other than a
    /// closed reader.
    pub fn nonblocking(&self) -> OwnedFd {
        set_nonblocking(&self.writer, true);
        self.writer.try_clone().unwrap()
    }

    /// A write end whose writes block forever. The flag belongs to the open
    /// pipe, so this switches every end handed out by this `FullPipe`.
    pub fn blocking(&self) -> OwnedFd {
        set_nonblocking(&self.writer, false);
        self.writer.try_clone().unwrap()
    }
}

/// Whether writes to `fd` fail instead of blocking.
pub fn is_nonblocking(fd: &OwnedFd) -> bool {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    flags & libc::O_NONBLOCK != 0
}

fn set_nonblocking(fd: &OwnedFd, nonblocking: bool) {
    let flags = if nonblocking { libc::O_NONBLOCK } else { 0 };
    assert_eq!(
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags) },
        0
    );
}

/// Reads one byte from `reader`, failing the test if none arrives in time.
pub fn read_byte(reader: &OwnedFd) -> u8 {
    let mut poll = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, TIMEOUT.as_millis() as i32) };
    assert_eq!(ready, 1, "nothing to read within {TIMEOUT:?}");
    let mut byte = 0u8;
    let read = unsafe { libc::read(reader.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
    assert_eq!(read, 1, "read failed: {}", std::io::Error::last_os_error());
    byte
}

/// Reads `reader` to its end in the background.
pub struct Drained(Capture);

impl Drained {
    /// The bytes read, once the pipe has ended.
    pub fn bytes(self) -> Vec<u8> {
        assert!(
            self.0.wait_done(Instant::now() + TIMEOUT),
            "pipe stayed open for {TIMEOUT:?}"
        );
        self.0.snapshot()
    }
}

pub fn drain(reader: OwnedFd) -> Drained {
    Drained(Capture::new(File::from(reader)))
}
