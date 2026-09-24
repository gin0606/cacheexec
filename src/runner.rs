use crate::cache::Record;
use anyhow::{Context, Result, anyhow};
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

pub struct Execution {
    pub code: i32,
    pub record: Record,
    pub reusable: bool,
    pub delivery: Delivery,
}

fn capture(mut input: impl Read, output: mpsc::Sender<Vec<u8>>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 16384];
    loop {
        let count = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        bytes.extend_from_slice(&buffer[..count]);
        // A failed consumer must not stop draining the child's pipe.
        let _ = output.send(buffer[..count].to_vec());
    }
    Ok(bytes)
}
fn stream(input: mpsc::Receiver<Vec<u8>>, mut output: impl Write) -> Result<()> {
    for bytes in input {
        output.write_all(&bytes)?;
        output.flush()?;
    }
    Ok(())
}

pub fn execute(argv: &[OsString]) -> Result<Execution> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {:?}", argv[0]))?;
    let stdout = child.stdout.take().context("missing child stdout pipe")?;
    let stderr = child.stderr.take().context("missing child stderr pipe")?;
    let (out_sender, out_receiver) = mpsc::channel();
    let (err_sender, err_receiver) = mpsc::channel();
    let out_writer = thread::spawn(move || stream(out_receiver, std::io::stdout().lock()));
    let err_writer = thread::spawn(move || stream(err_receiver, std::io::stderr().lock()));
    let (status, completed, out, err) = thread::scope(|scope| {
        let out = scope.spawn(move || capture(stdout, out_sender));
        let err = scope.spawn(move || capture(stderr, err_sender));
        let mut completed = None;
        let status = loop {
            let signal = crate::signals::take_pending();
            if signal != 0 {
                // The child is not reaped until its pipes close, keeping its group ID reserved.
                unsafe {
                    libc::kill(-(child.id() as i32), signal);
                }
            }
            if completed.is_none() {
                match exited_without_reaping(child.id()) {
                    Ok(true) => completed = Some(SystemTime::now()),
                    Ok(false) => {}
                    Err(error) => break Err(error),
                }
            }
            if completed.is_some() && out.is_finished() && err.is_finished() {
                break child.wait();
            }
            thread::sleep(Duration::from_millis(10));
        };
        (status, completed, out.join(), err.join())
    });
    let status = status.context("wait for child")?;
    let interrupted = crate::signals::received();
    let code = if interrupted != 0 {
        128 + interrupted
    } else {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };
    let context = || {
        format!(
            "child already completed with {}; output transfer failed",
            status
        )
    };
    let stdout = out
        .map_err(|_| anyhow!("stdout worker panicked"))
        .and_then(|v| v)
        .with_context(context)?;
    let stderr = err
        .map_err(|_| anyhow!("stderr worker panicked"))
        .and_then(|v| v)
        .with_context(context)?;
    let reusable = status.code().is_some() && interrupted == 0;
    let record = Record {
        completed: completed.context("missing child completion time")?,
        code,
        stdout,
        stderr,
    };
    Ok(Execution {
        code,
        record,
        reusable,
        delivery: Delivery([out_writer, err_writer]),
    })
}

pub struct Delivery([thread::JoinHandle<Result<()>>; 2]);

impl Delivery {
    pub fn finish(self, code: i32) -> Result<i32> {
        while crate::signals::received() == 0 && !self.0.iter().all(|writer| writer.is_finished()) {
            thread::sleep(Duration::from_millis(10));
        }
        let signal = crate::signals::received();
        if signal != 0 {
            return Ok(128 + signal);
        }
        let [out, err] = self.0.map(|writer| {
            writer
                .join()
                .map_err(|_| anyhow!("output writer panicked"))
                .and_then(|result| result)
                .with_context(|| {
                    format!("child already completed with exit code {code}; output transfer failed")
                })
        });
        delivery_result(out, err).map(|()| code)
    }
}

/// Combines the stdout and stderr delivery results. A reader that closed
/// early (EPIPE) on one stream must not hide a different failure on the other.
pub fn delivery_result(out: Result<()>, err: Result<()>) -> Result<()> {
    match (out, err) {
        (Err(out), Err(err)) if output_closed(&out) && !output_closed(&err) => Err(err),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub fn output_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(reader_closed)
    })
}

/// Whether a write failed because its reader closed early, which callers treat
/// as the reader's choice to stop rather than a tool failure.
pub fn reader_closed(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::BrokenPipe
}

fn exited_without_reaping(pid: u32) -> std::io::Result<bool> {
    // WNOWAIT records exit promptly for TTL while reserving the process-group ID
    // until output draining and signal forwarding have both finished.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(info.si_pid() != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn failed(kind: ErrorKind) -> Result<()> {
        Err(anyhow::Error::from(Error::from(kind)).context("write"))
    }

    #[test]
    fn a_closed_reader_never_hides_another_failure() {
        let closed = || failed(ErrorKind::BrokenPipe);
        let other = || failed(ErrorKind::Other);
        let is_closed = |result: Result<()>| output_closed(&result.unwrap_err());
        assert!(delivery_result(Ok(()), Ok(())).is_ok());
        assert!(is_closed(delivery_result(closed(), Ok(()))));
        assert!(is_closed(delivery_result(Ok(()), closed())));
        assert!(is_closed(delivery_result(closed(), closed())));
        assert!(!is_closed(delivery_result(closed(), other())));
        assert!(!is_closed(delivery_result(other(), closed())));
        assert!(!is_closed(delivery_result(Ok(()), other())));
    }
}
