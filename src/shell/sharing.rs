use crate::{
    domain::{
        delivery,
        execution::{self, CODES, Outcome, Saving, Votes, signal_code},
        policy::Request,
        record::{self, Record},
    },
    shell::{
        lock::{acquire_gate, lock, try_lock, unlock},
        replay, runner, signals, store,
        verbose::Verbose,
    },
};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::SystemTime,
};

// Layout of a `.active` file: one save-permission byte per exit code, then a
// state tag, a detail byte, and the encoded record or failure message.
const TAG_OFFSET: u64 = 256;
const DETAIL_OFFSET: u64 = 257;
const PENDING: u8 = 2;
const COMPLETED: u8 = 3;
const FAILED: u8 = 4;

fn read_votes(file: &mut File) -> std::io::Result<Votes> {
    let mut votes = [0; CODES];
    file.read_exact(&mut votes)?;
    Ok(votes)
}

fn vote(file: &mut File, request: &Request) -> Result<()> {
    file.rewind()?;
    let mut votes = read_votes(file).context("read active execution policies")?;
    execution::add_vote(&mut votes, request);
    file.rewind()?;
    file.write_all(&votes)?;
    Ok(())
}

fn interrupted(diagnostic: &Verbose, reason: &str) -> i32 {
    let code = signal_code(signals::received());
    diagnostic.finish(format!(
        "interrupted exit={code} saved=unknown reason={reason}"
    ));
    code
}

// Lock order: a caller holding the gate only try-locks `.active`, and the owner
// holding `.active` blocks on the gate, so the two locks cannot deadlock. The
// owner may relock its gate descriptor after execution because cleanup never
// unlinks `.lock` while `.active` is locked.
pub fn run(
    request: &Request,
    directory: &Path,
    key: &str,
    result_path: &Path,
    diagnostic: &Verbose,
) -> Result<i32> {
    let Some(gate) = acquire_gate(&directory.join(format!("{key}.lock")), true)? else {
        return Ok(interrupted(diagnostic, "interrupted"));
    };
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
    let previous = store::load(result_path)?;
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
    store::invalidate(result_path)?;
    let mut active = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&active_path)
        .with_context(|| format!("create active execution {active_path:?}"))?;
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
            let (saving, encoded) = detail
                .split_first()
                .context("missing shared saving status")?;
            let saving = Saving::decode(*saving)?;
            replay(
                record::decode(encoded)?,
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
    let child_context = execution.as_ref().ok().map(|execution| {
        format!(
            "child already completed with exit code {}",
            execution.outcome.code()
        )
    });
    let outcome = execution.and_then(|mut execution| {
        active.rewind()?;
        let votes = read_votes(&mut active)?;
        let child = &mut execution.outcome;
        apply_interrupt(child, result_path)?;
        if child.savable(&votes) {
            store::save(result_path, child.record())
                .with_context(|| format!("could not save result {result_path:?}"))?;
        }
        apply_interrupt(child, result_path)?;
        stage(&mut active, child, &votes)?;
        if apply_interrupt(child, result_path)? {
            stage(&mut active, child, &votes)?;
        }
        fs::remove_file(active_path).context("remove completed execution marker")?;
        let signal = signals::seal_execution();
        if apply_signal(child, result_path, signal)? {
            stage(&mut active, child, &votes)?;
        }
        // Only this final byte publishes success. Shared data is temporary, so the
        // commit byte needs process visibility, not crash durability.
        active.seek(SeekFrom::Start(TAG_OFFSET))?;
        active
            .write_all(&[COMPLETED])
            .context("commit shared result")?;
        let saving = Saving::of(child, &votes);
        Ok((child.code(), saving, execution.delivery))
    });
    let outcome = match child_context {
        Some(context) => outcome.context(context),
        None => outcome,
    };
    if let Err(error) = &outcome {
        let invalidation = store::invalidate(result_path);
        let invalidated = invalidation.is_ok();
        diagnostic.failed(if invalidated { "no" } else { "unknown" });
        let message = match invalidation {
            Ok(()) => format!("{error:#}"),
            Err(cleanup) => format!("{error:#}; could not invalidate result: {cleanup:#}"),
        };
        publish_failure(&mut active, invalidated, &message)?;
        let _ = fs::remove_file(active_path);
    }
    unlock(&active)?;
    unlock(gate)?;
    let (code, saving, delivery) = outcome?;
    report(
        delivery.finish(code),
        code,
        saving.describe(),
        saving == Saving::Interrupted,
        "delivery-failure",
        diagnostic,
    )
}

