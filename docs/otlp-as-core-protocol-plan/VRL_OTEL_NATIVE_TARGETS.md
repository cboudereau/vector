# Plan: OTel-native VRL Targets for All Signals

## Problem

The VRL targets for OTel events have inconsistent proto fidelity:

| Signal | Current approach | Proto fidelity | File:line |
|--------|-----------------|----------------|-----------|
| **OtelLog** | `to_log_event()` → flat Value | **Lossy** — body→message, attributes→top-level fields, resource/scope nested | `vrl_target.rs:220-228` |
| **OtelSpan** | Direct proto → Value projection | **Lossless** — all proto fields, correct names | `vrl_target.rs:234-289` |
| **OtelMetric** | Restricted path model (.tags, .kind) | **Very lossy** — only name/desc/unit/resource/scope + legacy tags bridge | `vrl_target.rs:374-426` |

### OtelLog Issues (via `to_log_event()` bridge)

**Read:** `otel_log_event_to_value` calls `event.to_log_event()` then extracts the Value.
This flattens the OTLP LogRecord into a legacy LogEvent:
- `body` → `message` (top-level)
- `attributes` → top-level fields (mixed with body fields)
- `resource` → nested `resource` object
- `severity_number`, `severity_text` → lost or renamed
- `time_unix_nano`, `observed_time_unix_nano` → collapsed into `timestamp`
- `trace_id`, `span_id` → lost or buried in attributes

**Write-back:** `value_to_otel_log_event` calls `OtelLog::from_log_event(LogEvent)` —
another lossy conversion (message→body, top-level fields→attributes).

**Missing VRL paths:**
- `.body` (OTel field name — currently `.message`)
- `.severity_number` / `.severity_text`
- `.time_unix_nano` / `.observed_time_unix_nano`
- `.trace_id` / `.span_id` (log correlation)
- `.attributes."key"` (distinct from body and resource)
- `.dropped_attributes_count`

### OtelSpan — Already Correct

The OtelSpan VRL target (`otel_span_event_to_value`, line 234) is the **model to follow**.
It projects the full proto structure:
- `.trace_id`, `.span_id`, `.parent_span_id` (hex-encoded)
- `.name`, `.kind`, `.start_time_unix_nano`, `.end_time_unix_nano`
- `.attributes."key"` (flat map from KeyValue)
- `.events[]`, `.links[]` (with nested attributes)
- `.status.code`, `.status.message`
- `.resource.attributes."key"`, `.scope.name`, `.scope.version`

Write-back (`value_to_otel_span_event`, line 291) reconstructs the proto from Value.

### OtelMetric Issues (restricted path model)

**Read:** `precompute_otel_metric_value` exposes only:
- `.name`, `.description`, `.unit` — direct
- `.resource`, `.scope` — direct
- `.tags` — **legacy bridge** via `to_legacy_metric()` clone
- `.kind` — **legacy bridge** via `to_legacy_metric()` clone

**Write:** Handles `.name`, `.description`, `.unit`, `.resource`, `.scope`,
`.tags."key"` (via `set_data_point_attribute`). All other paths return error.

**Missing VRL paths:**
- `.data.type` (sum/gauge/histogram/etc.)
- `.data.sum.data_points[n].attributes."key"`
- `.data.sum.data_points[n].value`
- `.data.sum.aggregation_temporality`, `.data.sum.is_monotonic`
- `.data.gauge.data_points[n].value`
- `.data.histogram.data_points[n].count`, `.sum`, `.bucket_counts`, `.explicit_bounds`
- `.data.exponential_histogram.data_points[n].*`
- `.data.summary.data_points[n].*`

---

## Goal

All three signals use **direct proto → Value projections** with OTel field names.
No legacy bridges. Consistent developer experience across signals.

---

## Target VRL Paths

### OtelLog (OTLP LogRecord)

```
.body                                    → AnyValue (string, map, array, etc.)
.attributes."key"                        → AnyValue
.severity_number                         → Integer
.severity_text                           → String
.time_unix_nano                          → Integer
.observed_time_unix_nano                 → Integer
.trace_id                                → String (hex-encoded)
.span_id                                 → String (hex-encoded)
.flags                                   → Integer
.dropped_attributes_count                → Integer
.resource.attributes."key"               → AnyValue
.resource.dropped_attributes_count       → Integer
.scope.name                              → String
.scope.version                           → String
.scope.attributes."key"                  → AnyValue
```

### OtelSpan (already implemented — no changes needed)

```
.trace_id                                → String (hex)
.span_id                                 → String (hex)
.parent_span_id                          → String (hex)
.name                                    → String
.kind                                    → Integer
.start_time_unix_nano                    → Integer
.end_time_unix_nano                      → Integer
.attributes."key"                        → AnyValue
.status.code                             → Integer
.status.message                          → String
.events[n].name                          → String
.events[n].attributes."key"              → AnyValue
.links[n].trace_id                       → String (hex)
.resource.attributes."key"               → AnyValue
.scope.name                              → String
```

### OtelMetric (OTLP Metric)

