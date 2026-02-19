# APM Stats: OTel Backport Plan

This document specifies how to preserve the APM statistics feature — currently implemented as a
DataDog-specific side-channel in `src/sinks/datadog/traces/apm_stats/` — as a
**vendor-neutral OTel transform** that emits standard OTLP metrics.

---

## 1. What APM Stats Actually Computes

The existing implementation (`~514 lines` across `bucket.rs`, `aggregation.rs`, `weight.rs`,
`flusher.rs`) computes the following per 10-second window, grouped by
`(service, name, resource, type, http_status_code, synthetics, env, hostname, version, container_id)`:

| Statistic | Meaning | Current output |
|---|---|---|
| `hits` | Weighted count of spans processed | `u64` |
| `top_level_hits` | Weighted count of root/top-level spans | `u64` |
| `errors` | Weighted count of spans with `error != 0` | `u64` |
| `duration` | Sum of span durations (nanoseconds), weighted | `u64` |
| `ok_summary` | Latency distribution for non-error spans | `AgentDDSketch` → msgpack bytes |
| `error_summary` | Latency distribution for error spans | `AgentDDSketch` → msgpack bytes |

The current output is a DataDog-proprietary `StatsPayload` serialised as MessagePack and
sent to the DataDog APM stats endpoint. The algorithm itself is not DataDog-specific — it is
standard RED metrics (Rate, Errors, Duration) plus latency histograms, bucketed by span
attributes.

Three DataDog-specific predicates control which spans are included:
- `_top_level == 1` — span is the root of a trace
- `_dd.measured == 1` — span is explicitly flagged for metrics
- `_dd.partial_version >= 0` — span is a partial snapshot of a long-running span

In OTel, these predicates map cleanly:
- **Top-level** → `parent_span_id` is empty/zero (root span of the trace)
- **Measured** → `attributes["dd.measured"] == "1"` (preserved as an attribute if coming from DD agent source)
- **Partial snapshot** → `attributes["dd.partial_version"]` exists and is numeric

---

## 2. OTel Mapping

### 2.1 Input: OTel span fields → APM stats fields

| DD span field | OTel span field | Notes |
|---|---|---|
| `trace_id` | `.trace_id` | Hex string |
| `span_id` | `.span_id` | Hex string |
| `parent_id` | `.parent_span_id` | Hex string; empty = root span |
| `start` | `.start_time_unix_nano` | `Value::Timestamp` |
| `duration` | `.end_time_unix_nano - .start_time_unix_nano` | Computed, not stored separately |
| `error` | `.status.code == 2` (`STATUS_CODE_ERROR`) | OTel uses status code, not an `error` integer field |
| `service` | `.resources."service.name"` | OTel semantic convention |
| `name` | `.name` | Span operation name — identical |
| `resource` | `.attributes."http.route"` or `.name` fallback | OTel has no `resource` concept; closest is route or span name |
| `type` | Derived from `.kind` | `SERVER`→`web`, `CLIENT`→`http`/`db` (see §2.2) |
| `meta.http.status_code` | `.attributes."http.response.status_code"` | OTel semantic convention (stable) or `http.status_code` (deprecated) |
| `meta.env` | `.resources."deployment.environment.name"` | OTel semantic convention |
| `metrics._sample_rate` | `.attributes."sampling.rate"` or absent | Used for weight computation |
| `app_version` | `.resources."service.version"` | OTel semantic convention |
| `container_id` | `.resources."container.id"` | OTel semantic convention |
| `origin` (synthetics) | `.resources."synthetics.test.id"` or `.attributes."synthetics"` | Synthetics flag |
| `hostname` | `.resources."host.name"` | OTel semantic convention |
| `_top_level` | `parent_span_id` is empty | Structural — no DD metadata needed |
| `_dd.measured` | `.attributes."dd.measured"` | Preserved as attribute from DD agent source |
| `_dd.partial_version` | `.attributes."dd.partial_version"` | Preserved as attribute from DD agent source |

### 2.2 Span kind → span type

The DD `type` field distinguishes web, DB, cache, messaging spans. OTel encodes this via
`SpanKind` and semantic convention attributes:

| `SpanKind` | Semantic convention attributes | DD type |
|---|---|---|
| `SERVER` (2) | any | `"web"` |
| `CLIENT` (3) | `db.system` present | `"db"` |
| `CLIENT` (3) | `messaging.system` present | `"queue"` |
| `CLIENT` (3) | otherwise | `"http"` |
| `PRODUCER` (4) / `CONSUMER` (5) | any | `"queue"` |
| `INTERNAL` (1) / `UNSPECIFIED` (0) | any | `""` (empty) |

