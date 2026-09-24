mod domain;
mod shell;

use anyhow::{Context, Result};
use clap::{
    Parser,
    error::{ContextKind, ContextValue, ErrorKind},
};
use domain::{key, location, policy};
use shell::{cleanup, sharing, signals, verbose};
use std::{
    convert::Infallible,
    ffi::OsString,
    fmt::Display,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Cache non-interactive command output and exit status",
    override_usage = "cacheexec --ttl <TTL> [OPTIONS] -- <COMMAND>...
       cacheexec --clear [--older-than <DURATION>] [--cache-dir <PATH>]",
    after_help = "Examples:
  cacheexec --ttl 5m --include-codes 0 -- curl -fsS https://example.com/status
  cacheexec --ttl 5m --include-codes 0,1 -- ./condition-check.sh
  cacheexec --ttl 5m --verbose -- ./check.sh
  cacheexec --clear --older-than 24h

Important behavior:
  Put -- before the command. Arguments run directly; stdin is closed.
  All normal exit codes, including nonzero codes, are cached by default.
  TTL starts at completion (500ms, 5m, 1h). Environment changes are not keyed.
  Same-key calls share execution, including --refresh and different policies.
  --verbose adds human diagnostics to stderr (best effort; not a stable format).

Exit codes:
  0..255       Child's normal exit code
  128+signal   Interrupted execution or waiter
  125          Tool error, with a cacheexec: diagnostic
  2            Invalid arguments
  Child codes can also be 2 or 125; distinguish tool errors by diagnostics.
  A closed output reader (e.g. | head) keeps the child's code, with no
  diagnostic.

Storage and cleanup:
  $XDG_CACHE_HOME/cacheexec (absolute only) or $HOME/.cache/cacheexec; override with --cache-dir.
  --clear takes no command or TTL, skips busy keys and reports counts.
  There is no automatic cleanup.
  See README.md / README.ja.md for recovery steps and full behavior."
)]
struct Cli {
    /// Maximum result age since completion (required, including for refresh)
    #[arg(long, value_parser = humantime::parse_duration, required_unless_present = "clear", conflicts_with = "clear")]
    ttl: Option<Duration>,
    /// Delete idle results and lock files of keys left empty; skip busy keys
    #[arg(long, conflicts_with_all = ["command", "key", "refresh", "include_codes", "exclude_codes"])]
    clear: bool,
    /// With --clear, delete only results strictly older than this completion age
    #[arg(long, requires = "clear", value_parser = humantime::parse_duration)]
    older_than: Option<Duration>,
    /// Override the cache directory
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Additional cache key (environment is not automatically included)
    #[arg(long)]
    key: Option<OsString>,
    /// Invalidate the old result, or join an execution already in progress
    #[arg(long)]
    refresh: bool,
    /// Explain cache decisions and saving on stderr (best effort, human-readable)
    #[arg(long, conflicts_with = "clear")]
    verbose: bool,
    /// Save/reuse only these exit codes (comma-separated, 0..255)
    #[arg(long, value_delimiter = ',', conflicts_with = "exclude_codes")]
    include_codes: Option<Vec<u8>>,
    /// Save/reuse all except these exit codes (comma-separated, 0..255)
    #[arg(long, value_delimiter = ',')]
    exclude_codes: Option<Vec<u8>>,
    /// Command and arguments, required after --; no shell interpretation
    #[arg(last = true, required_unless_present = "clear")]
    command: Vec<OsString>,
}

fn run(cli: Cli, diagnostic: &verbose::Verbose) -> Result<i32> {
    let directory = match cli.cache_dir {
        Some(path) => path,
        None => default_cache_dir()?,
    };
    if cli.clear {
        // Cleanup has no child to forward signals to and nothing to publish, so
        // it installs no handlers and a signal ends it at once.
        return cleanup::run(&directory, cli.older_than);
    }
    signals::install()?;
    let request = policy::Request {
        command: cli.command,
        ttl: cli.ttl.expect("clap requires --ttl without --clear"),
        refresh: cli.refresh,
        include_codes: cli.include_codes,
        exclude_codes: cli.exclude_codes,
    };
    let cwd = std::env::current_dir().context("read working directory")?;
    let key = key::key(&request.command, &cwd, cli.key.as_deref());
    std::fs::create_dir_all(&directory).context("create cache directory")?;
    sharing::run(&request, &directory, &key, diagnostic)
}

