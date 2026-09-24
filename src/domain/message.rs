//! The `--verbose` diagnostic lines, without their framing.

use std::{
    path::Path,
    time::{Duration, SystemTimeError},
};

/// `age` is the saved result's age, or how far in the future it completed.
pub fn decision(
    action: &str,
    age: Option<Result<Duration, SystemTimeError>>,
    ttl: Duration,
    directory: &Path,
    key: &str,
) -> String {
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

pub fn finished(kind: &str, code: i32, saving: &str) -> String {
    format!("{kind} exit={code} saved={saving}")
}

pub fn interrupted(code: i32, reason: &str) -> String {
    format!("interrupted exit={code} saved=unknown reason={reason}")
}

/// The saving status after a failed execution: its saved result is gone only
/// if it was invalidated.
pub fn saved_after_failure(invalidated: bool) -> &'static str {
    if invalidated { "no" } else { "unknown" }
}

pub fn failed(saving: &str, reason: &str) -> String {
    format!("failed saved={saving} reason={reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn a_decision_shows_a_future_result_as_a_negative_age() {
        let future = UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500));
        let line = |age| decision("hit", age, Duration::from_secs(300), Path::new("/c"), "k");
        assert_eq!(
            line(Some(Ok(Duration::from_millis(1500)))),
            r#"hit age=1.5s ttl=5m key=k cache-dir="/c""#
        );
        assert_eq!(
            line(Some(future)),
            r#"hit age=-1.5s ttl=5m key=k cache-dir="/c""#
        );
        assert_eq!(line(None), r#"hit ttl=5m key=k cache-dir="/c""#);
    }
}
