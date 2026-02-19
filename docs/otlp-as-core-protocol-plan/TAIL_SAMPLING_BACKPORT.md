# Tail Sampling and Span-Level Routing: OTel-Native Backport

This document specifies the `tail_sample` transform and span-level routing for the
OTLP-core migration. It replaces the DataDog-specific APM sampling pipeline with a
general-purpose, OTel-native implementation.

---

## 1. Why Vector Has No Tail Sampling Today

Vector's current architecture processes events one at a time through a pipeline. Tail sampling
requires **buffering a complete trace** (all spans sharing a `trace_id`) before a sampling
decision can be made. This is fundamentally at odds with the streaming model.

The DataDog agent handled this with a proprietary in-process sampler that had access to APM
stats. Vector had no equivalent.

The OTel-core migration is the right moment to add this: once spans are first-class OTel `Span`
events, the transform can group by `trace_id`, wait for a configurable window, then apply a
VRL policy to the complete trace.

---

## 2. The `tail_sample` Transform

### 2.1 Configuration

```toml
[transforms.sample_traces]
type = "tail_sample"
inputs = ["otel_source"]

# How long to buffer spans waiting for the trace to complete.
# Longer windows catch more complete traces; shorter windows reduce memory use.
timeout_secs = 10

# VRL policy evaluated against the complete trace.
# The policy receives `.spans` as an array of all spans in the trace.
policy = """
  spans_any(.spans, |span| span.attributes."http.status_code" == 500)
"""
```

### 2.2 Execution model

1. Spans arrive as individual OTel `Span` events.
2. The transform groups them by `trace_id` into an in-memory buffer.
3. After `timeout_secs` with no new span for a given `trace_id`, the trace is considered
   complete.
4. The VRL policy is evaluated against the complete trace (`{ .spans: [...] }`).
5. If the policy returns `true`, all spans are emitted downstream.
6. If the policy returns `false`, all spans are dropped.

Memory bound: configurable `max_traces` (default: 10,000). When the limit is reached, the
oldest incomplete trace is flushed immediately regardless of timeout.

### 2.3 VRL policy context

Inside the policy, the following fields are available:

| Field | Type | Description |
|---|---|---|
| `.spans` | `array<object>` | All OTel `Span` objects in the trace |
| `.trace_id` | `string` | The trace ID (hex-encoded) |
| `.span_count` | `integer` | Number of spans buffered |
| `.root_span` | `object \| null` | The span with no `parent_span_id`, if present |

Each span object follows the OTel `Span` schema:
`.name`, `.attributes`, `.status.code`, `.duration_nano`, `.start_time_unix_nano`, etc.

---

## 3. Span-Level Routing

Spans can be routed to different outputs based on span attributes using the standard `route`
transform. No special transform is needed — OTel spans are plain events.

```toml
[transforms.route_spans]
type = "route"
inputs = ["otel_source"]

[transforms.route_spans.route]
errors   = '.status.code == "STATUS_CODE_ERROR"'
slow     = '.duration_nano > 1000000000'  # > 1 second
database = '.attributes."db.system" != null'
```

For trace-level routing (route the entire trace based on any span's attributes), use
`tail_sample` with a policy that routes on the buffered `.spans` array.

---

## 4. VRL Policy Ergonomics

### 4.1 Standard VRL idiom (always works)

Any valid VRL expression returning a boolean is a valid policy. The full VRL standard library
is available.

```vrl
# Pass trace if any span has an error status
length(filter(.spans, |_index, span| span.status.code == "STATUS_CODE_ERROR")) > 0
```

```vrl
# Pass trace if the root span took more than 2 seconds
root = filter(.spans, |_index, s| s.parent_span_id == null)[0] ?? null
root != null && root.duration_nano > 2000000000
```

### 4.2 `for_each` pattern for complex policies

```vrl
# Pass trace if any span targets the payments service
found = false
for_each(.spans) -> |_index, span| {
  if span.attributes."service.name" == "payments" {
    found = true
  }
}
found
```

### 4.3 Policy shorthand types

To reduce boilerplate for the common "any span matches / all spans match" patterns, the
`tail_sample` transform supports two shorthand `type` values as an alternative to inline VRL:

#### `type = "spans_any"`

Pass the trace if **any** span satisfies the condition.

```toml
[transforms.sample_errors]
type = "tail_sample"
inputs = ["otel_source"]
timeout_secs = 10

[[transforms.sample_errors.policy]]
type = "spans_any"
condition = '.status.code == "STATUS_CODE_ERROR"'
```

Equivalent VRL:
```vrl
length(filter(.spans, |_index, span| span.status.code == "STATUS_CODE_ERROR")) > 0
```

#### `type = "spans_all"`

Pass the trace only if **all** spans satisfy the condition.

```toml
[transforms.sample_fast]
type = "tail_sample"
inputs = ["otel_source"]
timeout_secs = 10

[[transforms.sample_fast.policy]]
type = "spans_all"
condition = '.duration_nano < 100000000'  # all spans < 100ms
```

Equivalent VRL:
```vrl
length(filter(.spans, |_index, span| span.duration_nano >= 100000000)) == 0
```

#### Combining policies

Multiple policies can be combined with `operator = "and"` (default) or `operator = "or"`:

```toml
[[transforms.sample_traces.policy]]
type = "spans_any"
condition = '.status.code == "STATUS_CODE_ERROR"'

[[transforms.sample_traces.policy]]
type = "spans_any"
condition = '.duration_nano > 5000000000'

operator = "or"  # pass if errors OR slow
```

---

## 5. APM Stats as OTel Metrics

The `apm_stats` transform consumes OTel `Span` events and emits OTel `Metric` signals:

| Metric name | Type | Description |
|---|---|---|
| `trace.spans.duration` | `ExponentialHistogram` | Span duration distribution per service/operation |
| `trace.spans.errors` | `Sum` (monotonic) | Error span count per service/operation |
| `trace.spans.total` | `Sum` (monotonic) | Total span count per service/operation |
| `trace.traces.completed` | `Sum` (monotonic) | Completed trace count |

All metrics carry resource attributes (`service.name`, `deployment.environment`) and span
attributes as metric attributes.

These replace the DataDog `StatsPayload` / MessagePack APM stats. Dashboards are built in
OTel-native tooling (Grafana, Jaeger, OpenTelemetry Collector pipelines).

```toml
[transforms.apm_stats]
type = "apm_stats"
inputs = ["otel_source"]

# Aggregation window for rate/histogram metrics
window_secs = 10
```

---

## 6. Implementation Checklist

| Task | Location | Step |
|---|---|---|
| `tail_sample` transform skeleton | `src/transforms/tail_sample/` | 4 |
| Trace buffer (group by `trace_id`, timeout flush) | `src/transforms/tail_sample/buffer.rs` | 4 |
| VRL policy evaluation against `.spans` array | `src/transforms/tail_sample/policy.rs` | 4 |
| `spans_any` / `spans_all` shorthand policy types | `src/transforms/tail_sample/policy.rs` | 4 |
| `apm_stats` transform | `src/transforms/apm_stats/` | 4 |
| OTel metric emission from span data | `src/transforms/apm_stats/metrics.rs` | 4 |
| Integration test: tail sample on error spans | `tests/` | 4 |
| Integration test: apm_stats metric output | `tests/` | 4 |
