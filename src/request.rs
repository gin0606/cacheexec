use crate::cache::Record;
use std::{
    ffi::OsString,
    time::{Duration, SystemTime},
};

pub struct Request {
    pub command: Vec<OsString>,
    pub ttl: Duration,
    pub refresh: bool,
    pub include_codes: Option<Vec<u8>>,
    pub exclude_codes: Option<Vec<u8>>,
}

impl Request {
    pub fn allows(&self, code: i32) -> bool {
        let Ok(code) = u8::try_from(code) else {
            return false;
        };
        self.include_codes
            .as_ref()
            .is_none_or(|codes| codes.contains(&code))
            && self
                .exclude_codes
                .as_ref()
                .is_none_or(|codes| !codes.contains(&code))
    }

    /// Returns why a new execution is required, or `None` when `record` is reusable.
    pub fn reason(&self, record: Option<&Record>, now: SystemTime) -> Option<&'static str> {
        if self.refresh {
            return Some("refresh");
        }
        let Some(record) = record else {
            return Some("missing");
        };
        if now.duration_since(record.completed).is_err() {
            return Some("future-timestamp");
        }
        if !record.fresh(self.ttl, now) {
            return Some("expired");
        }
        if !self.allows(record.code) {
            return Some("policy");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn reason_priority_and_ttl_boundary() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        for refresh in [false, true] {
            for missing in [false, true] {
                for future in [false, true] {
                    for expired in [false, true] {
                        for excluded in [false, true] {
                            let request = Request {
                                command: vec!["true".into()],
                                ttl: Duration::from_secs(5),
                                refresh,
                                include_codes: None,
                                exclude_codes: excluded.then_some(vec![0]),
                            };
                            let record = Record {
                                completed: if future {
                                    now + Duration::from_secs(1)
                                } else if expired {
                                    now - Duration::from_secs(6)
                                } else {
                                    now - Duration::from_secs(5)
                                },
                                code: 0,
                                stdout: vec![],
                                stderr: vec![],
                            };
                            let expected = if refresh {
                                Some("refresh")
                            } else if missing {
                                Some("missing")
                            } else if future {
                                Some("future-timestamp")
                            } else if expired {
                                Some("expired")
                            } else if excluded {
                                Some("policy")
                            } else {
                                None
                            };
                            assert_eq!(
                                request.reason((!missing).then_some(&record), now),
                                expected
                            );
                        }
                    }
                }
            }
        }
    }
}