### 2.3 Output: OTel metrics

Instead of `StatsPayload` (DD MessagePack), the backported transform emits standard
**OTLP metrics** — one set per 10-second bucket flush. This feeds directly into the OTel
pipeline and reaches any backend (Prometheus, Grafana, DD via OTLP endpoint, etc.).

Each metric carries `Resource` attributes for `service.name`, `host.name`,
`deployment.environment.name`, `service.version`, `container.id`, and `synthetics`.
Additional dimension attributes are carried as data point attributes.

| Metric name | Type | Unit | Dimensions (data point attributes) |
|---|---|---|---|
| `spans.hits` | Sum (delta, int) | `{spans}` | `span.name`, `span.resource`, `span.type`, `http.status_code` |
| `spans.top_level_hits` | Sum (delta, int) | `{spans}` | `span.name`, `span.resource`, `span.type` |
| `spans.errors` | Sum (delta, int) | `{spans}` | `span.name`, `span.resource`, `span.type`, `http.status_code` |
| `spans.duration` | Sum (delta) | `ns` | `span.name`, `span.resource`, `span.type` |
| `spans.duration.ok` | ExponentialHistogram (delta) | `ns` | `span.name`, `span.resource`, `span.type` |
| `spans.duration.error` | ExponentialHistogram (delta) | `ns` | `span.name`, `span.resource`, `span.type` |

The `ExponentialHistogram` replaces `AgentDDSketch`. This is the natural OTel equivalent:
exponential histograms use the same γ-indexed bucket scheme as DDSketch (a power-of-two base
by default) and are natively supported by Prometheus, Grafana Mimir, Google Cloud Monitoring,
Datadog (via OTLP), and others.

`AgentDDSketch::gamma()` returns the exact γ value used. An `ExponentialHistogram` with
`scale` set such that `2^(2^-scale) ≈ gamma` provides a compatible bucket scheme. At
`scale = 0`, `γ = 2`; increasing `scale` halves the relative bucket width. The DD agent uses
`γ ≈ 1.005` which corresponds to OTel scale 7 (`2^(2^-7) ≈ 1.0054`).

---

## 3. Architecture: A New `apm_stats` Transform

Rather than rebuilding this inside a sink, the feature should live as a **standalone pipeline
transform**. This makes it:

- Sink-agnostic: computed statistics flow to any sink (Prometheus remote-write, OTLP, InfluxDB)
- Composable: the transform can be inserted in any topology that carries OTel spans
- Testable in isolation from any transport

```
[transforms.apm_stats]
type = "apm_stats"
inputs = ["otel_source.traces"]

# Flush interval matches the existing 10-second bucket window
flush_interval_secs = 10

# How many past buckets to keep before flushing (existing BUCKET_WINDOW_LEN = 2)
bucket_window = 2

# Resource attribute to use as the "service" dimension
service_attribute = "service.name"          # default

# Resource attribute to use as the "env" dimension
env_attribute = "deployment.environment.name"   # default

# Resource attribute for hostname
hostname_attribute = "host.name"            # default

# Span attribute for sampling rate (weight computation)
sampling_rate_attribute = "sampling.rate"   # default, absent = weight 1.0
```

The transform has **two outputs**:
- `apm_stats.spans` — pass-through of the original span events (unchanged)
- `apm_stats.stats` — the computed OTel metrics (one batch per flush tick)

```toml
[sinks.traces_out]
type = "opentelemetry"
inputs = ["apm_stats.spans"]

[sinks.metrics_out]
type = "prometheus_remote_write"   # or opentelemetry, influxdb, etc.
inputs = ["apm_stats.stats"]
```

---

## 4. Internal Design

### 4.1 Module structure

```
src/transforms/apm_stats/
  mod.rs          # transform config, registration, two-output wiring
  aggregator.rs   # port of aggregation.rs — field access updated for OTel paths
  bucket.rs       # port of bucket.rs — AgentDDSketch replaced with ExponentialHistogram
  weight.rs       # port of weight.rs — parent_span_id replaces parent_id
  flush.rs        # converts flushed buckets into OTel Metric events
  span_adapter.rs # extracts the APM-relevant fields from an OTel TraceEvent
```

### 4.2 Key changes from the existing implementation

**`span_adapter.rs` — field extraction**

