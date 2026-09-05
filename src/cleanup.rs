use crate::{cache, sharing};
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    path::Path,
    time::{Duration, SystemTime},
};

fn old_enough(completed: SystemTime, age: Option<Duration>, now: SystemTime) -> bool {
    age.is_none_or(|limit| now.duration_since(completed).is_ok_and(|age| age > limit))
}

pub fn run(directory: &Path, age: Option<Duration>) -> Result<i32> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("removed=0 abandoned=0 skipped=0 failed=0");
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
                    if matches!(extension, "result" | "active")
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
            // Never unlink this inode: executions may already have it open while
            // waiting for the gate, including calls that began after our scan.
            let gate = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(format!("{key}.lock")))
                .context("open key lock")?;
            if !sharing::try_lock(&gate, true)? {
                skipped += 1;
                return Ok(());
            }
            let active_path = directory.join(format!("{key}.active"));
            let active = match OpenOptions::new().read(true).write(true).open(&active_path) {
                Ok(active) => Some(active),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("open active execution"),
            };
            if let Some(active) = active {
                if !sharing::try_lock(&active, true)? {
                    skipped += 1;
                    return Ok(());
                }
                // Waiters retain open descriptors, so unlinking an abandoned name
                // cannot discard their generation or trigger a retry.
                fs::remove_file(&active_path).context("remove abandoned execution")?;
                abandoned += 1;
            }
            let result_path = directory.join(format!("{key}.result"));
            if let Some(record) = cache::load(&result_path)? {
                if old_enough(record.completed, age, now) {
                    fs::remove_file(&result_path).context("delete cached result")?;
                    removed += 1;
                }
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
    println!("{summary}");
    Ok(0)
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
