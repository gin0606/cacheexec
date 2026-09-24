//! The `--verbose` diagnostic lines, without their framing.

use crate::domain::{delivery::Ending, execution::Saving, policy::Reason};
use std::{
    path::Path,
    time::{Duration, SystemTimeError},
};

/// How a caller will obtain its result.
pub enum Decision {
    Hit,
    Join,
    Run(Reason),
}

/// `age` is the saved result's age, or how far in the future it completed.
pub fn decision(
    decision: Decision,
    age: Option<Result<Duration, SystemTimeError>>,
    ttl: Duration,
    directory: &Path,
    key: &str,
) -> String {
    let action = match decision {
        Decision::Hit => "hit".to_owned(),
        Decision::Join => "join".to_owned(),
        Decision::Run(reason) => format!(
            "run reason={}",
            match reason {
                Reason::Refresh => "refresh",
                Reason::Missing => "missing",
                Reason::FutureTimestamp => "future-timestamp",
                Reason::Expired => "expired",
                Reason::Policy => "policy",
            }
        ),
    };
    let age = match age {
        Some(Ok(age)) => format!(" age={}s", age.as_secs_f64()),
        Some(Err(future)) => format!(" age=-{}s", future.duration().as_secs_f64()),
        None => String::new(),
    };
    format!(
        "{action}{age} ttl={} key={key} cache-dir={directory:?}",
        humantime::format_duration(ttl),
    )
}

/// What became of the result an invocation delivered.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// The status an execution's owner decided for its result.
    Execution(Saving),
    /// Replayed from an existing saved result.
    Reused,
    /// The saved result for the key is gone.
    No,
    Unknown,
}

impl Saved {
    /// After a failed execution the saved result is gone only if it was
    /// invalidated.
    pub fn after_failure(invalidated: bool) -> Self {
        if invalidated { Self::No } else { Self::Unknown }
    }

    /// Whether the execution that produced the result was interrupted, which
    /// labels every delivery of it as interrupted.
    pub fn interrupted(self) -> bool {
        self == Self::Execution(Saving::Interrupted)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Execution(Saving::Saved) => "yes",
            Self::Execution(Saving::Excluded) => "no reason=participant-policy",
            Self::Execution(Saving::Interrupted) => "no reason=interrupted",
            Self::Reused => "no reason=reused",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

pub fn finished(ending: Ending, code: i32, saved: Saved) -> String {
    let ending = match ending {
        Ending::Completed => "completed",
        Ending::OutputClosed => "output-closed",
        Ending::Interrupted => "interrupted",
    };
    format!("{ending} exit={code} saved={}", saved.label())
}

/// Where a caller was waiting when a signal stopped it.
pub enum Wait {
    /// For the key's lock.
    Key,
    /// For another caller's execution of the same key.
    Execution,
}

pub fn interrupted(code: i32, wait: Wait) -> String {
    let reason = match wait {
        Wait::Key => "interrupted",
        Wait::Execution => "waiter-interrupted",
    };
    format!("interrupted exit={code} saved=unknown reason={reason}")
}

/// Which step of an invocation failed.
pub enum Failure {
    Execution,
    Delivery,
    Replay,
}

pub fn failed(saved: Saved, failure: Failure) -> String {
    let reason = match failure {
        Failure::Execution => "failure",
        Failure::Delivery => "delivery-failure",
        Failure::Replay => "replay-failure",
    };
    format!("failed saved={} reason={reason}", saved.label())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn a_decision_shows_a_future_result_as_a_negative_age() {
        let future = UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500));
        let line = |age| {
            decision(
                Decision::Hit,
                age,
                Duration::from_secs(300),
                Path::new("/c"),
                "k",
            )
        };
        assert_eq!(
            line(Some(Ok(Duration::from_millis(1500)))),
            r#"hit age=1.5s ttl=5m key=k cache-dir="/c""#
        );
        assert_eq!(
            line(Some(future)),
            r#"hit age=-1.5s ttl=5m key=k cache-dir="/c""#
        );
        assert_eq!(line(None), r#"hit ttl=5m key=k cache-dir="/c""#);
        assert_eq!(
            decision(
                Decision::Run(Reason::Missing),
                None,
                Duration::from_secs(1),
                Path::new("/c"),
                "k"
            ),
            r#"run reason=missing ttl=1s key=k cache-dir="/c""#
        );
    }

    #[test]
    fn endings_saved_statuses_and_failures_have_their_labels() {
        assert_eq!(
            finished(Ending::OutputClosed, 7, Saved::Execution(Saving::Excluded)),
            "output-closed exit=7 saved=no reason=participant-policy"
        );
        assert_eq!(
            finished(
                Ending::Interrupted,
                143,
                Saved::Execution(Saving::Interrupted)
            ),
            "interrupted exit=143 saved=no reason=interrupted"
        );
        assert_eq!(
            finished(Ending::Completed, 0, Saved::Reused),
            "completed exit=0 saved=no reason=reused"
        );
        assert_eq!(
            failed(Saved::Execution(Saving::Saved), Failure::Delivery),
            "failed saved=yes reason=delivery-failure"
        );
        assert_eq!(
            failed(Saved::Reused, Failure::Replay),
            "failed saved=no reason=reused reason=replay-failure"
        );
        assert_eq!(
            failed(Saved::after_failure(true), Failure::Execution),
            "failed saved=no reason=failure"
        );
        assert_eq!(
            failed(Saved::after_failure(false), Failure::Execution),
            "failed saved=unknown reason=failure"
        );
        assert_eq!(
            interrupted(130, Wait::Execution),
            "interrupted exit=130 saved=unknown reason=waiter-interrupted"
        );
        assert_eq!(
            interrupted(130, Wait::Key),
            "interrupted exit=130 saved=unknown reason=interrupted"
        );
    }

    #[test]
    fn only_an_interrupted_execution_labels_its_deliveries_interrupted() {
        assert!(Saved::Execution(Saving::Interrupted).interrupted());
        for saved in [
            Saved::Execution(Saving::Saved),
            Saved::Execution(Saving::Excluded),
            Saved::Reused,
            Saved::No,
            Saved::Unknown,
        ] {
            assert!(!saved.interrupted());
        }
    }
}
