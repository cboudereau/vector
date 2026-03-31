# Plan: OTel-native VRL target for OtelMetric

## Problem

The VRL target for `OtelMetric` uses a **restricted legacy path model**:

| Current VRL path | Actual proto path | How it works |
|-----------------|-------------------|-------------|
| `.name` | `metric.name` | Direct read/write ✓ |
| `.description` | `metric.description` | Direct read/write ✓ |
| `.unit` | `metric.unit` | Direct read/write ✓ |
| `.resource` | `Resource` | Direct read/write ✓ |
| `.scope` | `InstrumentationScope` | Direct read/write ✓ |
| `.tags` | Data point attributes | **Legacy bridge** — goes through `to_legacy_metric()` then `set_data_point_attribute()` |
| `.kind` | Aggregation temporality | **Legacy bridge** — read-only via `to_legacy_metric()` |

**Missing paths** (not accessible in VRL at all):
- `.data` — the metric data variant (Sum, Gauge, Histogram, etc.)
- `.data.sum.data_points[n].attributes` — per-data-point attributes
- `.data.sum.data_points[n].value` — the actual value
- `.data.sum.aggregation_temporality` — delta vs cumulative
- `.data.sum.is_monotonic` — counter vs gauge-like sum
- `.data.histogram.data_points[n].bucket_counts` — histogram buckets
- `.data.gauge.data_points[n].value` — gauge value

**Consequences:**
1. Users can't inspect or modify metric data point values in VRL
2. `.tags` bridge is lossy — clones the entire metric, converts to legacy, extracts tags
3. `.kind` is read-only — can't change temporality
4. No way to access histogram buckets, data point timestamps, etc.
5. Can't do the OTel Collector's `transform` processor equivalent properly

## Goal

Expose the full OTLP metric proto structure as VRL paths, matching how OtelSpan
already works (direct proto → Value projection with OTel field names).

## Target VRL Paths

Following the OTLP proto message structure:

```
.name                                          → String
.description                                   → String
.unit                                          → String
.resource.attributes."key"                     → AnyValue
.scope.name                                    → String
.scope.version                                 → String
.data.type                                     → "sum" | "gauge" | "histogram" | "exponential_histogram" | "summary"

# For Sum:
.data.sum.is_monotonic                         → Boolean
.data.sum.aggregation_temporality              → Integer (1=delta, 2=cumulative)
.data.sum.data_points                          → Array
.data.sum.data_points[n].attributes."key"      → AnyValue
.data.sum.data_points[n].value                 → Float or Integer
.data.sum.data_points[n].time_unix_nano        → Integer
.data.sum.data_points[n].start_time_unix_nano  → Integer

# For Gauge:
.data.gauge.data_points                        → Array
.data.gauge.data_points[n].attributes."key"    → AnyValue
.data.gauge.data_points[n].value               → Float or Integer
.data.gauge.data_points[n].time_unix_nano      → Integer

# For Histogram:
.data.histogram.aggregation_temporality        → Integer
.data.histogram.data_points                    → Array
.data.histogram.data_points[n].count           → Integer
.data.histogram.data_points[n].sum             → Float
.data.histogram.data_points[n].bucket_counts   → Array of Integer
.data.histogram.data_points[n].explicit_bounds → Array of Float
.data.histogram.data_points[n].attributes."key"→ AnyValue

# For ExponentialHistogram:
.data.exponential_histogram.data_points[n].count          → Integer
.data.exponential_histogram.data_points[n].sum            → Float
.data.exponential_histogram.data_points[n].scale          → Integer
.data.exponential_histogram.data_points[n].zero_count     → Integer
.data.exponential_histogram.data_points[n].positive.offset       → Integer
.data.exponential_histogram.data_points[n].positive.bucket_counts→ Array of Integer

# For Summary:
.data.summary.data_points[n].count             → Integer
.data.summary.data_points[n].sum               → Float
.data.summary.data_points[n].quantile_values   → Array of {quantile, value}
```

## Backward Compatibility

Keep `.tags` and `.kind` as **deprecated aliases** for one release cycle:
- `.tags` → reads/writes `.data.{variant}.data_points[0].attributes`
- `.kind` → reads `.data.{variant}.aggregation_temporality` (where applicable)

The `vector vrl-migrate` tool should add rules:
- `.tags."foo"` → `.data.sum.data_points[0].attributes."foo"` (or generic form)
- `.kind` → `.data.sum.aggregation_temporality` (context-dependent)

## Design

Follow the OtelSpan pattern (`otel_span_event_to_value` at vrl_target.rs:234):

1. **Read path** (`precompute_otel_metric_value`): Convert full proto to `Value::Object`
   with nested structure matching the proto message definition. Lazy — only projects
   paths that the VRL program actually references (via `ProgramInfo.target_queries`).

2. **Write path** (target_insert for OtelMetric): Match on path segments to modify
   the proto directly. For data point attributes: find the variant, index into
   `data_points[n]`, set the attribute.

3. **Write-back** (`into_events`): Already works — the `OtelMetric` event is returned
   directly. Proto modifications from the write path persist.

## Phases

### Phase 1: Read path — full proto projection (~200 lines)

Replace `precompute_otel_metric_value` with a full proto → Value projection:
- `otel_metric_to_value(event: &OtelMetric) -> Value`
- Maps each proto field to its VRL path
- Handles all 5 data variants (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
- Preserves lazy projection (only materialized paths the VRL program touches)

**File:** `lib/vector-core/src/event/vrl_target.rs`

### Phase 2: Write path — proto modifications (~200 lines)

Extend the target_insert match for OtelMetric:
- Match full proto paths: `.data.sum.data_points[n].attributes."key"`, etc.
- Modify the proto directly (no legacy bridge)
- Support inserting new data points, setting values, modifying attributes

**File:** `lib/vector-core/src/event/vrl_target.rs`

### Phase 3: Backward compat aliases + VRL migration rules (~100 lines)

- `.tags` reads/writes first data point's attributes (deprecated)
- `.kind` reads aggregation temporality (deprecated)
- Add migration rules to `src/vrl_migrate/rules/metric.rs`
- Deprecation warnings in logs when legacy paths are used

### Phase 4: Tests + documentation (~150 lines)

- Unit tests: read/write for each data variant
- Round-trip test: VRL modifies metric, verify proto changes persist
- Update VRL docs with new metric paths

## Example: After Migration

Current (legacy bridge):
```vrl
.tags."service.name" = .resource.attributes."service.name"
```

After (OTel-native):
```vrl
# Explicit: set attribute on first data point of a Sum metric
.data.sum.data_points[0].attributes."service.name" = .resource.attributes."service.name"

# Or use shorthand (applied to all data points):
.attributes."service.name" = .resource.attributes."service.name"
```

## References

- OtelSpan VRL projection: `vrl_target.rs:234-250` (the model to follow)
- OtelMetric current impl: `vrl_target.rs:374-426` (to be replaced)
- OTLP metric proto: `opentelemetry-proto/opentelemetry/proto/metrics/v1/metrics.proto`