Replaces direct `ObjectMap::get("service")` etc. with OTel-path lookups:

```rust
pub struct SpanView<'a> {
    trace: &'a TraceEvent,
}

impl<'a> SpanView<'a> {
    pub fn service(&self) -> &str {
        self.trace.get(event_path!("resources", "service.name"))
            .and_then(Value::as_str).unwrap_or("")
    }
    pub fn span_name(&self) -> &str {
        self.trace.get(event_path!("name"))
            .and_then(Value::as_str).unwrap_or("")
    }
    pub fn duration_ns(&self) -> u64 {
        let start = self.trace.get(event_path!("start_time_unix_nano"))
            .and_then(|v| v.as_timestamp())
            .map(|t| t.timestamp_nanos_opt().unwrap_or(0) as u64)
            .unwrap_or(0);
        let end = self.trace.get(event_path!("end_time_unix_nano"))
            .and_then(|v| v.as_timestamp())
            .map(|t| t.timestamp_nanos_opt().unwrap_or(0) as u64)
            .unwrap_or(0);
        end.saturating_sub(start)
    }
    pub fn is_error(&self) -> bool {
        // OTel STATUS_CODE_ERROR = 2
        self.trace.get(event_path!("status", "code"))
            .and_then(|v| v.as_integer())
            .map(|c| c == 2)
            .unwrap_or(false)
    }
    pub fn is_top_level(&self) -> bool {
        // Root span: parent_span_id is absent or all-zeros
        self.trace.get(event_path!("parent_span_id"))
            .map(|v| v.as_str().map(|s| s.is_empty() || s == "0000000000000000").unwrap_or(true))
            .unwrap_or(true)
    }
    pub fn is_measured(&self) -> bool {
        self.trace.get(event_path!("attributes", "dd.measured"))
            .and_then(|v| v.as_str()).map(|s| s == "1").unwrap_or(false)
    }
    pub fn weight(&self) -> f64 {
        let rate = self.trace.get(event_path!("attributes", "sampling.rate"))
            .and_then(|v| v.as_float()).map(|f| f.into_inner()).unwrap_or(1.0);
        if rate <= 0.0 || rate > 1.0 { 1.0 } else { 1.0 / rate }
    }
    pub fn span_type(&self) -> &'static str {
        let kind = self.trace.get(event_path!("kind"))
            .and_then(|v| v.as_integer()).unwrap_or(0);
        match kind {
            2 => "web",
            3 => {
                if self.trace.get(event_path!("attributes", "db.system")).is_some() { "db" }
                else if self.trace.get(event_path!("attributes", "messaging.system")).is_some() { "queue" }
                else { "http" }
            }
            4 | 5 => "queue",
            _ => "",
        }
    }
    pub fn http_status_code(&self) -> u32 {
        // Try stable OTel convention first, fall back to deprecated
        self.trace.get(event_path!("attributes", "http.response.status_code"))
            .or_else(|| self.trace.get(event_path!("attributes", "http.status_code")))
            .and_then(|v| v.as_integer()).map(|i| i as u32).unwrap_or(0)
    }
}
```

**`bucket.rs` — replace AgentDDSketch with ExponentialHistogram accumulator**

```rust
pub struct GroupedStats {
    hits:            f64,
    top_level_hits:  f64,
    errors:          f64,
    duration_sum:    f64,
    ok_histogram:    ExponentialHistogramAccumulator,
    error_histogram: ExponentialHistogramAccumulator,
}
```

`ExponentialHistogramAccumulator` is a lightweight accumulator that maintains bucket counts
at OTel scale 7 (matching DD agent γ). It implements `insert(value: f64)` and
`to_data_point(start_time, end_time) -> ExponentialHistogramDataPoint`. This replaces the
1,637-line `AgentDDSketch` with a ~100-line OTel-native accumulator — no DD algorithm, no
DD encoding.

**`flush.rs` — emit OTel Metric events**

Converts each `ClientStatsBucket` equivalent into a set of `Metric` events with OTel
`MetricValue::AggregatedHistogram` (for latency) and `MetricValue::Counter` (for counts).
Each metric carries the aggregation key fields as tags, and the Resource attributes as the
resource context.

### 4.3 Threading model

The existing implementation uses a separate OS thread (`flush_apm_stats_thread`) communicating
via a `oneshot` channel. The new transform uses Vector's standard transform model:

