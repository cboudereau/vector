# Pipeline Telemetry: All-Signal RED Metrics (Step 4a)

**Status: DEFERRED — to be implemented after Step 5 (core event model → OTel types).**

This document specifies the pipeline telemetry system that replaces the DD-specific
`apm_stats` concept with a vendor-neutral, all-signal, role-aware observability layer
emitting OTel-native metrics.

---

## 1. History and Rationale

### What was `apm_stats`

The original DataDog traces sink (`src/sinks/datadog/traces/apm_stats/`, removed in
Step 1) computed RED metrics (Rate, Errors, Duration) from spans and emitted them as a
proprietary `StatsPayload` (MessagePack) to the DD APM stats endpoint.

The initial backport plan proposed porting this as a `type = "apm_stats"` transform that
would read DD-specific predicates (`_dd.measured`, weighted hits, DD span type derivation)
and emit OTel metrics. That plan is now **cancelled** for the following reasons:

1. **The DD sinks are gone.** No consumer exists for DD-specific predicates.
2. **The scope was too narrow.** It only covered traces, but pipeline observability
   should cover all signal types (logs, metrics, traces).
3. **OTel has a standard equivalent.** The `spanmetricsconnector` in otel-col-contrib is
   the vendor-neutral approach for traces → RED metrics. Our implementation should align
   with it rather than reimplementing DD internals.
4. **Building on soon-to-be-replaced types is wasteful.** Step 5 changes the core event
   model. Internal metrics instrumentation touches every component.

### What replaces it

A **pipeline telemetry system** that:
- Covers **all three signal types** (logs, metrics, traces)
- Is **role-aware** (agent, gateway, sampler — tagged on emitted metrics)
- Emits **OTel-native metrics** consumable by any backend
- Includes a **`spanmetricsconnector`-equivalent** for traces as a subset

---

## 2. Design Principles

1. **OTel alignment.** Metric names, conventions, and semantics follow the OTel Collector
   Contrib `spanmetricsconnector` where applicable. Users following OTel tutorials should
   find familiar metric names.
2. **All signals.** Not just traces — logs and metrics get throughput/error/latency
   observability too.
3. **Role-aware.** Each Vector instance can declare its role (`agent`, `gateway`,
   `sampler`, or custom). This tag appears on all emitted telemetry metrics, enabling
   dashboards that distinguish tiers in a multi-instance deployment.
4. **Built on OTel-native types.** Implementation starts after Step 5, so it uses the
   final `Span`, `LogRecord`, `OtelMetric` types directly — no adapter layers.
