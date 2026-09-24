# cacheexec

[日本語](README.ja.md)

Cache the stdout, stderr and exit code of a non-interactive command on macOS and Linux.

cacheexec is a small tool for reusing the result of commands that are expensive or unnecessary to repeat, such as API queries, condition checks and build or deployment helper scripts. It runs commands directly, preserves their output and status, and skips execution while the saved result remains eligible.

## Installation

With Homebrew:

```sh
brew install gin0606/tap/cacheexec
```

With Nix:

```sh
nix profile install github:gin0606/cacheexec
```

Or build the tagged source with Cargo:

```sh
cargo install --git https://github.com/gin0606/cacheexec --locked --bin cacheexec
```

For development, install from a local checkout with `cargo install --path . --locked --bin cacheexec`.

## Quick start

```sh
cacheexec --ttl 5m --include-codes 0 -- curl -fsS https://example.com/status
cacheexec --ttl 5m --include-codes 0,1 -- ./condition-check.sh
cacheexec --ttl 5m --key "$DEPLOY_ENV" --refresh -- ./query
```

Run the same invocation again while its saved result is eligible to replay the output and exit code instead of executing the command.

## Important behavior

- **Nonzero exit codes are cached by default.** Use `--include-codes 0` when transient failures must not be reused.
- The command **must follow `--`** and is executed directly without shell interpretation. To cache a complete shell expression, such as a pipeline, invoke `sh -c` explicitly.
- Keys include the command, exact argument boundaries, working directory and optional `--key`. Environment variables, executable contents and input file contents are not automatically tracked.
- The child inherits the environment but receives closed stdin. Interactive commands and PTYs are out of scope.
- Outputs are buffered in memory. Replay preserves bytes, but not timing or ordering between stdout and stderr.

## Command execution and output

The child streams its stdout and stderr while running. A cache hit skips execution and replays each byte stream and the original exit code. Non-UTF-8 bytes are preserved. Stream timing and ordering between stdout and stderr are not preserved on replay. Diagnostics are added only when `--verbose` is specified.

To cache the result of an entire pipeline, pass the expression as the command string for `sh -c`:

```sh
cacheexec --ttl 5m -- sh -c './query | jq .result'
```

Shell syntax outside that command string is handled by the shell that invoked cacheexec. For example, `cacheexec --ttl 5m -- ./query > result.json` writes both live and replayed output to the file on every invocation. Putting the redirection inside `sh -c` instead makes the file write part of the skipped command on a cache hit.

Commands are never automatically retried. Start failures and signal termination are never saved.

## TTL, exit-code selection and refresh

`--ttl` is required for command execution and accepts durations such as `500ms`, `30s`, `5m` and `1h`. Age is measured from execution completion, and a result is reusable at age **<= TTL**. `0s` effectively requests a new execution. A clock earlier than the completion time makes the result ineligible. Changing TTL uses the same key.

All normal exit codes (0–255) are saved by default. `--include-codes 0,1` saves and reuses only those codes; `--exclude-codes 2,3` excludes them. These options are mutually exclusive, and each invocation applies its own policy to existing results.

`--refresh` invalidates the previous result before starting again. Ineligible, failed or interrupted updates never restore old results.

For a condition script, use:

```sh
cacheexec --ttl 5m --include-codes 0,1 -- ./condition-check.sh
```

Codes 0 and 1 are reusable, while other codes and their stderr are returned each time. cacheexec attaches no application meaning to codes. After clearing the saved result, the next call runs the script again.

## Inspecting cache behavior

```sh
cacheexec --ttl 5m --verbose -- ./query
```

`--verbose` adds human-readable diagnostics to **stderr**, prefixed with `cacheexec: verbose:`. For example (wording and duration formatting may change):

```text
cacheexec: verbose: run reason=missing ttl=5m key=… cache-dir="/…/cacheexec"
cacheexec: verbose: completed exit=0 saved=yes
```

