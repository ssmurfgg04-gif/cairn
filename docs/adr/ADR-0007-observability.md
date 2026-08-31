# ADR-0007: Observability — trace_id propagation now; OTLP export + Sentry behind flags

Date: 2026-08-31 · Status: Accepted

## Context
Spec §16 wants tracing + OpenTelemetry end-to-end from W1 and Sentry for client crashes. The
opentelemetry-otlp stack is a heavy dependency tree and requires a collector to be meaningful;
Sentry requires a DSN. Neither collector nor DSN exists in the v4 headless environment.

## Decision
1. `tracing` everywhere with structured fields; a 32-hex `trace_id` is generated at client
   operation start and propagated verbatim: gRPC metadata (`x-cairn-trace-id`), HTTP headers,
   object-store request notes, and audit rows. Logs carry it; server spans attach it. This is
   the contract OTel will plug into.
2. Metrics implemented as a dependency-free Prometheus text registry in `cairn-server`
   (`/metrics`): counters/histograms for the named SLO series (§16 list), with the I1 gauge
   reported by the daemon from hydration instrumentation.
3. OTLP export is flag-gated (`config_flags.otel_endpoint`); when set, spans are exported via a
   future `cairn-x`/ops adapter — the trace_id contract above makes this additive, not
   structural. Sentry crash capture is stubbed at a single integration point
   (`cairn_cli::telemetry::capture_crash`) for the same reason.

## Consequences
- Zero-dependency metrics from W1; SLOs alertable (Prometheus format) without OTel.
- Deviation from "opentelemetry + Sentry" crate list recorded here; §16 metric names and
  thresholds are unchanged and implemented.
