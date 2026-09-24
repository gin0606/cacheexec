use crate::domain::{policy::Request, record::Record};
use anyhow::{Result, bail};
use std::time::SystemTime;

/// The number of distinct normal exit codes.
pub const CODES: usize = 256;

/// One save-permission byte per exit code, set when any participant's policy
/// allows saving that code.
pub type Votes = [u8; CODES];

/// The exit status that reports an end caused by `signal`.
pub fn signal_code(signal: i32) -> i32 {
    128 + signal
}

pub fn add_vote(votes: &mut Votes, request: &Request) {
    for (code, vote) in votes.iter_mut().enumerate() {
        *vote |= u8::from(request.allows(code as i32));
    }
}

/// The result of one child execution, before it is saved or shared.
pub struct Outcome {
    record: Record,
    /// Only a normal exit that no signal to this process interrupted may be
    /// saved or labelled as the command's own result.
    reusable: bool,
}

impl Outcome {
    /// `exit_code` and `exit_signal` describe how the child ended; `interrupted`
    /// is the signal this process received (0 for none), which overrides both.
    pub fn new(
        completed: SystemTime,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        interrupted: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Self {
        let code = if interrupted != 0 {
            signal_code(interrupted)
        } else {
            exit_code.unwrap_or_else(|| signal_code(exit_signal.unwrap_or(0)))
        };
        Self {
            record: Record {
                completed,
                code,
                stdout,
                stderr,
            },
            reusable: exit_code.is_some() && interrupted == 0,
        }
    }

    pub fn record(&self) -> &Record {
        &self.record
    }

    pub fn code(&self) -> i32 {
        self.record.code
    }

    /// Marks the outcome as interrupted by `signal`; 0 means no signal and
    /// changes nothing. Returns whether anything observable changed.
    pub fn interrupt(&mut self, signal: i32) -> bool {
        if signal == 0 {
            return false;
        }
        let changed = self.code() != signal_code(signal) || self.reusable;
        self.record.code = signal_code(signal);
        self.reusable = false;
        changed
    }
}

/// Whether an outcome is saved, which decides both the save itself and the
/// `saved=` label that the owner and its waiters report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saving {
    Saved = 1,
    Excluded = 2,
    Interrupted = 3,
}

impl Saving {
    pub fn of(outcome: &Outcome, votes: &Votes) -> Self {
        if !outcome.reusable {
            Self::Interrupted
        } else if u8::try_from(outcome.code()).is_ok_and(|code| votes[usize::from(code)] != 0) {
            Self::Saved
        } else {
            Self::Excluded
        }
    }
    pub fn decode(byte: u8) -> Result<Self> {
        Ok(match byte {
            1 => Self::Saved,
            2 => Self::Excluded,
            3 => Self::Interrupted,
            _ => bail!("invalid shared saving status"),
        })
    }
    pub fn describe(self) -> &'static str {
        match self {
            Self::Saved => "yes",
            Self::Excluded => "no reason=participant-policy",
            Self::Interrupted => "no reason=interrupted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn outcome(exit_code: Option<i32>, exit_signal: Option<i32>, interrupted: i32) -> Outcome {
        Outcome::new(
            UNIX_EPOCH,
            exit_code,
            exit_signal,
            interrupted,
            vec![],
            vec![],
        )
    }

    #[test]
    fn a_received_signal_overrides_how_the_child_ended() {
        let normal = outcome(Some(7), None, 0);
        assert_eq!((normal.code(), normal.reusable), (7, true));
        let killed = outcome(None, Some(9), 0);
        assert_eq!((killed.code(), killed.reusable), (137, false));
        let interrupted = outcome(Some(7), None, 15);
        assert_eq!((interrupted.code(), interrupted.reusable), (143, false));
    }

    #[test]
    fn saving_follows_the_merged_votes() {
        let request = |include_codes| Request {
            command: vec!["true".into()],
            ttl: std::time::Duration::ZERO,
            refresh: false,
            include_codes,
            exclude_codes: None,
        };
        let mut votes = [0; 256];
        add_vote(&mut votes, &request(Some(vec![0])));
        add_vote(&mut votes, &request(Some(vec![7])));
        assert_eq!(
            Saving::of(&outcome(Some(0), None, 0), &votes),
            Saving::Saved
        );
        assert_eq!(
            Saving::of(&outcome(Some(7), None, 0), &votes),
            Saving::Saved
        );
        assert_eq!(
            Saving::of(&outcome(Some(1), None, 0), &votes),
            Saving::Excluded
        );
        assert_eq!(
            Saving::of(&outcome(Some(0), None, 2), &votes),
            Saving::Interrupted
        );
        // Codes outside 0..=255 have no vote and are never saved.
        for code in [-1, 256] {
            assert_eq!(
                Saving::of(&outcome(Some(code), None, 0), &[1; CODES]),
                Saving::Excluded
            );
        }
    }

    #[test]
    fn interrupting_reports_only_observable_changes() {
        let mut outcome = outcome(Some(7), None, 0);
        assert!(outcome.interrupt(15));
        assert_eq!((outcome.code(), outcome.reusable), (143, false));
        assert!(!outcome.interrupt(15));
        assert!(outcome.interrupt(2));
        assert_eq!(outcome.code(), 130);
        assert!(!outcome.interrupt(0));
        assert_eq!(outcome.code(), 130);
    }

    #[test]
    fn interrupting_a_reusable_result_with_its_own_code_is_a_change() {
        // The child exited 143 by itself, so only reusability changes.
        let mut exited = outcome(Some(143), None, 0);
        assert!(exited.interrupt(15));
        assert_eq!((exited.code(), exited.reusable), (143, false));
        // Killed by the same signal, the child's result is already final.
        let mut killed = outcome(None, Some(15), 0);
        assert!(!killed.interrupt(15));
    }
}
