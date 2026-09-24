use crate::domain::message::{self, Decision, Failure, Saved};
use std::{
    cell::Cell,
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

pub struct Verbose {
    sender: Option<Sender<String>>,
    drained: Option<Receiver<()>>,
    finished: Cell<bool>,
}

impl Verbose {
    pub fn new(enabled: bool) -> Self {
        let mut diagnostic = Self {
            sender: None,
            drained: None,
            finished: Cell::new(false),
        };
        if !enabled {
            return diagnostic;
        }
        // A separate descriptor avoids the standard stderr mutex without changing
        // O_NONBLOCK on the open file description shared with the caller.
        let fd = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return diagnostic;
        }
        let mut output = unsafe { File::from_raw_fd(fd) };
        // On macOS an over-limit file write can deliver SIGXFSZ to another
        // thread. Drop diagnostics to regular files under a finite size limit;
        // checking only the current offset would race other writers/O_APPEND.
        if !safe_destination(&output) {
            return diagnostic;
        }
        let (sender, messages) = mpsc::channel::<String>();
        let (completed, drained) = mpsc::channel();
        if std::thread::Builder::new()
            .name("verbose".into())
            .spawn(move || {
                // Diagnostic EPIPE/EFBIG must remain write errors, even when the
                // caller imposed a file-size limit or changed signal dispositions.
                // Background TOSTOP terminals must not suspend the process either.
                unsafe {
                    let mut blocked: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut blocked);
                    libc::sigaddset(&mut blocked, libc::SIGPIPE);
                    libc::sigaddset(&mut blocked, libc::SIGXFSZ);
                    libc::sigaddset(&mut blocked, libc::SIGTTOU);
                    if libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()) != 0 {
                        return;
                    }
                }
                for message in messages {
                    if output.write_all(message.as_bytes()).is_err() {
                        break;
                    }
                }
                let _ = completed.send(());
            })
            .is_ok()
        {
            diagnostic.sender = Some(sender);
            diagnostic.drained = Some(drained);
        }
        diagnostic
    }

    fn emit(&self, message: String) {
        if let Some(sender) = &self.sender {
            // A decision may be delivered after unterminated child stderr. Keep
            // every message on its own line without waiting for the child writer.
            let _ = sender.send(format!("\ncacheexec: verbose: {message}\n"));
        }
    }

    pub fn decision(
        &self,
        decision: Decision,
        age: Option<Result<Duration, std::time::SystemTimeError>>,
        ttl: Duration,
        directory: &Path,
        key: &str,
    ) {
        if self.sender.is_none() {
            return;
        }
        let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
        self.emit(message::decision(decision, age, ttl, &directory, key));
    }

    pub fn finish(&self, message: String) {
        if !self.finished.replace(true) {
            self.emit(message);
        }
    }

    pub fn failed(&self, saved: Saved) {
        self.finish(message::failed(saved, Failure::Execution));
    }
}

impl Drop for Verbose {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(drained) = &self.drained {
            // Never join a writer blocked on a diagnostic consumer. This bounded
            // grace period runs after sharing::run has released all its locks.
            let _ = drained.recv_timeout(Duration::from_millis(20));
        }
    }
}

fn safe_destination(output: &File) -> bool {
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::fstat(output.as_raw_fd(), &mut stat) != 0 {
            return false;
        }
        stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || (libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) == 0
                && limit.rlim_cur == libc::RLIM_INFINITY)
    }
}
