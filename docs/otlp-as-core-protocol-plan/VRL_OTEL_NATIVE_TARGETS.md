# Plan: OTel-native VRL Targets for All Signals

## Problem

The VRL targets for OTel events have inconsistent proto fidelity:

| Signal | Current approach | Proto fidelity | File:line |
|--------|-----------------|----------------|-----------|
| **OtelLog** | `to_log_event()` → flat Value | **Lossy** — body→message, attributes→top-level fields | `vrl_target.rs:220-228` |
| **OtelSpan** | Direct proto → Value projection | **Lossless** — all proto fields, correct names | `vrl_target.rs:234-289` |
| **OtelMetric** | Restricted path model (.tags, .kind) | **Very lossy** — only name/desc/unit/resource/scope + legacy tags bridge | `vrl_target.rs:374-426` |

## Strategy: Toggle-based migration (same pattern as buffer format + use_otlp_decoding)

Instead of replacing the VRL Value layout (which breaks all existing VRL programs and tests),
introduce a **per-transform toggle** that switches between legacy and OTel-native VRL paths.

### Precedents in this codebase

| Feature | Toggle | Default | Migration path |
|---------|--------|---------|----------------|
| Disk buffer format | `buffer_format = "vector"/"otlp"/"migrate"` | `vector` | Vector → Migrate → Otlp |
| OTel source decoding | `use_otlp_decoding = true` | `false` | false → true → flag removed |
| Log field layout | `LogNamespace::Legacy` / `::Vector` | `Legacy` | Legacy → Vector |

### New toggle: `otel_paths`

```toml
[transforms.my_remap]
type = "remap"
inputs = ["otel_source"]
otel_paths = true        # ← new toggle, default false
source = '''
  # With otel_paths = true, VRL sees OTel-native field names:
  .body = "hello"
  .severity_text = "ERROR"
  .attributes."http.method" = "GET"
  .time_unix_nano = 1234567890000000000
'''
```

```toml
[transforms.legacy_remap]
type = "remap"
inputs = ["otel_source"]
# otel_paths defaults to false — legacy VRL paths:
source = '''
  .message = "hello"
  .severity = "ERROR"
'''
```

### Migration path

1. **Phase 1** (this plan): Add `otel_paths` toggle. Default `false`. Legacy behavior unchanged.
2. **Phase 2**: Announce deprecation of legacy paths. `vector vrl-migrate` rewrites programs.
3. **Phase 3**: Change default to `true`. Legacy programs get deprecation warnings.
4. **Phase 4**: Remove legacy path support. Remove toggle.

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

- Add `otel_paths: bool` field to `VrlTarget::OtelLog`, `VrlTarget::OtelMetric`
- Pass it from `TransformConfig` context through to `VrlTarget::new()`
- Add `otel_paths` field to `RemapConfig` (the main VRL transform config)
- Default: `false`

**File changes:**
- `src/transforms/remap.rs` — add `otel_paths` to config, pass to VRL runtime
- `lib/vector-core/src/event/vrl_target.rs` — branch on `otel_paths` in projection functions

**Step 1b: OTel-native OtelLog projection (when otel_paths = true)**

- `otel_log_event_to_value`: direct LogRecord → Value (as designed above)
- `value_to_otel_log_event`: direct Value → LogRecord reconstruction
- When `otel_paths = false`: unchanged (legacy `to_log_event()` bridge)

**Step 1c: OTel-native OtelMetric projection (when otel_paths = true)**

- `precompute_otel_metric_value`: full Metric → Value with all 5 data variants
- Write path: match full proto paths for inserts
- When `otel_paths = false`: unchanged (legacy `.tags`/`.kind` bridge)

**Step 1d: Tests**

- New tests: VRL read/write with `otel_paths = true` for all 3 signals
- Existing tests: unchanged (all use default `otel_paths = false`)
- No test breakage by design — the toggle is opt-in

### Phase 2: VRL migration tool rules (~50 lines)

Add rules to `src/vrl_migrate/`:
- `LOG-BODY-01`: `.message` → `.body`
- `LOG-TS-01`: `.timestamp` → `.time_unix_nano`
- `MET-TAG-01`: `.tags."key"` → `.data.sum.data_points[0].attributes."key"`
- `MET-KIND-01`: `.kind` → `.data.sum.aggregation_temporality`

### Phase 3: Change default + deprecation warnings (~30 lines)

- Change `otel_paths` default to `true`
- Log deprecation warning when legacy paths are used

### Phase 4: Remove toggle (~200 lines removed)

- Remove `otel_paths` field
- Remove legacy projection functions
- Remove legacy VRL migration rules (no longer needed)

---

## Key Design Decisions

| Decision | Resolution |
|----------|-----------|
| Toggle scope | Per-transform (`otel_paths` on RemapConfig), not global |
| OtelSpan | No toggle needed — already OTel-native |
| Where toggle lives | `RemapConfig` (remap), `FilterConfig` (filter), `RouteConfig` (route) — any VRL-using transform |
| Default | `false` (legacy) — zero breakage |
| VrlTarget changes | Branch on `otel_paths` in `new()`, `into_events()`, `get()`, `set()` |

## Verification

- `cargo test -p vector --lib` — **zero test breakage** (toggle defaults to false)
- New tests exercise `otel_paths = true` explicitly
- Demo validation: o11y-weekly works with `otel_paths = true` on the metrics remap
