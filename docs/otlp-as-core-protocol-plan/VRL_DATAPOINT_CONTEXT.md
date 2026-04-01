# Plan: VRL Broadcast for Metric Data Points

## Problem

The OTel Collector's `transform` processor uses `context: datapoint` to iterate
over data points. Vector needs an equivalent way to operate on all data points
in a metric without adding a context model.

## Solution: Two-phase approach

### Phase A (now): `.attributes` shorthand convention

On metric events, `.attributes."key"` means "all data points' attributes":
- **Write** `.attributes."key" = value` → broadcasts to ALL data points (already works)
- **Read** `.attributes."key"` → returns first data point's value (approximation, sufficient for demo)

This requires NO VRL language changes. It works today.

### Study: `[]` broadcast operator (not planned)

We investigated adding `[]` to VRL paths for explicit broadcast. Conclusion:
`.attributes` already broadcasts writes to all data points, which is sufficient.
The `[]` syntax would require modifying the VRL crate's path parser (external
dependency) and is not justified by current needs.

### Write (broadcast)

```vrl
# Set attribute on ALL data points
.data.sum.data_points[].attributes."service.name" = .resource.attributes."service.name"
```

Equivalent OTel Collector:
```yaml
context: datapoint
statements:
  - set(attributes["service.name"], resource.attributes["service.name"])
```

### Read (collect)

```vrl
# Get array of all data point values
values = .data.sum.data_points[].value
```

Returns `[42.0, 17.5, ...]` — one value per data point.

### Extends naturally to other arrays

```vrl
# Span events
.events[].attributes."processed" = true

# Span links
.links[].attributes."source" = "my-service"

# Histogram buckets
counts = .data.histogram.data_points[].count
```

### No context needed

The path itself carries the iteration semantics. No `context` config field,
no mode switching, no transform-level changes. Just a VRL path feature.

## Comparison with OTel Collector contexts

| OTel Collector | Vector VRL (`[]`) |
|---------------|-------------------|
| `context: datapoint` + `attributes["key"]` | `.data.sum.data_points[].attributes."key"` |
| `context: spanevent` + `attributes["key"]` | `.events[].attributes."key"` |
| `context: metric` + `metric.name` | `.name` (default, no `[]`) |
| `context: resource` + `resource.attributes["key"]` | `.resource.attributes."key"` |

The `[]` operator replaces the need for contexts entirely. The path is self-describing.

## Semantics

| Expression | Meaning |
|-----------|---------|
| `.data.sum.data_points[0].value` | First data point's value |
| `.data.sum.data_points[1].value` | Second data point's value |
| `.data.sum.data_points[].value` | ALL data points' values (array on read, broadcast on write) |
| `.data.sum.data_points[].attributes."key"` | ALL data points' attribute (broadcast on write) |

### Write semantics

```vrl
.data.sum.data_points[].attributes."key" = "value"
```
→ For each data point, set `attributes["key"] = "value"`.

### Read semantics

```vrl
x = .data.sum.data_points[].attributes."key"
```
→ `x` is an array: one element per data point.

### Filter (future extension)

```vrl
# Only data points where value > 100
.data.sum.data_points[.value > 100].attributes."flagged" = true
```

## Implementation

### Phase 1: `[]` write broadcast in VRL target (~200 lines)

When the VRL target processes a write path containing `[]`, iterate over all
elements at that array level and apply the write to each.

**In `vrl_target.rs` target_insert for OtelMetric:**

Currently `.attributes."key"` write calls `set_data_point_attribute` (all data points).
Extend to recognize `["data", variant, "data_points", "[]", "attributes", key]` path
pattern and do the same.

**In `vrl_target.rs` target_insert for OtelSpan:**

Recognize `["events", "[]", "attributes", key]` and iterate over span events.

### Phase 2: `[]` read collect in VRL target (~150 lines)

When the VRL target processes a read path containing `[]`, collect values from
all elements into an array.

**In the VRL projection functions:**

When building the Value from a metric/span, `.data.sum.data_points[]` returns
the full array. `.data.sum.data_points[].value` returns `[v1, v2, ...]`.

### Phase 3: Demo validation (~0 lines code)

Update the demo `vector.yaml` to use:
```vrl
.data.sum.data_points[].attributes."service.name" = .resource.attributes."service.name"
```

Verify Mimir receives the same metric labels as with the OTel Collector gateway.

## Files to modify

- `lib/vector-core/src/event/vrl_target.rs` — handle `[]` in path matching for OtelMetric/OtelSpan
- `lib/vector-core/src/event/otel_event.rs` — helper to iterate/set on all data points by variant

## Verification

- `cargo test -p vector --lib` — all tests pass
- Demo: promote_resource_attrs produces same output as OTel Collector
- Multi-data-point metric: `[]` applies to each data point
- Span events: `[]` applies to each span event
