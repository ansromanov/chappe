default:
    @just --list

# The full local gate; CI runs the same checks.
check: fmt-check file-size lint test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Production files stay at or below 800 lines (the baseline can only shrink).
file-size:
    python3 scripts/check-file-size.py

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

# Fast deterministic gate for local iteration.
fast-test:
    cargo test --workspace --locked
