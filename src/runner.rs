use crate::cache::Record;
use anyhow::{Context, Result, anyhow};
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    process::{Command, Stdio},
    thread,
    time::SystemTime,
};

pub struct Execution {
    pub code: i32,
    pub record: Option<Record>,
}

fn copy(mut input: impl Read, mut output: impl Write) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 16384];
    let mut failure = None;
    loop {
        let count = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        bytes.extend_from_slice(&buffer[..count]);
        // Keep draining after output failure so the child cannot block on a full pipe.
        if failure.is_none() {
            if let Err(error) = output
                .write_all(&buffer[..count])
                .and_then(|()| output.flush())
            {
                failure = Some(error);
            }
        }
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(bytes)
}

pub fn execute(argv: &[OsString]) -> Result<Execution> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {:?}", argv[0]))?;
    let stdout = child.stdout.take().context("missing child stdout pipe")?;
    let stderr = child.stderr.take().context("missing child stderr pipe")?;
    let (status, completed, out, err) = thread::scope(|scope| {
        let out = scope.spawn(move || copy(stdout, std::io::stdout().lock()));
        let err = scope.spawn(move || copy(stderr, std::io::stderr().lock()));
        let status = child.wait();
        let completed = SystemTime::now();
        (status, completed, out.join(), err.join())
    });
    let status = status.context("wait for child")?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
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
    let record = status.code().map(|code| Record {
        completed,
        code,
        stdout,
        stderr,
    });
    Ok(Execution { code, record })
}
