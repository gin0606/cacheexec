use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// "CEXEC" followed by a three-digit format version.
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
}

pub fn other_version(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len()
        && bytes[..MAGIC.len()] != MAGIC[..]
        && bytes[..5] == MAGIC[..5]
        && bytes[5..MAGIC.len()].iter().all(u8::is_ascii_digit)
}

pub fn decode(bytes: &[u8]) -> Result<Record> {
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

pub fn encode(record: &Record) -> Result<Vec<u8>> {
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
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_another_three_digit_version_is_another_format() {
        assert!(!other_version(MAGIC));
        assert!(other_version(b"CEXEC002"));
        assert!(other_version(b"CEXEC999 and more"));
        for bytes in [&b"CEXEC00x"[..], b"CEXEC00", b"cexec002", b"XEXEC002"] {
            assert!(!other_version(bytes), "{bytes:?}");
        }
    }

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
