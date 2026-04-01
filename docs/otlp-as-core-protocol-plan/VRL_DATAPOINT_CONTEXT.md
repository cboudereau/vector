# Plan: VRL Datapoint Context for Metrics

## Problem

The OTel Collector Contrib's `transform` processor has a **per-data-point iteration** model:

```yaml
metric_statements:
  - context: datapoint
    statements:
      - set(attributes["service.name"], resource.attributes["service.name"])
```

This means: **for each data point** in the metric, execute the statement. Each data point
gets its own `attributes` scope. The processor iterates automatically.

Vector's VRL runs **once per event** (one event = one OtelMetric). An OtelMetric can contain
multiple data points. There is no iteration over data points — VRL sees the whole metric.

### Current behavior

| Operation | Vector VRL | OTel Collector (`context: datapoint`) |
|-----------|-----------|--------------------------------------|
| **Read** `.attributes."key"` | First data point only | Current data point (iterating) |
| **Write** `.attributes."key"` | ALL data points (`set_data_point_attribute`) | Current data point |
| **Read** `.resource.attributes."key"` | Metric's resource | Same (shared) |
| **Iteration** | None — one pass per metric | Automatic per data point |

### Consequences

1. **Write is correct** for the common case (promote resource attrs to all data points)
2. **Read returns first only** — wrong if data points have different attributes
3. **No per-data-point logic** — can't do "if this data point has attribute X, set Y"
4. VRL programs using `.attributes` look correct but have different semantics than OTel Collector

## OTel Collector Contexts (reference)

From the OTel Collector Contrib `transform` processor, metrics have 4 contexts:

| Context | Scope | Paths available |
|---------|-------|----------------|
| `resource` | Once per resource | `resource.attributes`, `resource.dropped_attributes_count` |
| `scope` | Once per scope | `instrumentation_scope.name`, `.version`, `.attributes` |
| `metric` | Once per metric | `metric.name`, `.description`, `.unit`, `.type`, `.is_monotonic`, `.aggregation_temporality`, `.data_points` |
| `datapoint` | **Once per data point** | `attributes`, `value_double`, `value_int`, `count`, `sum`, `bucket_counts`, `explicit_bounds`, `start_time_unix_nano`, `time_unix_nano`, `flags`, `exemplars`, `quantile_values` + all parent paths |

Key: in `context: datapoint`, `attributes` refers to the **current** data point's attributes.
The processor loops over all data points automatically.

## Design

### Option A: Add `context` field to remap transform (Recommended)

Add a `context` field to RemapConfig that controls iteration:

```toml
[transforms.promote_attrs]
type = "remap"
inputs = ["otlp.metrics"]
context = "datapoint"    # NEW: iterates per data point
source = '''
  .attributes."service.name" = .resource.attributes."service.name"
'''
```

When `context = "datapoint"`:
1. For each data point in the metric, execute the VRL program
2. `.attributes` refers to the current data point's attributes (read AND write)
3. `.resource`, `.scope`, `.metric` refer to parent objects (read-only)
4. After all iterations, reassemble the metric with modified data points

When `context` is absent (default): current behavior (one pass per metric).

**Paths in datapoint context:**

```
.attributes."key"                → current data point attributes (read/write)
.value                           → NumberDataPoint value (read/write)
.count                           → count field (Histogram/Summary)
.sum                             → sum field
.time_unix_nano                  → data point timestamp
.start_time_unix_nano            → start timestamp
.bucket_counts                   → Histogram bucket counts
.explicit_bounds                 → Histogram bounds
.flags                           → data point flags
.resource.attributes."key"       → metric resource (read-only)
.scope.name                      → scope name (read-only)
.metric.name                     → metric name (read-only)
.metric.description              → metric description (read-only)
.metric.unit                     → metric unit (read-only)
```

### Option B: Implicit iteration (like OTel Collector's context inference)

When VRL accesses `.attributes` on a metric event, automatically iterate over all
data points. No config change needed. More magical but matches OTel Collector behavior.

**Rejected** because: VRL's type system expects deterministic paths. Implicit iteration
would change the return type of `.attributes` depending on context (single map vs iteration).

## Implementation

### Phase 1: DatapointContext VRL projection (~200 lines)

When `context = "datapoint"`, create a different VRL projection:

```rust
fn otel_datapoint_to_value(
    data_point: &NumberDataPoint,  // or HistogramDataPoint, etc.
    resource: &Resource,
    scope: &InstrumentationScope,
    metric: &OtelMetricProto,
) -> Value
```

This produces a flat Value with:
- `.attributes` from the current data point
- `.value` / `.count` / `.sum` from the current data point
- `.resource.attributes` from the metric's resource
- `.metric.name` from the metric proto

### Phase 2: DatapointContext iteration in remap transform (~150 lines)

In the remap transform, when `context = "datapoint"`:
1. Extract all data points from the metric
2. For each data point, create a VRL Value, run the program, collect result
3. Reassemble the metric with modified data points
4. Emit the modified metric event

### Phase 3: Write-back from datapoint Value to proto (~150 lines)

After VRL runs on a data point Value, write modifications back:
- `.attributes` changes → update data point attributes
- `.value` changes → update data point value
- Other data point fields → update accordingly
- `.resource` / `.metric` changes → ignored (read-only in datapoint context)

### Phase 4: Tests + docs (~100 lines)

- Test: promote resource attrs to data points (matches OTel Collector behavior)
- Test: per-data-point filtering (set attribute only if value > threshold)
- Test: multi-data-point metric (verify each data point processed independently)
- Update VRL docs with `context` field documentation

## Files to modify

- `src/transforms/remap.rs` — add `context` field to `RemapConfig`, branch on it
- `lib/vector-core/src/event/vrl_target.rs` — add `DatapointVrlTarget` variant
- `lib/vector-core/src/event/otel_event.rs` — data point extraction/reassembly helpers

## Verification

- `cargo test -p vector --lib` — all tests pass
- Demo: `promote_resource_attrs` produces same output as OTel Collector
- Multi-data-point test: each data point processed independently
