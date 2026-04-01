# Plan: OTTL-compatible Contexts for VRL Transforms

## Problem

The OTel Collector Contrib's `transform` processor uses **contexts** to control
what each statement iterates over and what paths are available. Vector's VRL has
no equivalent — it runs once per event with a flat path model.

## OTel Collector Context Model (complete reference)

### Hierarchy

```
Traces:    resource → scope → span → spanevent
Logs:      resource → scope → log
Metrics:   resource → scope → metric → datapoint
```

Each context iterates over its level AND has read access to all parent levels.

### Statement types and their contexts

| Statement type | Available contexts |
|---------------|-------------------|
| `trace_statements` | resource, scope, span, spanevent |
| `metric_statements` | resource, scope, metric, datapoint |
| `log_statements` | resource, scope, log |

### Context details

| Context | Iterates over | Own paths | Parent paths (read) |
|---------|--------------|-----------|-------------------|
| **resource** | Each resource | `resource.attributes`, `.cache`, `.dropped_attributes_count` | — |
| **scope** | Each scope | `instrumentation_scope.name`, `.version`, `.attributes`, `.cache` | `resource.*` |
| **span** | Each span | `span.name`, `.kind`, `.trace_id`, `.span_id`, `.parent_span_id`, `.status`, `.attributes`, `.events`, `.links`, `.start_time_unix_nano`, `.end_time_unix_nano`, `.flags`, `.cache` | `resource.*`, `instrumentation_scope.*` |
| **spanevent** | Each span event | `spanevent.name`, `.attributes`, `.time_unix_nano`, `.dropped_attributes_count`, `.event_index`, `.cache` | `resource.*`, `instrumentation_scope.*`, `span.*` |
| **log** | Each log record | `log.body`, `.severity_number`, `.severity_text`, `.attributes`, `.time_unix_nano`, `.observed_time_unix_nano`, `.trace_id`, `.span_id`, `.flags`, `.cache` | `resource.*`, `instrumentation_scope.*` |
| **metric** | Each metric | `metric.name`, `.description`, `.unit`, `.type`, `.is_monotonic`, `.aggregation_temporality`, `.data_points`, `.cache` | `resource.*`, `instrumentation_scope.*` |
| **datapoint** | Each data point | `datapoint.attributes`, `.value_double`, `.value_int`, `.count`, `.sum`, `.bucket_counts`, `.explicit_bounds`, `.scale`, `.zero_count`, `.positive.*`, `.negative.*`, `.flags`, `.time_unix_nano`, `.start_time_unix_nano`, `.exemplars`, `.quantile_values`, `.cache` | `resource.*`, `instrumentation_scope.*`, `metric.*` |

### Demo use case

```yaml
# OTel Collector Contrib
metric_statements:
  - context: datapoint
    statements:
      - set(attributes["service.name"], resource.attributes["service.name"])
```

Equivalent Vector VRL (target):
```toml
[transforms.promote_attrs]
type = "remap"
inputs = ["otlp.metrics"]
context = "datapoint"
source = '''
  .attributes."service.name" = .resource.attributes."service.name"
'''
```

## Current Vector State

Vector VRL runs **once per event**:
- Logs: one event = one OtelLog → `context: log` is the implicit default
- Traces: one event = one OtelSpan → `context: span` is the implicit default
- Metrics: one event = one OtelMetric → `context: metric` is the implicit default

**Missing**: `context: datapoint` (iterate over data points), `context: spanevent`
(iterate over span events), `context: resource`, `context: scope`.

## Design

### Config

```toml
[transforms.my_transform]
type = "remap"
inputs = ["source"]
context = "datapoint"    # optional, defaults to signal-appropriate context
source = '''
  # VRL program — paths scoped to the chosen context
'''
```

### Context values

| Value | Signal | Iterates over | Default for |
|-------|--------|--------------|-------------|
| `log` | Logs | Each log record | Log events (current behavior) |
| `span` | Traces | Each span | Trace events (current behavior) |
| `spanevent` | Traces | Each span event in each span | — |
| `metric` | Metrics | Each metric | Metric events (current behavior) |
| `datapoint` | Metrics | Each data point in each metric | — |
| `resource` | All | Each resource | — |
| `scope` | All | Each scope | — |

### Priority for implementation

1. **`datapoint`** — needed for the demo (promote resource attrs to data points)
2. **`log`** — already the implicit default, just formalize
3. **`span`** — already the implicit default, just formalize
4. **`metric`** — already the implicit default, just formalize
5. **`spanevent`** — future (iterate over span events)
6. **`resource`** / **`scope`** — future (rarely needed)

### Implementation for `context: datapoint`

When `context = "datapoint"` is set on a remap transform receiving metric events:

1. Extract data points from the metric (variant-dependent: Sum, Gauge, Histogram, etc.)
2. For each data point:
   a. Build a VRL Value with `datapoint.*` paths (attributes, value, timestamps, etc.)
   b. Include parent paths: `.resource.*`, `.scope.*`, `.metric.*` (read-only)
   c. Execute the VRL program
   d. Write back modified `datapoint.*` fields to the proto data point
3. Reassemble the metric with modified data points
4. Emit the modified metric event

Non-metric events passing through a `context = "datapoint"` transform are passed
through unchanged (with a warning).

## Phased Implementation

### Phase 1: `context: datapoint` for metrics (~300 lines)

**Files:**
- `src/transforms/remap.rs` — add `context` field to `RemapConfig`
- `lib/vector-core/src/event/vrl_target.rs` — add `DatapointVrlTarget`
- `lib/vector-core/src/event/otel_event.rs` — data point extraction/reassembly

**DatapointVrlTarget projection:**
```rust
fn datapoint_to_value(dp, resource, scope, metric) -> Value {
    // .attributes = dp.attributes
    // .value = dp.value (or .count, .sum for histograms)
    // .time_unix_nano = dp.time_unix_nano
    // .resource.attributes = resource.attributes (read)
    // .metric.name = metric.name (read)
}
```

**Data point reassembly:**
```rust
fn value_to_datapoint(value, original_dp) -> DataPoint {
    // Write back .attributes, .value, timestamps
    // Ignore .resource/.metric changes (read-only)
}
```

### Phase 2: Formalize default contexts (~50 lines)

- `context = "log"` — explicit default for log events (no behavior change)
- `context = "span"` — explicit default for trace events (no behavior change)
- `context = "metric"` — explicit default for metric events (no behavior change)

### Phase 3: `context: spanevent` for traces (~200 lines)

Same pattern as datapoint: iterate over span events within a span.

### Phase 4: `context: resource` and `context: scope` (~150 lines)

Iterate over resource/scope levels. Less common but completes the model.

## Verification

- Demo: `promote_resource_attrs` with `context = "datapoint"` matches OTel Collector output
- Multi-data-point test: each data point processed independently
- Default context: existing VRL programs work without specifying `context`
- `cargo test -p vector --lib` — all tests pass