fn default_cache_dir() -> Result<PathBuf> {
    location::default_cache_dir(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
        .context("neither an absolute XDG_CACHE_HOME nor HOME is set; supply --cache-dir")
}

fn parse_cli() -> Cli {
    Cli::try_parse().unwrap_or_else(|mut error| {
        if error.kind() == ErrorKind::UnknownArgument
            && matches!(error.get(ContextKind::InvalidArg), Some(ContextValue::String(arg)) if !arg.starts_with('-'))
        {
            error.insert(
                ContextKind::Suggested,
                ContextValue::StyledStrs(vec![
                    "put -- before the command, for example: cacheexec --ttl 5m -- echo hello".into(),
                ]),
            );
        }
        error.exit()
    })
}

/// How long a signal still waits for the tool error diagnostic to be written.
/// The diagnostic is part of the exit code 125 contract, and a window of a
/// few milliseconds from the writer's spawn is lost on a loaded host; this
/// only delays exit when stderr is not being read.
const ERROR_GRACE: Duration = Duration::from_millis(200);

/// Writes the tool error diagnostic from a thread that `spawn` starts, and
/// waits for it; a signal waits only [`ERROR_GRACE`]. Thread creation fails
/// under the same resource exhaustion that often caused the error, so when no
/// thread starts the diagnostic is written inline instead of lost; a signal
/// cannot cut that write short, which this double failure accepts.
fn print_error(
    error: impl Display + Send + Sync + 'static,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    write: impl Fn(&str) + Clone + Send + 'static,
) {
    let error = Arc::new(error);
    let (printed, printing) = mpsc::channel::<Infallible>();
    let printer = Box::new({
        let (error, write) = (Arc::clone(&error), write.clone());
        move || {
            let _printed = printed;
            write(&format!("cacheexec: {error:#}"));
        }
    });
    if spawn(printer).is_err() {
        write(&format!("cacheexec: {error:#}"));
        return;
    }
    let _ = signals::wait_with_grace(&printing, ERROR_GRACE);
}

fn spawn_printer(printer: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    std::thread::Builder::new().spawn(printer).map(drop)
}

fn write_stderr(line: &str) {
    // A closed stderr must not turn the error into a panic.
    let _ = writeln!(io::stderr(), "{line}");
}

fn main() {
    let outcome = {
        let cli = parse_cli();
        let diagnostic = verbose::Verbose::new(cli.verbose);
        let outcome = run(cli, &diagnostic);
        if outcome.is_err() {
            diagnostic.failed(domain::message::Saved::Unknown);
        }
        outcome
    };
    let code = match outcome {
        Ok(code) => code,
        Err(error) => {
            // A signal must not wait long for a stderr consumer that stopped
            // reading, but the error still decides the exit code, so its
            // diagnostic gets a short grace.
            print_error(error, spawn_printer, write_stderr);
            125
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, thread};

    fn recorder() -> (
        Arc<Mutex<Vec<String>>>,
        impl Fn(&str) + Clone + Send + 'static,
    ) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&written);
        (written, move |line: &str| {
            sink.lock().unwrap().push(line.to_owned())
        })
    }

    #[test]
    fn the_diagnostic_is_written_inline_when_no_thread_starts() {
        let (written, write) = recorder();
        print_error("boom", |_| Err(io::Error::other("no threads")), write);
        assert_eq!(*written.lock().unwrap(), ["cacheexec: boom"]);
    }

    #[test]
    fn the_diagnostic_thread_is_waited_for() {
        let (written, write) = recorder();
        let on_main = thread::current().id();
        let write = move |line: &str| {
            assert_ne!(thread::current().id(), on_main);
            // Only a wait makes the line visible once print_error returns.
            thread::sleep(Duration::from_millis(20));
            write(line);
        };
        print_error("boom", spawn_printer, write);
        assert_eq!(*written.lock().unwrap(), ["cacheexec: boom"]);
    }
}
