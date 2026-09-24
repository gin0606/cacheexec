use crate::{
    domain::delivery,
    shell::{lock, store},
};
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::{Duration, SystemTime},
};

fn old_enough(completed: SystemTime, age: Option<Duration>, now: SystemTime) -> bool {
    age.is_none_or(|limit| now.duration_since(completed).is_ok_and(|age| age > limit))
}

/// Unlinks an idle key's `.lock` while its lock is held. Callers waiting on the
/// old inode see that it has no links left once they lock it, and reopen the
/// path (see `lock::acquire_gate`); checking the path alone would be racy.
fn remove_gate(path: &Path) -> Result<()> {
    fs::remove_file(path).context("remove idle key lock")
}

pub fn run(directory: &Path, age: Option<Duration>) -> Result<i32> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            print_summary("removed=0 abandoned=0 skipped=0 failed=0")?;
            return Ok(0);
        }
        Err(error) => return Err(error).context("scan cache directory"),
    };
    let mut keys = BTreeSet::new();
    let mut errors = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if let Some((key, extension)) = name.rsplit_once('.') {
                    if matches!(extension, "result" | "active" | "lock")
                        && key.len() == 64
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                    {
                        keys.insert(key.to_owned());
                    }
                }
            }
            Err(error) => errors.push(format!("scan cache entry: {error}")),
        }
    }
    let now = SystemTime::now();
    let (mut removed, mut abandoned, mut skipped) = (0, 0, 0);
    for key in keys {
        let outcome = (|| -> Result<()> {
            let gate_path = directory.join(format!("{key}.lock"));
            let Some(_gate) = lock::acquire_gate(&gate_path, false)? else {
                skipped += 1;
                return Ok(());
            };
            let active_path = directory.join(format!("{key}.active"));
            let active = match OpenOptions::new().read(true).write(true).open(&active_path) {
                Ok(active) => Some(active),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("open active execution"),
            };
            if let Some(active) = active {
                if !lock::try_lock(&active, true)? {
                    skipped += 1;
                    return Ok(());
                }
                // Waiters retain open descriptors, so unlinking an abandoned name
                // cannot discard their generation or trigger a retry.
                fs::remove_file(&active_path).context("remove abandoned execution")?;
                abandoned += 1;
            }
            let result_path = directory.join(format!("{key}.result"));
            let completed = match store::read(&result_path)? {
                Some(store::Stored::Current(record)) => Some(record.completed),
                // Results are written right after completion, so the
                // modification time stands in for an unreadable completion time.
                Some(store::Stored::OtherVersion) => Some(
                    fs::metadata(&result_path)
                        .and_then(|metadata| metadata.modified())
                        .context("read result modification time")?,
                ),
                None => None,
            };
            let Some(completed) = completed else {
                return remove_gate(&gate_path);
            };
            if old_enough(completed, age, now) {
                fs::remove_file(&result_path).context("delete cached result")?;
                removed += 1;
                return remove_gate(&gate_path);
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            errors.push(format!("{key}: {error:#}"));
        }
    }
    let summary = format!(
        "removed={removed} abandoned={abandoned} skipped={skipped} failed={}",
        errors.len()
    );
    if !errors.is_empty() {
        bail!(
            "cache cleanup partially applied ({summary}): {}",
            errors.join("; ")
        );
    }
    print_summary(&summary)?;
    Ok(0)
}

/// A reader that closed early (e.g. `| head`) chose to stop reading, and the
/// deletions are already done, so only other write failures are errors. Those
/// still carry the counts, which would otherwise be lost.
fn print_summary(summary: &str) -> Result<()> {
    match writeln!(std::io::stdout(), "{summary}") {
        Err(error) if !delivery::reader_closed(&error) => {
            Err(error).with_context(|| format!("print cleanup summary ({summary})"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_completion_age_boundary() {
        let completed = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let age = Duration::from_secs(5);
        assert!(!old_enough(completed, Some(age), completed + age));
        assert!(old_enough(
            completed,
            Some(age),
            completed + age + Duration::from_nanos(1)
        ));
        assert!(!old_enough(completed, Some(age), SystemTime::UNIX_EPOCH));
        assert!(old_enough(completed, None, SystemTime::UNIX_EPOCH));
    }
}
