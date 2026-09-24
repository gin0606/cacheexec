use crate::domain::execution::signal_code;
use anyhow::Result;

/// Combines the stdout and stderr delivery results. A reader that closed
/// early (EPIPE) on one stream must not hide a different failure on the other.
pub fn delivery_result(out: Result<()>, err: Result<()>) -> Result<()> {
    match (out, err) {
        (Err(out), Err(err)) if output_closed(&out) && !output_closed(&err) => Err(err),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub fn output_closed(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(reader_closed)
    })
}

/// Whether a write failed because its reader closed early, which callers treat
/// as the reader's choice to stop rather than a tool failure.
pub fn reader_closed(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::BrokenPipe
}

/// The exit status and completion label of a delivery or replay outcome.
pub fn classify(
    outcome: Result<i32>,
    command_code: i32,
    signal: i32,
    generation_interrupted: bool,
) -> Result<(i32, &'static str)> {
    let (code, kind) = match outcome {
        // A signal received during delivery ends it with 128 + signal however
        // delivery ended, as live delivery and replay do.
        _ if signal != 0 => (signal_code(signal), "interrupted"),
        Ok(code) => (code, "completed"),
        // A consumer such as `head` closing its end is a normal way to stop
        // reading, not a tool failure. The status of the delivered result is
        // still known, so it is reported; shared and saved results stay intact.
        Err(error) if output_closed(&error) => (command_code, "output-closed"),
        Err(error) => return Err(error),
    };
    Ok((
        code,
        if generation_interrupted {
            "interrupted"
        } else {
            kind
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn failed(kind: ErrorKind) -> Result<()> {
        Err(anyhow::Error::from(Error::from(kind)).context("write"))
    }

    fn closed() -> anyhow::Error {
        anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            .context("replay stdout")
    }

    #[test]
    fn a_signal_during_delivery_wins_over_every_outcome() {
        for outcome in [Ok(3), Err(closed()), Err(anyhow::anyhow!("disk full"))] {
            let classified = classify(outcome, 3, 15, false).unwrap();
            assert_eq!(classified, (143, "interrupted"));
        }
    }

    #[test]
    fn a_closed_reader_keeps_the_command_status_and_other_failures_are_errors() {
        assert_eq!(classify(Ok(7), 7, 0, false).unwrap(), (7, "completed"));
        assert_eq!(
            classify(Err(closed()), 7, 0, false).unwrap(),
            (7, "output-closed")
        );
        assert!(classify(Err(anyhow::anyhow!("EIO")), 7, 0, false).is_err());
    }

    #[test]
    fn an_interrupted_generation_is_labelled_interrupted() {
        assert_eq!(
            classify(Ok(143), 143, 0, true).unwrap(),
            (143, "interrupted")
        );
        assert_eq!(
            classify(Err(closed()), 143, 0, true).unwrap(),
            (143, "interrupted")
        );
    }

    #[test]
    fn a_closed_reader_never_hides_another_failure() {
        let closed = || failed(ErrorKind::BrokenPipe);
        let other = || failed(ErrorKind::Other);
        let is_closed = |result: Result<()>| output_closed(&result.unwrap_err());
        assert!(delivery_result(Ok(()), Ok(())).is_ok());
        assert!(is_closed(delivery_result(closed(), Ok(()))));
        assert!(is_closed(delivery_result(Ok(()), closed())));
        assert!(is_closed(delivery_result(closed(), closed())));
        assert!(!is_closed(delivery_result(closed(), other())));
        assert!(!is_closed(delivery_result(other(), closed())));
        assert!(!is_closed(delivery_result(Ok(()), other())));
    }
}
