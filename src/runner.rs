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
        for writer in self.0 {
            writer
                .join()
                .map_err(|_| anyhow!("output writer panicked"))?
                .with_context(|| {
                    format!("child already completed with exit code {code}; output transfer failed")
                })?;
        }
        Ok(code)
    }
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
