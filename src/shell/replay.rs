use crate::{
    domain::{delivery::delivery_result, execution::signal_code, record::Record},
    shell::signals::{self, Waited},
};
use anyhow::{Context, Result, bail};
use std::{io::Write, sync::mpsc};

pub fn write(record: Record) -> Result<i32> {
    let code = record.code;
    let (completed, completion) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = completed.send(replay_bytes(&record));
    });
    match signals::wait(&completion) {
        Waited::Done(result) => result.map(|()| code),
        Waited::Interrupted(signal) => Ok(signal_code(signal)),
        Waited::Closed => bail!("replay worker panicked"),
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
