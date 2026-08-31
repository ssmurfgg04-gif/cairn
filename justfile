default:
    @just --list

export PATH := "$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

build:
    cargo build --workspace

check:
    cargo check --workspace

test:
    cargo nextest run --workspace

test-full:
    cargo test --workspace

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

sim:
    CAIRN_SIM_ITERS=64 cargo test -p cairn-sim -- --nocapture

run-server:
    RUST_LOG=info cargo run -p cairn-cli -- server --data-dir ./.cairn-server --http-objects

run-daemon:
    RUST_LOG=info cargo run -p cairn-cli -- daemon

doctor:
    cargo run -p cairn-cli -- doctor

ci: fmt-check clippy test
