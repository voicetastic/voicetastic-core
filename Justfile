default: fmt clippy test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

run-cli *ARGS:
    cargo run -p voicetastic-cli -- {{ARGS}}

run-gui:
    cargo run -p voicetastic-gui

build-release:
    cargo build --workspace --release
