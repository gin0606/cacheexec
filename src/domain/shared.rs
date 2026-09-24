//! Layout of a `.active` file, through which an execution's owner shares its
//! result with waiters: one save-permission byte per exit code (the
//! [`Votes`](crate::domain::execution::Votes)), then a state tag, a detail
//! byte, and the encoded record or failure message. The detail byte of a
//! completed execution is its [`Saving`] status.

use crate::domain::{
    execution::{CODES, Saving},
    record::{self, Record},
};
use anyhow::{Context, Result};

pub const TAG_OFFSET: u64 = CODES as u64;
pub const DETAIL_OFFSET: u64 = TAG_OFFSET + 1;
pub const PENDING: u8 = 2;
pub const COMPLETED: u8 = 3;
pub const FAILED: u8 = 4;

pub enum Published {
    Completed {
        saving: Saving,
        record: Record,
    },
    Failed {
        /// Whether the owner removed the saved result for this key.
        invalidated: bool,
        message: String,
    },
    /// The owner ended without committing a tag.
    Unfinished,
}

/// A new execution's file: no votes yet, and nothing published.
pub fn pending() -> Vec<u8> {
    let mut bytes = vec![0; CODES];
    bytes.push(PENDING);
    bytes
}

/// Parses the bytes from [`TAG_OFFSET`] to the end of the file.
pub fn parse(from_tag: &[u8]) -> Result<Published> {
    Ok(match from_tag.split_first() {
        Some((&COMPLETED, detail)) => {
            let (saving, encoded) = detail
                .split_first()
                .context("missing shared saving status")?;
            let saving = Saving::decode(*saving)?;
            Published::Completed {
                saving,
                record: record::decode(encoded)?,
            }
        }
        Some((&FAILED, detail)) => {
            let (invalidated, message) = detail
                .split_first()
                .context("missing shared failure status")?;
            Published::Failed {
                invalidated: *invalidated == 1,
                message: String::from_utf8_lossy(message).into_owned(),
            }
        }
        _ => Published::Unfinished,
    })
}

/// The bytes from [`DETAIL_OFFSET`] that describe a completed execution.
pub fn completed_detail(saving: Saving, record: &Record) -> Result<Vec<u8>> {
    let mut bytes = vec![saving as u8];
    record::encode_into(&mut bytes, record)?;
    Ok(bytes)
}

/// The bytes from [`DETAIL_OFFSET`] that describe a failed execution.
pub fn failure_detail(invalidated: bool, message: &str) -> Vec<u8> {
    let mut bytes = vec![u8::from(invalidated)];
    bytes.extend(message.as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn a_new_execution_is_pending_after_its_votes() {
        let bytes = pending();
        assert_eq!(bytes.len() as u64, DETAIL_OFFSET);
        assert!(matches!(
            parse(&bytes[TAG_OFFSET as usize..]).unwrap(),
            Published::Unfinished
        ));
    }

    #[test]
    fn a_completed_result_round_trips() {
        let record = Record {
            completed: UNIX_EPOCH,
            code: 7,
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        let mut bytes = vec![COMPLETED];
        bytes.extend(completed_detail(Saving::Excluded, &record).unwrap());
        let Published::Completed { saving, record } = parse(&bytes).unwrap() else {
            panic!("not completed");
        };
        assert_eq!(saving, Saving::Excluded);
        assert_eq!((record.code, &record.stdout[..]), (7, &b"out"[..]));
    }

    #[test]
    fn a_failure_round_trips() {
        for invalidated in [false, true] {
            let mut bytes = vec![FAILED];
            bytes.extend(failure_detail(invalidated, "disk full"));
            let Published::Failed {
                invalidated: parsed,
                message,
            } = parse(&bytes).unwrap()
            else {
                panic!("not failed");
            };
            assert_eq!((parsed, &message[..]), (invalidated, "disk full"));
        }
    }

    #[test]
    fn a_pending_or_missing_tag_is_unfinished() {
        for bytes in [&[][..], &[PENDING], &[PENDING, 1, 2]] {
            assert!(matches!(parse(bytes).unwrap(), Published::Unfinished));
        }
    }

    #[test]
    fn a_tag_without_its_detail_is_an_error() {
        assert!(parse(&[COMPLETED]).is_err());
        assert!(parse(&[FAILED]).is_err());
        assert!(parse(&[COMPLETED, 9]).is_err());
    }
}