- The transform accumulates spans in an in-memory `Aggregator` protected by a `Mutex`.
- A `tokio::time::interval` task (10 seconds) holds a weak reference to the aggregator and,
  when it fires, locks the aggregator, calls `flush(false)`, converts the result to
  `Metric` events, and sends them to the `stats` output channel.
- On shutdown, `flush(true)` is called via the standard Vector shutdown signal.

This is consistent with how `log_to_metric` and other stateful transforms work.

---

## 5. What Is Preserved vs What Changes

| Aspect | Current (DD sink) | Backport (OTel transform) |
|---|---|---|
| 10-second bucket window | Yes | Yes — `BUCKET_DURATION_NANOSECONDS` |
| 2-bucket lookahead cache | Yes | Yes — `BUCKET_WINDOW_LEN` |
| Weighted hit counting | Yes | Yes — `weight.rs` ported |
| Error isolation (ok/error histograms) | Yes | Yes |
| Top-level span detection | `_top_level == 1` (DD tag) | `parent_span_id` is empty (structural) |
| Measured span detection | `_dd.measured == 1` | `attributes."dd.measured"` (pass-through from DD source) |
| Partial snapshot detection | `_dd.partial_version >= 0` | `attributes."dd.partial_version"` |
| Latency distribution format | `AgentDDSketch` → DD msgpack | `ExponentialHistogram` → OTel |
| Output wire format | DD `StatsPayload` MessagePack | OTel `ExportMetricsServiceRequest` |
| Output destination | DataDog APM stats endpoint only | Any sink |
| `service` dimension | DD `service` span field | `resources."service.name"` |
| `resource` dimension | DD `resource` span field | `attributes."http.route"` or `.name` |
| `type` dimension | DD `type` span field | Derived from `SpanKind` |
| Sampling rate for weight | `metrics._sample_rate` | `attributes."sampling.rate"` |
| AgentDDSketch dependency | Yes (1,637 lines) | **No** — uses OTel ExponentialHistogram |
| DataDog API key dependency | Yes | **No** — routed via normal sink config |

---

## 6. Non-Goals

- **Exact bit-for-bit parity with the DD APM stats endpoint.** The output format changes from
  DD MessagePack to OTel metrics. Users sending to the DataDog backend should use the DataDog
  OTLP endpoint, which accepts standard OTel histograms.
- **Synthetic test integration** (`TAG_SYNTHETICS`). This is preserved as a tag/dimension but
  no special routing is done.
- **DataDog `client_computed` flag.** This was always `false` in the existing implementation
  and has no OTel equivalent.

---

## 7. Migration for Existing Users

Users currently using the DataDog traces sink (which computes APM stats as a side effect)
should:

1. Add the `apm_stats` transform between their OTel source and their traces sink:

```toml
[sources.traces]
type = "opentelemetry"

[transforms.apm]
type = "apm_stats"
inputs = ["traces.traces"]

[sinks.traces_out]
type = "opentelemetry"   # or datadog_traces via OTLP
inputs = ["apm.spans"]

[sinks.metrics_out]
type = "opentelemetry"   # Grafana, DD OTLP endpoint, Prometheus, etc.
inputs = ["apm.stats"]
```

2. The metrics output is now queryable in any OTLP-compatible backend under `spans.hits`,
   `spans.errors`, `spans.duration.ok`, `spans.duration.error`, etc.

---

## 8. Implementation Checklist

| Task | Location | Notes |
|---|---|---|
| Create `src/transforms/apm_stats/` module tree | New | ~5 files |
| Implement `SpanView` adapter for OTel fields | `span_adapter.rs` | Replaces direct ObjectMap field access |
| Implement `ExponentialHistogramAccumulator` | `bucket.rs` | ~100 lines, replaces AgentDDSketch |
| Port `Aggregator` logic (bucket management, flush) | `aggregator.rs` | Algorithmic port — no DD types |
| Port weight extraction | `weight.rs` | `parent_span_id` replaces `parent_id` |
| Implement OTel metric emission on flush | `flush.rs` | Emits `Metric` events to `stats` output |
| Wire two-output transform config | `mod.rs` | `spans` pass-through + `stats` output |
| Register transform in `src/transforms/mod.rs` | Existing | Standard transform registration |
| Write golden-file tests | `tests/` | Compare output metrics for known span inputs |
| Add to `MIGRATION_STUDY.md` phase plan | Doc update | Move from "dropped" to "ported" |
| Update `PERFORMANCE_AND_TRADEOFFS.md` | Doc update | Remove "Loss of APM stats" from cons |
