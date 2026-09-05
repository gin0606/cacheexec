use std::{
    cell::Cell,
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, SystemTime},
};

use crate::{Cli, cache::Record};

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
        action: &str,
        age: Option<Result<Duration, std::time::SystemTimeError>>,
        cli: &Cli,
        directory: &Path,
        key: &str,
    ) {
        if self.sender.is_none() {
            return;
        }
        let age = match age {
            Some(Ok(age)) => format!(" age={}s", age.as_secs_f64()),
            Some(Err(future)) => format!(" age=-{}s", future.duration().as_secs_f64()),
            None => String::new(),
        };
        let directory = std::path::absolute(directory).unwrap_or_else(|_| directory.to_path_buf());
        self.emit(format!(
            "{action}{age} ttl={} key={key} cache-dir={directory:?}",
            humantime::format_duration(cli.ttl.expect("execution requires TTL")),
        ));
    }

    pub fn finish(&self, message: String) {
        if !self.finished.replace(true) {
            self.emit(message);
        }
    }

    pub fn failed(&self, saved: &str) {
        self.finish(format!("failed saved={saved} reason=failure"));
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

pub fn reason(cli: &Cli, record: Option<&Record>, now: SystemTime) -> Option<&'static str> {
    if cli.refresh {
        return Some("refresh");
    }
    let Some(record) = record else {
        return Some("missing");
    };
    if now.duration_since(record.completed).is_err() {
        return Some("future-timestamp");
    }
    if !record.fresh(cli.ttl.expect("execution requires TTL"), now) {
        return Some("expired");
    }
    if !cli.allows(record.code) {
        return Some("policy");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::time::UNIX_EPOCH;

    #[test]
    fn reason_priority_and_ttl_boundary() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        for refresh in [false, true] {
            for missing in [false, true] {
                for future in [false, true] {
                    for expired in [false, true] {
                        for excluded in [false, true] {
                            let mut cli =
                                Cli::parse_from(["cacheexec", "--ttl", "5s", "--", "true"]);
                            cli.refresh = refresh;
                            cli.exclude_codes = excluded.then_some(vec![0]);
                            let record = Record {
                                completed: if future {
                                    now + Duration::from_secs(1)
                                } else if expired {
                                    now - Duration::from_secs(6)
                                } else {
                                    now - Duration::from_secs(5)
                                },
                                code: 0,
                                stdout: vec![],
                                stderr: vec![],
                            };
                            let expected = if refresh {
                                Some("refresh")
                            } else if missing {
                                Some("missing")
                            } else if future {
                                Some("future-timestamp")
                            } else if expired {
                                Some("expired")
                            } else if excluded {
                                Some("policy")
                            } else {
                                None
                            };
                            assert_eq!(reason(&cli, (!missing).then_some(&record), now), expected);
                        }
                    }
                }
            }
        }
    }
}
