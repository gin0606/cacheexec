use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    os::unix::ffi::OsStrExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"CEXEC001";

pub struct Record {
    pub completed: SystemTime,
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl Record {
    pub fn fresh(&self, ttl: Duration, now: SystemTime) -> bool {
        now.duration_since(self.completed)
            .is_ok_and(|age| age <= ttl)
    }
    pub fn replay(&self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        out.write_all(&self.stdout).context("replay stdout")?;
        out.flush().context("flush stdout")?;
        err.write_all(&self.stderr).context("replay stderr")?;
        err.flush().context("flush stderr")?;
        Ok(())
    }
}

pub fn key(argv: &[OsString], cwd: &Path, extra: Option<&OsStr>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cacheexec-key-v1");
    let mut field = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    field(cwd.as_os_str().as_bytes());
    field(&(argv.len() as u64).to_le_bytes());
    for arg in argv {
        field(arg.as_bytes());
    }
    field(&[u8::from(extra.is_some())]);
    if let Some(extra) = extra {
        field(extra.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub fn load(path: &Path) -> Result<Option<Record>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read cached result"),
    };
    decode(&bytes).context("corrupt cached result").map(Some)
}

fn decode(bytes: &[u8]) -> Result<Record> {
    if bytes.len() < 76 || &bytes[..8] != MAGIC {
        bail!("invalid header");
    }
    let (body, checksum) = bytes.split_at(bytes.len() - 32);
    if Sha256::digest(body).as_slice() != checksum {
        bail!("checksum mismatch");
    }
    let nanos = u128::from_le_bytes(body[8..24].try_into()?);
    let seconds = u64::try_from(nanos / 1_000_000_000)?;
    let completed = UNIX_EPOCH
        .checked_add(Duration::new(seconds, (nanos % 1_000_000_000) as u32))
        .context("invalid completion timestamp")?;
    let code = i32::from_le_bytes(body[24..28].try_into()?);
    if !(0..=255).contains(&code) {
        bail!("invalid exit code");
    }
    let out_len = usize::try_from(u64::from_le_bytes(body[28..36].try_into()?))?;
    let err_len = usize::try_from(u64::from_le_bytes(body[36..44].try_into()?))?;
    if out_len.checked_add(err_len) != Some(body.len() - 44) {
        bail!("invalid output lengths");
    }
    Ok(Record {
        completed,
        code,
        stdout: body[44..44 + out_len].to_vec(),
        stderr: body[44 + out_len..].to_vec(),
    })
}

pub fn invalidate(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("invalidate previous cached result"),
    }
}

pub fn save(path: &Path, record: &Record) -> Result<()> {
    let mut bytes = Vec::new();
    bytes.extend(MAGIC);
    bytes.extend(
        record
            .completed
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            .to_le_bytes(),
    );
    bytes.extend(record.code.to_le_bytes());
    bytes.extend((record.stdout.len() as u64).to_le_bytes());
    bytes.extend((record.stderr.len() as u64).to_le_bytes());
    bytes.extend(&record.stdout);
    bytes.extend(&record.stderr);
    bytes.extend_from_slice(&Sha256::digest(&bytes));
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("cache path has no parent")?)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ttl_boundary_and_future_clock() {
        let record = Record {
            completed: UNIX_EPOCH + Duration::from_secs(100),
            code: 0,
            stdout: vec![],
            stderr: vec![],
        };
        let ttl = Duration::from_secs(5);
        assert!(record.fresh(ttl, record.completed + ttl));
        assert!(!record.fresh(ttl, record.completed + ttl + Duration::from_nanos(1)));
        assert!(!record.fresh(ttl, UNIX_EPOCH));
    }
}
