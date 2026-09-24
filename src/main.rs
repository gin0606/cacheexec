mod domain;
mod shell;

use anyhow::{Context, Result};
use clap::{
    Parser,
    error::{ContextKind, ContextValue, ErrorKind},
};
use domain::{key, location, policy};
use shell::{cleanup, sharing, signals, verbose};
use std::{ffi::OsString, path::PathBuf, time::Duration};

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
  $XDG_CACHE_HOME/cacheexec or $HOME/.cache/cacheexec; override with --cache-dir.
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
    let path = directory.join(format!("{key}.result"));
    std::fs::create_dir_all(&directory).context("create cache directory")?;
    sharing::run(&request, &directory, &key, &path, diagnostic)
}

fn default_cache_dir() -> Result<PathBuf> {
    location::default_cache_dir(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
        .context("neither XDG_CACHE_HOME nor HOME is set; supply --cache-dir")
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
            let diagnostic = std::thread::spawn(move || eprintln!("cacheexec: {error:#}"));
            while !diagnostic.is_finished() && signals::received() == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            125
        }
    };
    std::process::exit(code);
}
