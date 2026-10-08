# Cairn meeting point — multi-stage image (builder -> slim runtime).
#
# Build from the REPO ROOT (the source tree is the build context):
#   docker build -f deploy/Dockerfile -t cairn-server .
#   docker compose -f deploy/docker-compose.yml up -d     # does the same
#
# Toolchain note: rust-toolchain.toml pins the exact channel (1.98.0), and the
# rustup in the base image honors it on first build — the image floats, the
# compiler does not.
#
# tokio_unstable (do not "clean up"): the workspace enables tokio's io-uring
# feature (ADR-0025) which hard-errors without --cfg tokio_unstable. The flag
# comes from .cargo/config.toml, copied below like every dev/CI build.
# Do NOT set env RUSTFLAGS here — it would REPLACE the config flags (the
# release.yml trap).

FROM rust:1-bookworm AS builder
WORKDIR /src

# Dependency layer first (Cargo.lock committed — reproducible resolve)
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/ .cargo/
COPY proto/ proto/
COPY crates/ crates/

RUN cargo build --release -p cairn-cli --bin cairn

FROM debian:bookworm-slim AS runtime

# ca-certificates: SigV4 bucket endpoints are HTTPS (ADR-0005)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user; the state volume is /var/lib/cairn (chown'd here so
# the named volume inherits the ownership on first mount).
RUN useradd --system --create-home --home-dir /var/lib/cairn --shell /usr/sbin/nologin cairn \
    && mkdir -p /var/lib/cairn \
    && chown -R cairn:cairn /var/lib/cairn

COPY --from=builder /src/target/release/cairn /usr/local/bin/cairn

# cold-tier directory (run.rs: CAIRN_COLD_DIR wins; S3 deployments without a
# cold dir skip tiering — never fake-cold)
ENV CAIRN_COLD_DIR=/var/lib/cairn/cold

WORKDIR /var/lib/cairn
VOLUME /var/lib/cairn

# 7443/tcp  gRPC metadata plane (journal, leases, auth, upload/download)
# 7444/tcp  dev local-fs objects endpoint (production: real bucket via CAIRN_S3_*)
EXPOSE 7443 7444

USER cairn

# Honest healthcheck: the server has NO dedicated /healthz HTTP endpoint yet
# (the only HTTP surface is the signed /objects/* route, which a plain probe
# cannot authenticate) — tracked as a TODO for the next server round. Until
# then the check is a TCP connect to the gRPC listener, which proves the
# process is alive and the metadata plane is accepting.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/7443' || exit 1

ENTRYPOINT ["cairn"]
# Shape A of the runbook: plaintext loopback-style behind a TLS proxy. For
# server-native TLS add --tls-cert/--tls-key and mount the PEMs (runbook §TLS).
CMD ["server", "--data-dir", "/var/lib/cairn", "--grpc-addr", "0.0.0.0:7443", "--objects-addr", "0.0.0.0:7444"]