5. **Configurable dimensions.** Users choose which span/log/metric attributes become
   metric labels (matching `spanmetricsconnector`'s `dimensions` config).

---

## 3. Trace Signal: `spanmetricsconnector` Equivalent

### 3.1 Output metrics

Aligned with otel-col-contrib `spanmetricsconnector` naming:

| Metric name | Type | Unit | Default dimensions |
|-------------|------|------|--------------------|
| `traces.span.metrics.calls` | Sum (configurable temporality) | `{spans}` | `service.name`, `span.name`, `span.kind`, `status.code` |
| `traces.span.metrics.duration` | Histogram (explicit or exponential) | `s` (configurable) | `service.name`, `span.name`, `span.kind`, `status.code` |

Additional user-configured dimensions (any span or resource attribute) are added on top.

### 3.2 Configuration

```toml
[transforms.span_metrics]
type = "span_metrics"
inputs = ["otel_source.traces"]

namespace = "traces.span.metrics"            # default
aggregation_temporality = "cumulative"       # or "delta"
metrics_flush_interval_secs = 60             # default

[transforms.span_metrics.histogram]
type = "exponential"                         # or "explicit"
unit = "s"                                   # or "ms"

[[transforms.span_metrics.dimensions]]
name = "http.method"
default = "GET"

[[transforms.span_metrics.dimensions]]
name = "http.status_code"

exclude_dimensions = ["status.code"]         # optional
```

### 3.3 Key differences from the cancelled `apm_stats`

| Aspect | Old `apm_stats` | New `span_metrics` |
|--------|----------------|-------------------|
| Metric names | `spans.hits`, `spans.duration.ok` | `traces.span.metrics.calls`, `traces.span.metrics.duration` |
| Dimensions | Fixed DD aggregation key | Configurable (any attribute) |
| DD-specific logic | `_dd.measured`, weighted hits, DD type derivation | None |
| Histogram type | ExponentialHistogram only | Configurable (explicit or exponential) |
| Temporality | Delta only | Configurable (delta or cumulative) |
| Error separation | Separate `ok_histogram`/`error_histogram` | `status.code` dimension on same metric |

---

## 4. Log Signal: Throughput and Error Metrics

### 4.1 Output metrics

| Metric name | Type | Unit | Dimensions |
|-------------|------|------|-----------|
| `logs.throughput` | Sum (delta) | `{logs}` | `source.name`, `severity`, `component.id` |
| `logs.bytes` | Sum (delta) | `By` | `source.name`, `component.id` |
| `logs.errors` | Sum (delta) | `{logs}` | `source.name`, `component.id`, `error.type` |

### 4.2 Configuration

```toml
[transforms.log_metrics]
type = "log_metrics"
inputs = ["otel_source.logs"]

metrics_flush_interval_secs = 60

[[transforms.log_metrics.dimensions]]
name = "service.name"
```

---

## 5. Metric Signal: Pipeline Health Metrics

### 5.1 Output metrics

| Metric name | Type | Unit | Dimensions |
|-------------|------|------|-----------|
| `metrics.throughput` | Sum (delta) | `{datapoints}` | `source.name`, `metric.type`, `component.id` |
| `metrics.cardinality` | Gauge | `{series}` | `component.id` |
| `metrics.flush_latency` | Histogram | `s` | `component.id` |

---

## 6. Role Awareness

Each Vector instance can declare its deployment role in the global config:

```toml
[global]
instance_role = "sampler"   # or "agent", "gateway", custom string
```

This value is added as `vector.instance.role` resource attribute on all emitted
telemetry metrics. Combined with the existing `vector.instance.id`, this enables
per-tier dashboards in multi-instance deployments:

```promql
rate(traces_span_metrics_calls_total{vector_instance_role="sampler"}[5m])
```

---

## 7. Relationship to Existing Internal Metrics

Vector already emits internal metrics (`vector_component_sent_events_total`,
`vector_buffer_events`, `vector_component_errors_total`, etc.) via the `metrics` crate.
These are **not replaced** — they remain for Vector-internal operational monitoring.

The pipeline telemetry described here is **user-facing** — it provides application-level
observability (RED metrics on the data flowing through the pipeline), not Vector-internal
health. The two are complementary:

| Concern | Existing internal metrics | New pipeline telemetry |
|---------|--------------------------|----------------------|
| "Is Vector healthy?" | Yes | No |
| "What's the p99 latency of my spans?" | No | Yes |
| "How many logs/sec am I processing?" | Partially (`events_total`) | Yes (with dimensions) |
| "Is my sampler tier keeping up?" | Partially | Yes (role-tagged) |

---

## 8. Why After Step 5

1. The core event model changes in Step 5. Pipeline telemetry instruments every
   component — building on the current types then rewriting is double work.
2. All-signal coverage (logs, metrics, traces) benefits from a unified event model
   where each signal type has typed fields.
3. The `span_metrics` transform needs typed `Span` fields (`.status.code`, `.kind`,
   `.attributes`) — building on `TraceEvent(LogEvent)` with `Value::get(event_path!())`
   then rewriting for typed accessors is wasteful.
4. Role-awareness fits naturally into the post-Step-5 config model.

---

## 9. Implementation Checklist

| Task | Location | Notes |
|------|----------|-------|
| `span_metrics` transform (traces → RED metrics) | `src/transforms/span_metrics/` | Aligned with `spanmetricsconnector` |
| `log_metrics` transform (logs → throughput metrics) | `src/transforms/log_metrics/` | Or integrated into `span_metrics` as multi-signal |
| Metric signal health metrics | `src/transforms/` or internal | May be part of internal metrics enhancement |
| Role-awareness config + resource attribute | `src/config/global_options.rs` | `instance_role` field |
| Configurable dimensions engine | Shared module | Reusable across signal-specific transforms |
| Configurable histogram (explicit/exponential) | Shared module | Reusable |
| Integration tests | `tests/` | Per-signal metric emission, role tagging |
| Dashboard examples (Grafana) | `docs/` | Reference dashboards for 3-tier deployment |
