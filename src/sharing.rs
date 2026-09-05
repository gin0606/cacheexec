use crate::{Cli, cache, runner, signals};
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
pub fn run(cli: &Cli, directory: &Path, key: &str, result_path: &Path) -> Result<i32> {
    let gate = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{key}.lock")))
        .context("open key lock")?;
    if !lock(&gate, true)? {
        return Ok(128 + signals::received());
    }
    let active_path = directory.join(format!("{key}.active"));
    match OpenOptions::new().read(true).write(true).open(&active_path) {
        Ok(mut active) => {
            if !try_lock(&active, true)? {
                vote(&mut active, cli)?;
                unlock(&gate)?;
                if !lock(&active, true)? {
                    return Ok(128 + signals::received());
                }
                active.seek(SeekFrom::Start(256))?;
                let mut bytes = Vec::new();
                active.read_to_end(&mut bytes)?;
                unlock(&active)?;
                match bytes.split_first() {
                    Some((0, bytes)) => {
                        let record = cache::decode(bytes)?;
                        return record.replay();
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
    if !cli.refresh {
        if let Some(record) = previous {
            if record.fresh(cli.ttl.expect("execution requires TTL"), SystemTime::now())
                && cli.allows(record.code)
            {
                unlock(&gate)?;
                return record.replay();
            }
        }
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
    let execution = runner::execute(&cli.command);
    lock(&gate, false)?;
    let child_context = match &execution {
        Ok(execution) => format!("child already completed with exit code {}", execution.code),
        Err(error) => format!("execution failed: {error:#}"),
    };
    let outcome = execution
        .and_then(|mut execution| {
            active.rewind()?;
            let mut votes = [0; 256];
            active.read_exact(&mut votes)?;
            apply_interrupt(&mut execution, result_path)?;
            if execution.reusable && votes[execution.code as usize] != 0 {
                cache::save(result_path, &execution.record).context("could not save result")?;
            }
            apply_interrupt(&mut execution, result_path)?;
            stage(&mut active, &cache::encode(&execution.record)?)?;
            if apply_interrupt(&mut execution, result_path)? {
                stage(&mut active, &cache::encode(&execution.record)?)?;
            }
            fs::remove_file(&active_path).context("remove completed execution marker")?;
            if apply_interrupt(&mut execution, result_path)? {
                stage(&mut active, &cache::encode(&execution.record)?)?;
            }
            // Only this final byte publishes success. Shared data is temporary, so the
            // commit byte needs process visibility, not crash durability.
            active.seek(SeekFrom::Start(256))?;
            active.write_all(&[0]).context("commit shared result")?;
            Ok(execution.code)
        })
        .with_context(|| child_context);
    if let Err(error) = &outcome {
        let invalidation = cache::invalidate(result_path);
        let message = match invalidation {
            Ok(()) => format!("{error:#}"),
            Err(cleanup) => format!("{error:#}; could not invalidate result: {cleanup:#}"),
        };
        // An error is useful even if storage cannot be synchronized. A failed
        // error write leaves the pending tag, never a successful result.
        active.seek(SeekFrom::Start(257))?;
        active.set_len(257)?;
        active
            .write_all(message.as_bytes())
            .with_context(|| message.clone())?;
        active.seek(SeekFrom::Start(256))?;
        active.write_all(&[1]).with_context(|| message.clone())?;
        let _ = fs::remove_file(&active_path);
    }
    unlock(&active)?;
    unlock(&gate)?;
    outcome
}

fn stage(active: &mut File, bytes: &[u8]) -> Result<()> {
    active.seek(SeekFrom::Start(257))?;
    active.set_len(257)?;
    active.write_all(bytes).context("write shared result")?;
    active.sync_all().context("sync shared result")
}

fn apply_interrupt(execution: &mut runner::Execution, path: &Path) -> Result<bool> {
    let signal = signals::received();
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
