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

Simultaneous calls for the same key share one execution, including `--refresh`
and calls with different TTLs or code selections. The execution owner streams
output immediately; waiters receive complete stdout, stderr and status when it
finishes, even if their code policy excludes that result. If any participant
allows the normal exit code, it is saved for subsequent eligible calls. Otherwise
it is held only for existing waiters. Each waiter keeps its own execution generation,
so later executions cannot overwrite its result. Different keys run independently.
A refresh already in progress takes precedence over the previous saved result.

SIGINT/SIGTERM sent to the execution owner are forwarded to the child process group;
all participants receive 128 + signal, and the result is not saved, even if the child
handles the signal and exits normally. A signal sent only to a waiter ends that
wait with 128 + signal and leaves the shared execution running. This also applies
while replaying a completed result, even if the output consumer stops reading.
Interrupting output delivery can truncate that invocation’s output or diagnostics. There is no child
timeout: children that ignore these signals can continue to run. Descendants that
leave the child process group are outside signal propagation guarantees.

If the owner dies suddenly (including SIGKILL), waiters fail with a tool error
without retrying. Kernel locks identify execution ownership, avoiding PID reuse
checks. A subsequent invocation can start a new execution; any child orphaned by
SIGKILL may still be running and is not automatically killed. Shared temporary
results are unlinked on completion and stay accessible through existing waiters'
open descriptors. An abandoned marker is reclaimed by the next same-key call.
Do not remove or replace lock files while invocations are running. Filesystem or
lock failures are explicit errors. Cache deletion is not yet supported.

## Development

Rust 1.98.0 is pinned in `rust-toolchain.toml`. The same checks run in macOS/Linux CI:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
