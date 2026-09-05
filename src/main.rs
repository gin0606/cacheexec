mod cache;
mod runner;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{ffi::OsString, path::PathBuf, time::Duration};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Cache non-interactive command output and exit status",
    after_help = "Runs argv directly with closed stdin and inherited environment. Use -- sh -c '...' for shell syntax. TTL is measured from completion (e.g. 500ms, 5m, 1h). Exit codes: child code on success, 128+signal for a signaled child, 125 with a cacheexec: diagnostic for tool errors (including post-execution save failures). Cache defaults to $XDG_CACHE_HOME/cacheexec or $HOME/.cache/cacheexec on macOS and Linux. Environment changes are not automatically keyed; use --key or --refresh. Output is binary-safe; timing and ordering between stdout and stderr are not preserved on replay."
)]
struct Cli {
    /// Maximum result age since completion (required, including for refresh)
    #[arg(long, value_parser = humantime::parse_duration)]
    ttl: Duration,
    /// Override the cache directory
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Additional cache key (environment is not automatically included)
    #[arg(long)]
    key: Option<OsString>,
    /// Invalidate the old result and run again
    #[arg(long)]
    refresh: bool,
    /// Save/reuse only these exit codes (comma-separated, 0..255)
    #[arg(long, value_delimiter = ',', conflicts_with = "exclude_codes")]
    include_codes: Option<Vec<u8>>,
    /// Save/reuse all except these exit codes (comma-separated, 0..255)
    #[arg(long, value_delimiter = ',')]
    exclude_codes: Option<Vec<u8>>,
    /// Command and arguments, required after --; no shell interpretation
    #[arg(last = true, required = true)]
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
    let cwd = std::env::current_dir().context("read working directory")?;
    let key = cache::key(&cli.command, &cwd, cli.key.as_deref());
    let path = directory.join(format!("{key}.result"));
    std::fs::create_dir_all(&directory).context("create cache directory")?;
    // Even refresh diagnoses corruption instead of silently bypassing storage faults.
    let previous = cache::load(&path)?;
    if !cli.refresh {
        if let Some(record) = previous {
            if record.fresh(cli.ttl, std::time::SystemTime::now()) && cli.allows(record.code) {
                record.replay()?;
                return Ok(record.code);
            }
        }
    }
    cache::invalidate(&path)?;
    let result = runner::execute(&cli.command)?;
    if let Some(record) = result.record {
        if cli.allows(record.code) {
            cache::save(&path, &record).with_context(|| {
                format!(
                    "child already completed with exit code {}; could not save result",
                    record.code
                )
            })?;
        }
    }
    Ok(result.code)
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
    let code = match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cacheexec: {error:#}");
            125
        }
    };
    std::process::exit(code);
}