A decision is reported when made, without waiting for execution or a join to finish: `hit` reuses a saved result, `run` starts a command, and `join` waits for an existing same-key execution. An active execution takes priority over TTL, code selection and refresh. Run reasons have this priority: `refresh`, `missing`, `future-timestamp`, `expired`, then `policy`. Decisions include the applied TTL, key hash and absolute cache directory. When a saved result was evaluated, `age` shows seconds since completion (negative for future timestamps).

Completion reports the exit result and whether that generation was saved. `reason=participant-policy` means no participant allowed the exit code; `reason=reused` means a hit made no new save. Interruptions, failures and `output-closed` (an output reader closed early) are identified separately from normal completion. A join reports the shared generation's saving outcome, including another participant's permission to save. `saved=unknown` means it could not be established, such as when only the waiter was interrupted before publication or the owner disappeared.

Diagnostics are generated per invocation and never stored in cached stdout/stderr. Owners and waiters each honor their own `--verbose`; it does not change keys, reuse rules, saving policies or sharing. Diagnostic fields omit commands, arguments, environment values, extra-key values and child output. Paths and control characters are escaped. The prefix distinguishes diagnostics from child stderr and tool errors, but a child can print the same prefix.

Delivery is **best effort**: diagnostic write failures, closed pipes and stalled readers cannot prevent execution, publication, saving or exit. Diagnostics may be dropped or truncated; prompt delivery is not guaranteed to a blocked reader. Existing child-output transfer behavior is unchanged. This is not a stable format for machine parsing; there is no JSON format or environment-variable switch. `--clear --verbose` is an argument error; cleanup retains its count summary.

## Keys and storage

Use an additional key or refresh when an environment variable, executable or input file affects the result:

```sh
cacheexec --ttl 5m --key "$DEPLOY_ENV" -- ./query
```

Storage is `$XDG_CACHE_HOME/cacheexec`, or `$HOME/.cache/cacheexec` when XDG is unset, empty or relative, on both platforms. `--cache-dir PATH` overrides it. Storage uses local files; completed results are checksummed and atomically published. Each key has one latest result. Outputs are buffered in memory, so size them to available RAM. Network filesystem guarantees are out of scope. Use a cache directory that only you can write to: anyone who can write there can replace results that later calls replay.

## Exit codes and errors

| Outcome | Exit code |
|---|---|
| Normal child exit | Child code (0–255) |
| Signal interruption | 128 + signal |
| Output reader closed early (e.g. `\| head`) | The command's own code, without a diagnostic |
| Tool error | 125, with a `cacheexec:` diagnostic |
| Invalid arguments | 2 |

Missing results are cache misses. Corrupt or inaccessible storage, including during refresh, is a tool error rather than a silent cache bypass. Tool errors print a `cacheexec:` diagnostic to stderr and exit **125**. Argument errors exit **2**. A normally terminated child returns its own code (including 2 or 125); distinguish tool errors by their diagnostic. A signaled child returns **128 + signal**. When the reader of stdout or stderr closes before all output is written, as with `cacheexec ... | head -1`, that invocation stops writing to the closed stream, keeps delivering the other one, and exits with the command's own status without a diagnostic. This does not stop the command, unlike SIGPIPE: it still runs to completion and its output is still collected, shared and saved as usual, so its real status is known, and a command with unbounded output such as `yes` never finishes. If the other stream fails for another reason, the invocation exits 125 instead. If saving fails after execution, the diagnostic explicitly states that the child already completed and gives its exit code; the command is not repeated.

## Concurrent execution and interruption

Simultaneous calls for the same key share one execution, including `--refresh` and calls with different TTLs or code selections. The execution owner streams output immediately; waiters receive complete stdout, stderr and status when it finishes, even if their code policy excludes that result. If any participant allows the normal exit code, it is saved for subsequent eligible calls. Otherwise it is held only for existing waiters. Each waiter keeps its own execution generation, so later executions cannot overwrite its result. Different keys run independently. A refresh already in progress takes precedence over the previous saved result.

After the child exits and both output streams are collected, the owner commits to publishing the shared result. Saving and publication do not wait for delivery to the owner's output consumer, so a stalled consumer cannot hold up waiters. A closed reader or another delivery error affects only that invocation, as described above; it does not invalidate shared or saved results.

