use std::time::{Duration, SystemTime};

/// What cleanup removes for an idle key.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposal {
    Keep,
    /// No result is left, so only the key's lock file goes.
    Gate,
    ResultAndGate,
}

/// `completed` is the completion time of the key's result, if it has one.
pub fn disposal(completed: Option<SystemTime>, age: Option<Duration>, now: SystemTime) -> Disposal {
    match completed {
        None => Disposal::Gate,
        Some(completed) if old_enough(completed, age, now) => Disposal::ResultAndGate,
        Some(_) => Disposal::Keep,
    }
}

/// Whether a result that completed at `completed` is strictly older than
/// `age`. Without `age` every result qualifies; one completed in the future
/// never qualifies for a limit.
fn old_enough(completed: SystemTime, age: Option<Duration>, now: SystemTime) -> bool {
    age.is_none_or(|limit| now.duration_since(completed).is_ok_and(|age| age > limit))
}

pub fn summary_line(removed: usize, abandoned: usize, skipped: usize, failed: usize) -> String {
    format!("removed={removed} abandoned={abandoned} skipped={skipped} failed={failed}")
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

    #[test]
    fn a_key_without_a_result_loses_only_its_lock_file() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let age = Some(Duration::from_secs(5));
        assert_eq!(disposal(None, age, now), Disposal::Gate);
        assert_eq!(
            disposal(Some(now - Duration::from_secs(6)), age, now),
            Disposal::ResultAndGate
        );
        assert_eq!(
            disposal(Some(now - Duration::from_secs(5)), age, now),
            Disposal::Keep
        );
    }
}
