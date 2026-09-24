use crate::{cache, request::Request, runner, signals, verbose::Verbose};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    thread,
    time::{Duration, SystemTime},
};

pub fn try_lock(file: &File, exclusive: bool) -> Result<bool> {
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error).context("cache lock failed")
}
fn lock(file: &File, interruptible: bool) -> Result<bool> {
    loop {
        if interruptible && signals::received() != 0 {
            return Ok(false);
        }
        if try_lock(file, true)? {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn unlock(file: &File) -> Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        return Err(std::io::Error::last_os_error()).context("release cache lock");
    }
    Ok(())
}
// Layout of a `.active` file: one save-permission byte per exit code, then a
// state tag, a detail byte, and the encoded record or failure message.
const TAG_OFFSET: u64 = 256;
const DETAIL_OFFSET: u64 = 257;
const PENDING: u8 = 2;
const COMPLETED: u8 = 3;
const FAILED: u8 = 4;

#[derive(Clone, Copy, PartialEq)]
enum Saving {
    Saved = 1,
    Excluded = 2,
    Interrupted = 3,
}

impl Saving {
    fn of(execution: &runner::Execution, votes: &[u8; 256]) -> Self {
        if !execution.reusable {
            Self::Interrupted
        } else if votes[execution.code as usize] != 0 {
            Self::Saved
        } else {
            Self::Excluded
        }
    }
    fn decode(byte: u8) -> Result<Self> {
        Ok(match byte {
            1 => Self::Saved,
            2 => Self::Excluded,
            3 => Self::Interrupted,
            _ => bail!("invalid shared saving status"),
        })
    }
    fn describe(self) -> &'static str {
        match self {
            Self::Saved => "yes",
            Self::Excluded => "no reason=participant-policy",
            Self::Interrupted => "no reason=interrupted",
        }
    }
}

fn vote(file: &mut File, request: &Request) -> Result<()> {
    file.rewind()?;
    let mut votes = [0; 256];
    file.read_exact(&mut votes)
        .context("read active execution policies")?;
    for (code, vote) in votes.iter_mut().enumerate() {
        *vote |= u8::from(request.allows(code as i32));
    }
    file.rewind()?;
    file.write_all(&votes)?;
    Ok(())
}

fn interrupted(diagnostic: &Verbose, reason: &str) -> i32 {
    let code = 128 + signals::received();
    diagnostic.finish(format!(
        "interrupted exit={code} saved=unknown reason={reason}"
    ));
    code
}

// Lock order: a caller holding the gate only try-locks `.active`, and the owner
// holding `.active` blocks on the gate, so the two locks cannot deadlock.
pub fn run(
    request: &Request,
    directory: &Path,
    key: &str,
    result_path: &Path,
    diagnostic: &Verbose,
) -> Result<i32> {
    let gate = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{key}.lock")))
        .context("open key lock")?;
    if !lock(&gate, true)? {
        return Ok(interrupted(diagnostic, "interrupted"));
    }
    let active_path = directory.join(format!("{key}.active"));
    match OpenOptions::new().read(true).write(true).open(&active_path) {
        Ok(mut active) => {
            if !try_lock(&active, true)? {
                vote(&mut active, request)?;
                unlock(&gate)?;
                diagnostic.decision("join", None, request.ttl, directory, key);
                return join(active, diagnostic);
            }
            // An open descriptor identifies this generation even after its name is reused.
            fs::remove_file(&active_path).context("remove abandoned execution")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("open active execution"),
    }
    let previous = cache::load(result_path)?;
    let now = SystemTime::now();
    let age = previous
        .as_ref()
        .map(|record| now.duration_since(record.completed));
    let Some(reason) = request.reason(previous.as_ref(), now) else {
        unlock(&gate)?;
        diagnostic.decision("hit", age, request.ttl, directory, key);
        return replay(
            previous.expect("hit requires a result"),
            "no reason=reused",
            false,
            diagnostic,
        );
    };
    cache::invalidate(result_path)?;
    let mut active = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&active_path)?;
    if !try_lock(&active, true)? {
        bail!("new execution unexpectedly locked");
    }
    active.write_all(&[0; 256])?;
    active.write_all(&[PENDING])?;
    vote(&mut active, request)?;
    unlock(&gate)?;
    diagnostic.decision(
        &format!("run reason={reason}"),
        age,
        request.ttl,
        directory,
        key,
    );
    own(
        request,
        &gate,
        active,
        &active_path,
        result_path,
        diagnostic,
    )
}

fn join(mut active: File, diagnostic: &Verbose) -> Result<i32> {
    if !lock(&active, true)? {
        return Ok(interrupted(diagnostic, "waiter-interrupted"));
    }
    active.seek(SeekFrom::Start(TAG_OFFSET))?;
    let mut bytes = Vec::new();
    active.read_to_end(&mut bytes)?;
    unlock(&active)?;
    match bytes.split_first() {
        Some((&COMPLETED, detail)) => {
            let (saving, record) = detail
                .split_first()
                .context("missing shared saving status")?;
            let saving = Saving::decode(*saving)?;
            replay(
                cache::decode(record)?,
                saving.describe(),
                saving == Saving::Interrupted,
                diagnostic,
            )
        }
        Some((&FAILED, detail)) => {
            let (invalidated, message) = detail
                .split_first()
                .context("missing shared failure status")?;
            diagnostic.failed(if *invalidated == 1 { "no" } else { "unknown" });
            bail!(
                "shared execution failed: {}",
                String::from_utf8_lossy(message)
            );
        }
        _ => bail!(
            "execution owner disappeared before publishing a complete result; command not retried"
        ),
    }
}