SIGHUP/SIGINT/SIGQUIT/SIGTERM received by the owner before the publication decision cancel the shared execution. While executing or collecting output, signals are forwarded to the child process group. The child runs in its own process group, so terminal-generated signals reach it only through this forwarding; job-control stops such as Ctrl-Z suspend cacheexec but not the child. Any of these four signals already ignored when cacheexec starts, for example SIGHUP under `nohup`, stays ignored by cacheexec and the child. All participants receive 128 + signal and the result is not saved, even if the child handles the signal and exits normally.

After the publication decision, signals cancel only the owner's delivery with 128 + signal; waiters and saved results are unaffected. Racing signals are ordered at a single decision point. Subsequent storage errors during publication still fail the shared execution. A signal sent only to a waiter ends that wait with 128 + signal and leaves the shared execution running. This also applies while replaying a completed result, even if the output consumer stops reading. Interrupting output delivery can truncate that invocation's output or diagnostics. There is no child timeout: children that ignore these signals can continue to run. Descendants that leave the child process group are outside signal propagation guarantees.

If the owner dies suddenly (including SIGKILL) before publishing the shared result, waiters fail with a tool error without retrying. Death after publication leaves shared and saved results intact. Kernel locks identify execution ownership, avoiding PID reuse checks. A subsequent invocation can start a new execution; any child orphaned by SIGKILL may still be running and is not automatically killed. Shared temporary results are unlinked on completion and stay accessible through existing waiters' open descriptors. An abandoned marker is reclaimed by the next same-key call. Do not remove or replace lock files while invocations are running. Filesystem or lock failures are explicit errors. Use the cleanup options below for deletion.

## Recovering from corrupt results

Corruption errors identify the affected `.result` file and include recovery steps. Stop all cacheexec invocations using that cache directory, remove only the named `.result` file, then retry the original command. Keep `.lock` and `.active` files. Neither `--refresh` nor `--clear` silently discards corrupt results.

## Explicit cleanup

```sh
cacheexec --cache-dir ./cache --clear
cacheexec --cache-dir ./cache --clear --older-than 24h
```

Cleanup takes no command or TTL. `--clear` deletes all idle reusable results; `--older-than` restricts deletion to completion ages strictly greater than the specified duration (the boundary and future timestamps are retained). This age is independent of any reader's TTL. Cleanup scans once; keys created concurrently may remain for the next cleanup. There is no automatic cleanup or background service.

The summary reports removed results, reclaimed abandoned execution markers, skipped busy keys and failures. Busy keys are skipped successfully, so run cleanup again later if needed. Active executions and undelivered waiter generations are preserved, even across refresh and new executions. Missing directories succeed without creating them. Corruption, permissions and deletion failures exit 125 with a diagnostic and partial counts; successful deletions are not rolled back. Corrupt results are kept for inspection. Cleanup installs no signal handlers, so a signal that is not ignored ends it immediately by its default action (a shell reports 128 + signal) rather than by an exit status; deletions already made stay done. A summary reader that closes early does not fail cleanup; any other failure to print the summary exits 125 with the counts in its diagnostic, after the deletions. The `.lock` file of a key left with neither a result nor an execution is removed as well; callers already waiting on it detect the removal and lock its replacement. Anonymous save temporary files and unrecognized files are retained because cleanup cannot safely identify their owner; remove such leftovers manually only after all cacheexec invocations have stopped. Never recursively delete the cache directory while invocations are running.

## Development

Rust 1.98.0 is pinned in `rust-toolchain.toml`. The same checks run in macOS/Linux CI:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`nix flake check` builds and tests the Nix package. It rebuilds every dependency,
so `just check` leaves it to CI and `just release`. Releases use the version in
`Cargo.toml`; pushing the matching `vX.Y.Z` tag publishes the platform archives
and updates `gin0606/homebrew-tap`. From a clean, committed `main` branch,
`just release` validates the repository and pushes that tag.
