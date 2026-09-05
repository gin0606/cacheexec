set shell := ["bash", "-euo", "pipefail", "-c"]

check:
    ./scripts/check

# Release the version declared in Cargo.toml from a clean, committed main branch.
release:
    ./scripts/release