```
.name                                    → String
.description                             → String
.unit                                    → String
.resource.attributes."key"               → AnyValue
.scope.name                              → String
.data.type                               → "sum" | "gauge" | "histogram" | "exponential_histogram" | "summary"

.data.sum.is_monotonic                   → Boolean
.data.sum.aggregation_temporality        → Integer (1=delta, 2=cumulative)
.data.sum.data_points[n].attributes."key"→ AnyValue
.data.sum.data_points[n].value           → Float or Integer
.data.sum.data_points[n].time_unix_nano  → Integer

.data.gauge.data_points[n].attributes."key" → AnyValue
.data.gauge.data_points[n].value         → Float or Integer

.data.histogram.aggregation_temporality  → Integer
.data.histogram.data_points[n].count     → Integer
.data.histogram.data_points[n].sum       → Float
.data.histogram.data_points[n].bucket_counts   → Array of Integer
.data.histogram.data_points[n].explicit_bounds → Array of Float
.data.histogram.data_points[n].attributes."key"→ AnyValue

.data.exponential_histogram.data_points[n].count     → Integer
.data.exponential_histogram.data_points[n].sum       → Float
.data.exponential_histogram.data_points[n].scale     → Integer
.data.exponential_histogram.data_points[n].zero_count→ Integer

.data.summary.data_points[n].count       → Integer
.data.summary.data_points[n].sum         → Float
.data.summary.data_points[n].quantile_values[m].quantile → Float
.data.summary.data_points[n].quantile_values[m].value    → Float
```

---

## Backward Compatibility

### Legacy aliases (deprecated, kept for one release cycle)

| Legacy path | New path | Signal |
|-------------|----------|--------|
| `.message` | `.body` | Log |
| `.timestamp` | `.time_unix_nano` | Log |
| `.tags."key"` | `.data.{variant}.data_points[0].attributes."key"` | Metric |
| `.kind` | `.data.{variant}.aggregation_temporality` | Metric |

### VRL migration tool updates

Add rules to `src/vrl_migrate/rules/`:
- `LOG-MSG-01`: `.message` → `.body`
- `LOG-TS-01`: `.timestamp` → `.time_unix_nano`
- `MET-TAG-01`: `.tags."key"` → `.data.sum.data_points[0].attributes."key"`
- `MET-KIND-01`: `.kind` → `.data.sum.aggregation_temporality`

---

## Phased Implementation

### Phase 1: OtelLog — direct proto projection (~250 lines)

Replace `otel_log_event_to_value` (currently `to_log_event()` bridge) with direct
LogRecord → Value projection, following the OtelSpan pattern.

**Read path:** Map LogRecord proto fields → VRL Value:
- `body` → `any_value_to_vrl()` (already exists)
- `attributes` → `otel_kvlist_to_object_map()`
- `severity_number`, `severity_text`, timestamps, trace/span IDs, flags

**Write-back path:** Replace `value_to_otel_log_event` (currently `from_log_event()`)
with direct Value → LogRecord reconstruction.

**Backward compat:**
- `.message` reads `.body` (deprecated alias)
- `.timestamp` reads `.time_unix_nano` (deprecated alias)

**Files:** `lib/vector-core/src/event/vrl_target.rs`
**Tests:** Read/write round-trip, backward compat aliases

### Phase 2: OtelMetric — full proto projection (~300 lines)

Replace `precompute_otel_metric_value` (restricted path model) with full
Metric → Value projection covering all 5 data variants.

**Read path:** Map Metric proto + data variant → VRL Value.
**Write path:** Match on `.data.{variant}.data_points[n].attributes."key"` etc.
**Backward compat:** `.tags` and `.kind` as deprecated aliases.

**Files:** `lib/vector-core/src/event/vrl_target.rs`
**Tests:** Read/write for each data variant, backward compat aliases

### Phase 3: OtelSpan — verification only (~0 lines code, ~50 lines tests)

OtelSpan already uses direct proto projection. Verify completeness:
- All proto fields accessible
- Write-back preserves all fields
- Add any missing fields (e.g., `.flags` if missing)

### Phase 4: Codec + sink compatibility (~100 lines)

Ensure all serializers (JSON, text, native, OTLP) handle the new Value
structure. The OTLP serializer should pass through without conversion.
Log-oriented serializers (JSON, text) may need updated projection logic.

### Phase 5: VRL migration tool + documentation (~100 lines)

- Add migration rules for legacy → OTel paths
- Update VRL docs with new signal-specific path reference
- Deprecation warnings when legacy paths are used

---

## Verification

- `cargo test -p vector --lib` — all tests pass
- Round-trip tests: VRL reads/writes all proto fields, proto structure preserved
- Backward compat: existing VRL programs using `.message`, `.tags` still work
- Demo validation: the o11y-weekly demo works with OTel-native VRL paths

## Example: After Migration

### Log

Current (legacy bridge):
```vrl
.message = "hello"
.timestamp = now()
```

After (OTel-native):
```vrl
.body = "hello"
.time_unix_nano = to_unix_timestamp(now(), unit: "nanoseconds")
.attributes."my.key" = "my.value"
.severity_text = "INFO"
.severity_number = 9
```

### Metric

Current (legacy bridge):
```vrl
.tags."service.name" = .resource.attributes."service.name"
```

After (OTel-native):
```vrl
.data.sum.data_points[0].attributes."service.name" = .resource.attributes."service.name"
```

### Span (already works)

```vrl
.attributes."http.method" = "GET"
.status.code = 2  # ERROR
```
