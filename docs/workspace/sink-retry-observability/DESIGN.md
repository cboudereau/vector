# sink-retry-observability — Design Doc

## Context

While building the SOL pipeline monitoring dashboard ([sol-telemetry-monitoring](../sol-telemetry-monitoring/DESIGN.md)), we discovered that sink retry behavior is invisible to metrics. When a downstream service (Tempo, Loki, Mimir) goes down:

1. The OTLP HTTP sink retries with Fibonacci backoff
2. Default `retry_attempts` is `isize::MAX` (~2 billion) — effectively infinite
3. `component_errors_total` only increments when retries are **exhausted**, which practically never happens
4. Retries are logged at `warn`/`debug` level but not counted in any metric
5. The retry policy (`FibonacciRetryPolicy` in `src/sinks/util/retries.rs`) calls `build_retry()` on each attempt but emits no counter

The result: an operator watching the SOL Pipeline dashboard sees zero errors while a backend is down for hours. The only signal is in container logs, which may not be collected or queryable.

This affects all sinks that use the Tower retry service, not just OTLP HTTP.

## Functional Requirements

### <a id="fr1"></a>FR1 — Emit a retry counter metric

Each retry attempt must increment a counter metric (e.g. `component_retries_total`) with labels:
- `component_id` — which sink is retrying
- `component_kind` = `"sink"`
- `component_type` — sink type (e.g. `opentelemetry`)
- `error_type` — the retry reason category (request_failed, timeout, etc.)

### <a id="fr2"></a>FR2 — Emit a metric for in-flight retry backoff

Expose the current retry backoff duration as a gauge (e.g. `component_retry_backoff_seconds`) so operators can see when a sink is in deep backoff (minutes between attempts).

## Non-Functional Requirements

### <a id="nfr1"></a>NFR1 — No overhead when not retrying

The metric must only be registered/incremented on actual retry, not on every successful request.

### <a id="nfr2"></a>NFR2 — Works for all sinks

The implementation must be in the shared retry policy (`FibonacciRetryPolicy`), not per-sink.

## Non-goals

- Changing the default `retry_attempts` value — that's a separate configuration decision
- Adding alerting rules — those come after metrics exist
- Retry circuit-breaking or backpressure — different concern

## Rabbit holes

- **Metric cardinality**: adding `error_type` and `reason` labels could explode cardinality if reasons are free-form strings. Cap: use a fixed set of error categories, not raw error messages.

## Design

The fix is in `FibonacciRetryPolicy::build_retry()` (`src/sinks/util/retries.rs`). This method is called on every retry attempt. Add a `counter!("component_retries_total", ...)` increment there.

For the backoff gauge, set it in `build_retry()` to `self.backoff().as_secs_f64()` and clear it (set to 0) when the request succeeds.

Decisions: (none yet — straightforward implementation)

## Cross-cutting Concerns

- **Dashboard**: once the metric exists, add a "Retry rate" panel to the SOL Pipeline dashboard
- **All sinks affected**: this improves observability for every sink, not just OTLP
