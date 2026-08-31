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

corpus-gen:
    cargo run -q -p cairn-x -- corpus-gen --base-mb 128 --sequences 2 --saves 8

corpus-verify:
    cargo test -p cairn-core --test properties golden_corpus

run-server:
    RUST_LOG=info cargo run -p cairn-cli -- server --data-dir ./.cairn-server --grpc-addr 127.0.0.1:7443 --objects-addr 127.0.0.1:7444 --dev-insecure

run-daemon:
    RUST_LOG=info cargo run -p cairn-cli -- daemon

doctor:
    cargo run -p cairn-cli -- doctor

ci: fmt-check clippy test

wo1-acceptance:
    SIZE_MB=${SIZE_MB:-500} bash scripts/wo1-acceptance.sh