fn own(
    request: &Request,
    gate: &File,
    mut active: File,
    active_path: &Path,
    result_path: &Path,
    diagnostic: &Verbose,
) -> Result<i32> {
    let execution = runner::execute(&request.command);
    lock(gate, false)?;
    let child_context = execution
        .as_ref()
        .ok()
        .map(|execution| format!("child already completed with exit code {}", execution.code));
    let outcome = execution.and_then(|mut execution| {
        active.rewind()?;
        let mut votes = [0; 256];
        active.read_exact(&mut votes)?;
        apply_interrupt(&mut execution, result_path)?;
        if execution.reusable && votes[execution.code as usize] != 0 {
            cache::save(result_path, &execution.record)
                .with_context(|| format!("could not save result {result_path:?}"))?;
        }
        apply_interrupt(&mut execution, result_path)?;
        stage(&mut active, &execution, &votes)?;
        if apply_interrupt(&mut execution, result_path)? {
            stage(&mut active, &execution, &votes)?;
        }
        fs::remove_file(active_path).context("remove completed execution marker")?;
        let signal = signals::seal_execution();
        if apply_signal(&mut execution, result_path, signal)? {
            stage(&mut active, &execution, &votes)?;
        }
        // Only this final byte publishes success. Shared data is temporary, so the
        // commit byte needs process visibility, not crash durability.
        active.seek(SeekFrom::Start(TAG_OFFSET))?;
        active
            .write_all(&[COMPLETED])
            .context("commit shared result")?;
        let saving = Saving::of(&execution, &votes);
        Ok((execution.code, saving, execution.delivery))
    });
    let outcome = match child_context {
        Some(context) => outcome.context(context),
        None => outcome,
    };
    if let Err(error) = &outcome {
        let invalidation = cache::invalidate(result_path);
        let invalidated = invalidation.is_ok();
        diagnostic.failed(if invalidated { "no" } else { "unknown" });
        let message = match invalidation {
            Ok(()) => format!("{error:#}"),
            Err(cleanup) => format!("{error:#}; could not invalidate result: {cleanup:#}"),
        };
        // An error is useful even if storage cannot be synchronized. A failed
        // error write leaves the pending tag, never a successful result.
        active.seek(SeekFrom::Start(DETAIL_OFFSET))?;
        active.set_len(DETAIL_OFFSET)?;
        active.write_all(&[u8::from(invalidated)])?;
        active
            .write_all(message.as_bytes())
            .with_context(|| message.clone())?;
        active.seek(SeekFrom::Start(TAG_OFFSET))?;
        active
            .write_all(&[FAILED])
            .with_context(|| message.clone())?;
        let _ = fs::remove_file(active_path);
    }
    unlock(&active)?;
    unlock(gate)?;
    let (code, saving, delivery) = outcome?;
    match delivery.finish(code) {
        Ok(code) => {
            let kind = if saving == Saving::Interrupted || signals::received() != 0 {
                "interrupted"
            } else {
                "completed"
            };
            diagnostic.finish(format!("{kind} exit={code} saved={}", saving.describe()));
            Ok(code)
        }
        Err(error) => {
            diagnostic.finish(format!(
                "failed saved={} reason=delivery-failure",
                saving.describe()
            ));
            Err(error)
        }
    }
}

fn replay(
    record: cache::Record,
    saving: &str,
    interrupted: bool,
    diagnostic: &Verbose,
) -> Result<i32> {
    let result = record.replay();
    match &result {
        Ok(code) => {
            let kind = if interrupted || signals::received() != 0 {
                "interrupted"
            } else {
                "completed"
            };
            diagnostic.finish(format!("{kind} exit={code} saved={saving}"));
        }
        Err(_) => diagnostic.finish(format!("failed saved={saving} reason=replay-failure")),
    }
    result
}

fn stage(active: &mut File, execution: &runner::Execution, votes: &[u8; 256]) -> Result<()> {
    active.seek(SeekFrom::Start(DETAIL_OFFSET))?;
    active.set_len(DETAIL_OFFSET)?;
    active.write_all(&[Saving::of(execution, votes) as u8])?;
    active
        .write_all(&cache::encode(&execution.record)?)
        .context("write shared result")?;
    active.sync_all().context("sync shared result")
}

fn apply_interrupt(execution: &mut runner::Execution, path: &Path) -> Result<bool> {
    apply_signal(execution, path, signals::received())
}

fn apply_signal(execution: &mut runner::Execution, path: &Path, signal: i32) -> Result<bool> {
    if signal == 0 {
        return Ok(false);
    }
    let changed = execution.code != 128 + signal || execution.reusable;
    execution.code = 128 + signal;
    execution.record.code = execution.code;
    execution.reusable = false;
    cache::invalidate(path)?;
    Ok(changed)
}
