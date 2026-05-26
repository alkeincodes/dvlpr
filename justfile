test:
    cargo test

check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
