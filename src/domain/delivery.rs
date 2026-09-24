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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn failed(kind: ErrorKind) -> Result<()> {
        Err(anyhow::Error::from(Error::from(kind)).context("write"))
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
