use crate::{
    Cli, cache, runner, signals,
    verbose::{self, Verbose},
};
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
fn vote(file: &mut File, cli: &Cli) -> Result<()> {
    file.rewind()?;
    let mut votes = [0; 256];
    file.read_exact(&mut votes)
        .context("read active execution policies")?;
    for (code, vote) in votes.iter_mut().enumerate() {
        *vote |= u8::from(cli.allows(code as i32));
    }
    file.rewind()?;
    file.write_all(&votes)?;
    Ok(())
}
pub fn run(
    cli: &Cli,
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
        diagnostic.finish(format!(
            "interrupted exit={} saved=unknown reason=interrupted",
            128 + signals::received()
        ));
        return Ok(128 + signals::received());
    }
    let active_path = directory.join(format!("{key}.active"));
    match OpenOptions::new().read(true).write(true).open(&active_path) {
        Ok(mut active) => {
            if !try_lock(&active, true)? {
                vote(&mut active, cli)?;
                unlock(&gate)?;
                diagnostic.decision("join", None, cli, directory, key);
                if !lock(&active, true)? {
                    diagnostic.finish(format!(
                        "interrupted exit={} saved=unknown reason=waiter-interrupted",
                        128 + signals::received()
                    ));
                    return Ok(128 + signals::received());
                }
                active.seek(SeekFrom::Start(256))?;
                let mut bytes = Vec::new();
                active.read_to_end(&mut bytes)?;
                unlock(&active)?;
                match bytes.split_first() {
                    Some((0, bytes)) => {
                        let record = cache::decode(bytes)?;
                        return replay(record, "unknown", false, diagnostic);
                    }
                    Some((3, bytes)) => {
                        let (saved, bytes) = bytes
                            .split_first()
                            .context("missing shared saving status")?;
                        let (saving, interrupted) = match saved {
                            1 => ("yes", false),
                            2 => ("no reason=participant-policy", false),
                            3 => ("no reason=interrupted", true),
                            _ => bail!("invalid shared saving status"),
                        };
                        return replay(cache::decode(bytes)?, saving, interrupted, diagnostic);
                    }
                    Some((4, bytes)) => {
                        let (invalidated, bytes) = bytes
                            .split_first()
                            .context("missing shared failure status")?;
                        diagnostic.failed(if *invalidated == 1 { "no" } else { "unknown" });
                        bail!(
                            "shared execution failed: {}",
                            String::from_utf8_lossy(bytes)
                        );
                    }
                    Some((1, bytes)) => bail!(
                        "shared execution failed: {}",
                        String::from_utf8_lossy(bytes)
                    ),
                    _ => bail!(
                        "execution owner disappeared before publishing a complete result; command not retried"
                    ),
                }
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
    let reason = verbose::reason(cli, previous.as_ref(), now);
    if reason.is_none() {
        unlock(&gate)?;
        diagnostic.decision("hit", age, cli, directory, key);
        return replay(
            previous.expect("hit requires a result"),
            "no reason=reused",
            false,
            diagnostic,
        );
    }
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
    active.write_all(&[2])?;
    vote(&mut active, cli)?;
    unlock(&gate)?;
    diagnostic.decision(
        &format!("run reason={}", reason.expect("run requires a reason")),
        age,
        cli,
        directory,
        key,
    );
    let execution = runner::execute(&cli.command);
    lock(&gate, false)?;
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
        fs::remove_file(&active_path).context("remove completed execution marker")?;
        let signal = signals::seal_execution();
        if apply_signal(&mut execution, result_path, signal)? {
            stage(&mut active, &execution, &votes)?;
        }
        // Only this final byte publishes success. Shared data is temporary, so the
        // commit byte needs process visibility, not crash durability.
        active.seek(SeekFrom::Start(256))?;
        active.write_all(&[3]).context("commit shared result")?;
        let saved = saving_status(&execution, &votes);
        Ok((execution.code, saved, execution.delivery))
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
        active.seek(SeekFrom::Start(257))?;
        active.set_len(257)?;
        active.write_all(&[u8::from(invalidated)])?;
        active
            .write_all(message.as_bytes())
            .with_context(|| message.clone())?;
        active.seek(SeekFrom::Start(256))?;
        active.write_all(&[4]).with_context(|| message.clone())?;
        let _ = fs::remove_file(&active_path);
    }
    unlock(&active)?;
    unlock(&gate)?;
    outcome.and_then(|(code, saved, delivery)| {
        let (kind, saving) = match saved {
            1 => ("completed", "yes"),
            2 => ("completed", "no reason=participant-policy"),
            _ => ("interrupted", "no reason=interrupted"),
        };
        match delivery.finish(code) {
            Ok(code) => {
                let kind = if signals::received() != 0 {
                    "interrupted"
                } else {
                    kind
                };
                diagnostic.finish(format!("{kind} exit={code} saved={saving}"));
                Ok(code)
            }
            Err(error) => {
                diagnostic.finish(format!("failed saved={saving} reason=delivery-failure"));
                Err(error)
            }
        }
    })
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

fn saving_status(execution: &runner::Execution, votes: &[u8; 256]) -> u8 {
    if !execution.reusable {
        3
    } else if votes[execution.code as usize] != 0 {
        1
    } else {
        2
    }
}

fn stage(active: &mut File, execution: &runner::Execution, votes: &[u8; 256]) -> Result<()> {
    active.seek(SeekFrom::Start(257))?;
    active.set_len(257)?;
    active.write_all(&[saving_status(execution, votes)])?;
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
