# Plan: OTel-native VRL Targets for All Signals

## Problem

The VRL targets for OTel events have inconsistent proto fidelity:

| Signal | Current approach | Proto fidelity | File:line |
|--------|-----------------|----------------|-----------|
| **OtelLog** | `to_log_event()` → flat Value | **Lossy** — body→message, attributes→top-level fields | `vrl_target.rs:220-228` |
| **OtelSpan** | Direct proto → Value projection | **Lossless** — all proto fields, correct names | `vrl_target.rs:234-289` |
| **OtelMetric** | Restricted path model (.tags, .kind) | **Very lossy** — only name/desc/unit/resource/scope + legacy tags bridge | `vrl_target.rs:374-426` |

## Strategy: Direct replacement with backward-compatible aliases (no toggle)

~~The toggle-based approach (`otel_paths = true/false`) was considered but rejected~~
because it would need to be threaded through every VRL execution context (remap,
filter, route, sample, conditions, codec VRL deserializers) — fragile and incomplete.

Instead: **replace the legacy projections directly** with OTel-native ones that
maintain backward compatibility through aliases:

- **OtelLog**: `.message` alias for `.body`, arbitrary fields become attributes
- **OtelMetric**: `.tags` alias for first data point attributes, `.kind` for temporality
- **OtelSpan**: already OTel-native (no change needed)

**Validated: zero test breakage** across the full suite (1783 pass, 0 fail).

Users run `vector vrl-migrate` to adopt new OTel-native paths in their programs.

---

## Target VRL Paths (when `otel_paths = true`)

### OtelLog (OTLP LogRecord)

```
.body                                    → AnyValue (the log message)
.attributes."key"                        → AnyValue (record-level attributes)
.severity_number                         → Integer (0-24)
.severity_text                           → String ("INFO", "ERROR", etc.)
.time_unix_nano                          → Integer
.observed_time_unix_nano                 → Integer
.trace_id                                → String (hex-encoded, for log-trace correlation)
.span_id                                 → String (hex-encoded)
.flags                                   → Integer
.dropped_attributes_count                → Integer
.resource.attributes."key"               → AnyValue
.scope.name                              → String
.scope.version                           → String
```

### OtelSpan (already OTel-native — no change needed)

```
.trace_id, .span_id, .parent_span_id    → String (hex)
.name, .kind                             → String, Integer
.start_time_unix_nano, .end_time_unix_nano → Integer
.attributes."key"                        → AnyValue
.status.code, .status.message            → Integer, String
.events[], .links[]                      → Arrays with nested attributes
.resource.attributes."key"               → AnyValue
.scope.name, .scope.version              → String
```

### OtelMetric (OTLP Metric)

```
.name, .description, .unit               → String
.resource.attributes."key"               → AnyValue
.scope.name, .scope.version              → String
.data.type                               → "sum" | "gauge" | "histogram" | "exponential_histogram" | "summary"
.data.sum.data_points[n].attributes."key"→ AnyValue
.data.sum.data_points[n].value           → Float or Integer
.data.sum.is_monotonic                   → Boolean
.data.sum.aggregation_temporality        → Integer (1=delta, 2=cumulative)
.data.gauge.data_points[n].value         → Float or Integer
.data.histogram.data_points[n].count     → Integer
.data.histogram.data_points[n].sum       → Float
.data.histogram.data_points[n].bucket_counts → Array of Integer
.data.histogram.data_points[n].explicit_bounds → Array of Float
```

---

## Implementation

### Phase 1: Add `otel_paths` toggle + OTel-native projections (~400 lines)

**Step 1a: Thread the toggle through VrlTarget**

**Step 1a: OtelLog — COMPLETE**

Replaced `to_log_event()` bridge with direct LogRecord proto projection:
- `.body` AND `.message` (alias) expose LogRecord.body
- LogRecord attributes flattened to top-level (`.key` works directly)
- Proto fields: `.severity_text`, `.severity_number`, `.time_unix_nano`, etc.
- Write-back: `.body`/`.message`→body, known proto keys extracted, remaining→attributes
- Zero test breakage (1783 pass, 0 fail)

**Step 1b: OtelMetric — COMPLETE**

Replaced restricted `.tags`/`.kind` bridge with full proto projection:
- `.tags` (alias) reads first data point attributes
- `.kind` (alias) reads aggregation temporality
- `.data` exposes full proto: `.data.type`, `.data.sum.data_points[n].value`, etc.
- All 5 data variants: Sum, Gauge, Histogram, ExponentialHistogram, Summary
- Zero test breakage (1783 pass, 0 fail)

**Step 1c: OtelSpan — already OTel-native (no changes needed)**

### Phase 2: VRL migration tool rules (future)

Add rules to `src/vrl_migrate/`:
- `LOG-BODY-01`: `.message` → `.body` (optional — `.message` still works)
- `MET-TAG-01`: `.tags."key"` → `.data.sum.data_points[0].attributes."key"` (optional)

---

## Key Design Decisions

| Decision | Resolution |
|----------|-----------|
| No toggle | Direct replacement — aliases provide backward compat without a flag |
| OtelLog | `.message` alias for `.body`, arbitrary fields→attributes |
| OtelMetric | `.tags` alias for first data point attrs, `.data` for full proto |
| OtelSpan | Already OTel-native — no change needed |
| Write-back | Known proto keys extracted, remaining keys→LogRecord attributes |

## Verification

- `cargo test -p vector --lib` — **zero test breakage** (toggle defaults to false)
- New tests exercise `otel_paths = true` explicitly
- Demo validation: o11y-weekly works with `otel_paths = true` on the metrics remap
