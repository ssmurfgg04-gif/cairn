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

# WO6-9: security sweep (RustSec advisories, secret-shape scan, unsafe policy,
# path-traversal gates, TLS fail-closed, I3 scoping, token logging, scope checks).
security:
    bash scripts/security-sweep.sh

wo1-acceptance:
    SIZE_MB=${SIZE_MB:-500} bash scripts/wo1-acceptance.sh

# WO6-4: S3 wire-conformance against a server YOU OWN (CI runs the same suite against
# an ephemeral MinIO). Reads CAIRN_S3_* (endpoint/bucket/region/access/secret) — the
# same env the server backend uses. NEVER point this at buckets you do not own.
s3-conformance:
    cargo build --release -p cairn-x
    ./target/release/cairn-x s3-conformance --i-own-the-target

tls-dev-cert:
    #!/bin/sh
    mkdir -p ./.cairn-server/keys/tls-dev
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
      -keyout ./.cairn-server/keys/tls-dev/server.key \
      -out ./.cairn-server/keys/tls-dev/server.pem \
      -days 365 -nodes -subj "/CN=localhost" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
    echo "dev TLS cert: ./.cairn-server/keys/tls-dev/server.pem"
    echo "run server with: --tls-cert ./.cairn-server/keys/tls-dev/server.pem --tls-key ./.cairn-server/keys/tls-dev/server.key"
    echo "login with:      cairn login --server https://localhost:7443 --ca ./.cairn-server/keys/tls-dev/server.pem --code <code>"

# WO6-4 soak: 5GB-class ingest, kill -9 at ~50%, resume, byte-identity,
# zero-dup journal, COLD-FETCH first byte. Needs ~3.2x SIZE_MB free disk.
# DRY-RUN (no CAIRN_S3_*) = LocalFs objects; set CAIRN_S3_* for the REAL wire.
soak-5gb SIZE_MB="5000":
    SIZE_MB={{SIZE_MB}} bash scripts/soak.sh

# quick validation variant (CI-parity scale, ~2GB disk)
soak-quick:
    SIZE_MB=400 bash scripts/soak.sh
