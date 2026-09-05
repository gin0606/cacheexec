mod cache;
mod cleanup;
mod runner;
mod sharing;
mod signals;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{ffi::OsString, path::PathBuf, time::Duration};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Cache non-interactive command output and exit status",
    after_help = "Runs argv directly with closed stdin and inherited environment. Use -- sh -c '...' for shell syntax. TTL is measured from completion (e.g. 500ms, 5m, 1h). Exit codes: child code on success, 128+signal for a signaled child, 125 with a cacheexec: diagnostic for tool errors (including post-execution save failures). Cache defaults to $XDG_CACHE_HOME/cacheexec or $HOME/.cache/cacheexec on macOS and Linux. Environment changes are not automatically keyed; use --key or --refresh. Concurrent calls share one execution even with different policies or --refresh. Waiters replay the completed result. SIGINT/SIGTERM interrupt only a waiter, or propagate from the execution owner to its child process group. Owner death fails waiters without retry. Cleanup: --clear [--older-than 24h] takes no command or TTL, reports removed/abandoned/skipped/failed counts, and skips busy keys. Missing directories succeed; partial failures exit 125. Age is strictly greater than the cleanup duration, independently of TTL. Stable locks and unidentified temporary files remain; no automatic cleanup. No execution timeout. Output is binary-safe; timing and ordering between stdout and stderr are not preserved on replay."
)]
struct Cli {
    /// Maximum result age since completion (required, including for refresh)
    #[arg(long, value_parser = humantime::parse_duration, required_unless_present = "clear", conflicts_with = "clear")]
    ttl: Option<Duration>,
    /// Delete idle results; retain busy keys and stable lock files
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

impl Cli {
    fn allows(&self, code: i32) -> bool {
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
}

fn run(cli: Cli) -> Result<i32> {
    let directory = match cli.cache_dir.clone() {
        Some(path) => path,
        None => default_cache_dir()?,
    };
    if cli.clear {
        return cleanup::run(&directory, cli.older_than);
    }
    let cwd = std::env::current_dir().context("read working directory")?;
    let key = cache::key(&cli.command, &cwd, cli.key.as_deref());
    let path = directory.join(format!("{key}.result"));
    std::fs::create_dir_all(&directory).context("create cache directory")?;
    sharing::run(&cli, &directory, &key, &path)
}

fn default_cache_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(path).join("cacheexec"));
    }
    if let Some(path) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(path).join(".cache/cacheexec"));
    }
    bail!("neither XDG_CACHE_HOME nor HOME is set; supply --cache-dir")
}

fn main() {
    let code = match signals::install().and_then(|()| run(Cli::parse())) {
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
