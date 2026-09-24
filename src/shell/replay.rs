use crate::{
    domain::{delivery::delivery_result, execution::signal_code, record::Record},
    shell::signals,
};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};

pub fn write(record: Record) -> Result<i32> {
    let code = record.code;
    let (completed, completion) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = completed.send(replay_bytes(&record));
    });
    loop {
        if signals::received() != 0 {
            // main exits immediately after this return; a blocked writer must not
            // prevent cancellation or keep the process alive.
            return Ok(signal_code(signals::received()));
        }
        // Completion wakes immediately; the timeout only bounds signal latency
        // while an output consumer has stopped reading.
        match completion.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                if signals::received() != 0 {
                    return Ok(signal_code(signals::received()));
                }
                result?;
                return Ok(code);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("replay worker panicked"),
        }
    }
}

fn replay_bytes(record: &Record) -> Result<()> {
    // Like live delivery, a failure on one stream does not stop the other.
    let out = (|| {
        let mut out = std::io::stdout().lock();
        out.write_all(&record.stdout).context("replay stdout")?;
        out.flush().context("flush stdout")
    })();
    let err = (|| {
        let mut err = std::io::stderr().lock();
        err.write_all(&record.stderr).context("replay stderr")?;
        err.flush().context("flush stderr")
    })();
    delivery_result(out, err)
}
