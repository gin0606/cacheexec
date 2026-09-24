use crate::domain::record::{self, Record};
use anyhow::{Context, Result};
use std::{fs, io::Write, path::Path};

pub enum Stored {
    Current(Record),
    /// Written in another format version, so this binary can neither reuse nor
    /// verify it. Callers treat it as absent rather than corrupt, keeping format
    /// upgrades and downgrades free of manual recovery.
    OtherVersion,
}

pub fn read(path: &Path) -> Result<Option<Stored>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read cached result {path:?}")),
    };
    if record::other_version(&bytes) {
        return Ok(Some(Stored::OtherVersion));
    }
    record::decode(&bytes)
        .with_context(|| {
            format!(
                "corrupt cached result {path:?}. Recovery: stop all cacheexec invocations using this cache directory, remove only this .result file, then retry; keep .lock and .active files"
            )
        })
        .map(|record| Some(Stored::Current(record)))
}

pub fn load(path: &Path) -> Result<Option<Record>> {
    Ok(match read(path)? {
        Some(Stored::Current(record)) => Some(record),
        Some(Stored::OtherVersion) | None => None,
    })
}

pub fn invalidate(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("invalidate previous cached result {path:?}")),
    }
}

pub fn save(path: &Path, record: &Record) -> Result<()> {
    let bytes = record::encode(record)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("cache path has no parent")?)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}