fn replay(record: Record, saving: &str, interrupted: bool, diagnostic: &Verbose) -> Result<i32> {
    let code = record.code;
    report(
        replay::write(record),
        code,
        saving,
        interrupted,
        "replay-failure",
        diagnostic,
    )
}

fn report(
    outcome: Result<i32>,
    command_code: i32,
    saving: &str,
    generation_interrupted: bool,
    failure: &str,
    diagnostic: &Verbose,
) -> Result<i32> {
    // Sampled once so the exit status and its label cannot disagree.
    let signal = signals::received();
    match delivery::classify(outcome, command_code, signal, generation_interrupted) {
        Ok((code, kind)) => {
            diagnostic.finish(format!("{kind} exit={code} saved={saving}"));
            Ok(code)
        }
        Err(error) => {
            diagnostic.finish(format!("failed saved={saving} reason={failure}"));
            Err(error)
        }
    }
}

/// Publishes a failed execution to waiters. An error is useful even if storage
/// cannot be synchronized. If publishing fails, the original failure is kept
/// in the error, and the tag stays pending, never a successful result.
fn publish_failure(active: &mut File, invalidated: bool, message: &str) -> Result<()> {
    (|| -> std::io::Result<()> {
        active.seek(SeekFrom::Start(DETAIL_OFFSET))?;
        active.set_len(DETAIL_OFFSET)?;
        active.write_all(&[u8::from(invalidated)])?;
        active.write_all(message.as_bytes())?;
        active.seek(SeekFrom::Start(TAG_OFFSET))?;
        active.write_all(&[FAILED])
    })()
    .with_context(|| message.to_owned())
}

fn stage(active: &mut File, outcome: &Outcome, votes: &Votes) -> Result<()> {
    active.seek(SeekFrom::Start(DETAIL_OFFSET))?;
    active.set_len(DETAIL_OFFSET)?;
    active.write_all(&[Saving::of(outcome, votes) as u8])?;
    active
        .write_all(&record::encode(outcome.record())?)
        .context("write shared result")
}

fn apply_interrupt(outcome: &mut Outcome, path: &Path) -> Result<bool> {
    apply_signal(outcome, path, signals::received())
}

fn apply_signal(outcome: &mut Outcome, path: &Path, signal: i32) -> Result<bool> {
    if signal == 0 {
        return Ok(false);
    }
    let changed = outcome.interrupt(signal);
    store::invalidate(path)?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_failure_that_cannot_be_published_keeps_its_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.active");
        let mut active = create(&path);
        active.write_all(&[0; DETAIL_OFFSET as usize]).unwrap();
        // Every write fails, including the first one.
        let mut read_only = File::open(&path).unwrap();
        let message = "child already completed with exit code 7; could not save result";
        let error = publish_failure(&mut read_only, true, message).unwrap_err();
        assert!(format!("{error:#}").starts_with(message), "{error:#}");
        assert_eq!(fs::read(&path).unwrap(), [0; DETAIL_OFFSET as usize]);

        publish_failure(&mut active, true, message).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes[TAG_OFFSET as usize], FAILED);
        assert_eq!(bytes[DETAIL_OFFSET as usize], 1);
        assert_eq!(&bytes[DETAIL_OFFSET as usize + 1..], message.as_bytes());
    }
}
