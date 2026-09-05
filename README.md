# cacheexec

Cache the stdout, stderr and exit code of a non-interactive command on macOS and Linux.

```sh
cargo install --path . --locked
cacheexec --ttl 5m -- curl -fsS https://example.com/status
cacheexec --ttl 30s --include-codes 0,1 -- sh -c './condition-check.sh'
cacheexec --ttl 5m --key "$DEPLOY_ENV" --refresh -- ./query
```

The command **must follow `--`**. Arguments are executed directly without shell
interpretation; explicitly use `sh -c` for pipelines, redirects or expansions.
The child inherits the environment, receives closed stdin, and streams its stdout
and stderr while running. A hit skips execution and replays each byte stream and
the original exit code. Non-UTF-8 bytes are preserved. Stream timing and ordering
between stdout and stderr are not preserved on replay. No hit/miss logs are added.

`--ttl` is required and accepts durations such as `500ms`, `30s`, `5m`, and `1h`.
Age is measured from execution completion and is reusable at age **<= TTL**.
`0s` effectively requests a new execution. A clock earlier than the completion
time makes the result ineligible. Changing TTL uses the same key.

All normal exit codes (0–255) are saved by default. `--include-codes 0,1` saves
and reuses only those codes; `--exclude-codes 2,3` excludes them. These options are
mutually exclusive and each invocation applies its own policy to existing results.
Start failures and signal termination are never saved. `--refresh` invalidates
the previous result before starting again. Ineligible, failed or interrupted
updates never restore old results. Commands are never automatically retried.

Keys include the exact command/argument boundaries, working directory, and optional
`--key`. Environment, executable contents, and input file contents are **not**
automatically tracked: use an additional key or refresh when they change.

Storage is `$XDG_CACHE_HOME/cacheexec`, or `$HOME/.cache/cacheexec` when XDG is
unset or empty, on both platforms. `--cache-dir PATH` overrides it. Storage uses
local files; completed results are checksummed and atomically published. Each key
has one latest result. Outputs are buffered in memory; size them to available RAM.
Network filesystem guarantees, interactive commands and PTYs are out of scope.

Missing results are cache misses. Corrupt/inaccessible storage, including during
refresh, is a tool error rather than a silent cache bypass. Tool errors print a
`cacheexec:` diagnostic to stderr and exit **125**. Argument errors exit **2**.
A normally terminated child returns its own code (including 2 or 125); distinguish
internal errors by their diagnostic. A signaled child returns **128 + signal**.
If saving fails after execution, the diagnostic explicitly states that the child
already completed and gives its exit code; the command is not repeated.

Concurrent execution sharing, propagation of wrapper signals, and cache deletion
are planned in the following implementation stages and are not yet supported.

## Development

Rust 1.98.0 is pinned in `rust-toolchain.toml`. The same checks run in macOS/Linux CI:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
