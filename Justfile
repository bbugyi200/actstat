# actstat developer tasks. Run `just` for the default check.

# Show available recipes.
default:
    @just --list

# Build the debug binary.
build:
    cargo build

# Build the optimized release binary.
release:
    cargo build --release

# Run the CLI (pass args after `--`, e.g. `just run -- list -n 3`).
run *ARGS:
    cargo run -- {{ARGS}}

# Run the test suite.
test:
    cargo test

# Check formatting without modifying files.
fmt-check:
    cargo fmt --check

# Apply formatting.
fmt:
    cargo fmt

# Lint with clippy, treating warnings as errors.
clippy:
    cargo clippy --all-targets -- -D warnings

# Full pre-commit gate: format check, lint, test.
check: fmt-check clippy test
