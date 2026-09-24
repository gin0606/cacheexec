use crate::{
    domain::{
        delivery,
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
            store::save(result_path, &execution.record)
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
    match classify(outcome, command_code, signal, generation_interrupted) {
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

/// The exit status and completion label of a delivery or replay outcome.
fn classify(
    outcome: Result<i32>,
    command_code: i32,
    signal: i32,
    generation_interrupted: bool,
) -> Result<(i32, &'static str)> {
    let (code, kind) = match outcome {
        // A signal received during delivery ends it with 128 + signal however
        // delivery ended, as Delivery::finish and Record::replay do.
        _ if signal != 0 => (128 + signal, "interrupted"),
        Ok(code) => (code, "completed"),
        // A consumer such as `head` closing its end is a normal way to stop
        // reading, not a tool failure. The status of the delivered result is
        // still known, so it is reported; shared and saved results stay intact.
        Err(error) if delivery::output_closed(&error) => (command_code, "output-closed"),
        Err(error) => return Err(error),
    };
    Ok((
        code,
        if generation_interrupted {
            "interrupted"
        } else {
            kind
        },
    ))
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

fn stage(active: &mut File, execution: &runner::Execution, votes: &[u8; 256]) -> Result<()> {
    active.seek(SeekFrom::Start(DETAIL_OFFSET))?;
    active.set_len(DETAIL_OFFSET)?;
    active.write_all(&[Saving::of(execution, votes) as u8])?;
    active
        .write_all(&record::encode(&execution.record)?)
        .context("write shared result")
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
    store::invalidate(path)?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed() -> anyhow::Error {
        anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            .context("replay stdout")
    }

    #[test]
    fn a_signal_during_delivery_wins_over_every_outcome() {
        for outcome in [Ok(3), Err(closed()), Err(anyhow::anyhow!("disk full"))] {
            let classified = classify(outcome, 3, libc::SIGTERM, false).unwrap();
            assert_eq!(classified, (143, "interrupted"));
        }
    }

    #[test]
    fn a_closed_reader_keeps_the_command_status_and_other_failures_are_errors() {
        assert_eq!(classify(Ok(7), 7, 0, false).unwrap(), (7, "completed"));
        assert_eq!(
            classify(Err(closed()), 7, 0, false).unwrap(),
            (7, "output-closed")
        );
        assert!(classify(Err(anyhow::anyhow!("EIO")), 7, 0, false).is_err());
    }

    #[test]
    fn an_interrupted_generation_is_labelled_interrupted() {
        assert_eq!(
            classify(Ok(143), 143, 0, true).unwrap(),
            (143, "interrupted")
        );
        assert_eq!(
            classify(Err(closed()), 143, 0, true).unwrap(),
            (143, "interrupted")
        );
    }

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
