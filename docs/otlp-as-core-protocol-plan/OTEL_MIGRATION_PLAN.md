# OTLP as Core Protocol — Forward Plan

Vector's internal event model uses OpenTelemetry (OTLP) as its sole core protocol.

```rust
pub enum Event {
    Log(OtelLog),       // OpenTelemetry LogRecord
    Metric(OtelMetric), // OpenTelemetry Metric (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
    Trace(OtelSpan),    // OpenTelemetry Span
}
```

All legacy types (`LogEvent`, `Metric`, `TraceEvent`, `MetricValue`, `MetricData`, `NativeSerializer`, DD sinks, `event.proto`) are deleted. 2,170 tests pass.

---

## OTel Fidelity Review

### Deviations from OTel Spec

These are Vector-specific extensions embedded in OTel attributes. They work but reduce interoperability — an external OTel collector receiving these events sees attributes it doesn't understand.

| Deviation | Where | Why it exists | Fidelity concern |
|-----------|-------|---------------|------------------|
| `vector.set_values` attribute | OtelMetric (Gauge with set semantics) | OTLP has no Set metric type. Encoded as Gauge + attribute holding string array. | Non-standard. Any OTel-native consumer ignores this and sees a Gauge with cardinality as value. **Low risk** — Set is rare in OTLP ecosystems. |
| `vector.metric_type=distribution` | OtelMetric (Histogram) | Vector distinguishes Distribution from Histogram internally (statsd `d` vs `h`). Encoded as Histogram + marker attribute. | Non-standard but **harmless** — downstream sees a valid Histogram. The attribute is informational. |
| `vector.metric_type=set` | OtelMetric (Gauge) | Marks a Gauge as actually being a Set. | Same as above — downstream sees valid Gauge. |
| `vector.metric_kind=incremental` | OtelMetric (Gauge) | OTLP Gauge has no temporality. Vector tracks incremental Gauges. | Non-standard. OTel Gauge is always "last value." **Medium risk** — semantically incorrect for strict OTel consumers. |
| `vector.statistic` attribute | OtelMetric (Histogram) | Distinguishes `histogram` vs `summary` statistic type (legacy from statsd). | Already eliminated (StatisticKind deleted). Attribute may still be written — **clean up**. |
| `MetricKind` on Gauge | OtelMetric | OTLP Gauge has no temporality concept. Vector's `Incremental` Gauge makes no OTel sense. | Design choice — kept for backward compat with sources that emit incremental Gauges (statsd). |
| `OtelAttributes` (BTreeMap) | All events | OTLP spec uses `repeated KeyValue`. We use BTreeMap for O(log n) lookup, converting at proto boundaries. | **No fidelity loss** — conversion is lossless. But adds O(n log n) at ingestion and O(n) at egress. |
| `EventMetadata` sidecar | All events | Carries pipeline metadata (finalizers, source_id, schema_definition) outside the proto. | **Correct** — this is pipeline infrastructure, not telemetry data. Should never leak into OTLP output. |

### Verdict

The `vector.*` attributes are pragmatic — they encode Vector concepts that OTLP doesn't have natively. **For OTLP passthrough (OTel source → OTel sink), these are never injected.** They only appear when non-OTel sources (statsd, internal_metrics) create metrics that don't map cleanly to OTLP.

**Acceptable.** The main risk is `vector.metric_kind=incremental` on Gauges — this is semantically wrong per OTel spec but needed for Vector's incremental aggregation. Document it; don't try to eliminate it.

---

## Performance Review

### Current State

| Path | Cost | Status |
|------|------|--------|
| **OTLP gRPC → OTLP gRPC** (passthrough, no VRL) | Zero conversion | Optimal |
| **OTLP gRPC → OTLP HTTP** | Proto → JSON at sink boundary | Optimal |
| **VRL read-only** (filter, route, sample) | Proto → Value at entry, **original proto returned** at exit (P38/P39 passthrough) | Optimal |
| **VRL mutating** (remap with writes) | Proto → Value at entry, Value → Proto at exit | ~5% regression vs legacy for typical workloads |
| **VRL attribute lookup** | BTreeMap O(log n) + Value clone per access | ~5% regression (was ~15-25% before OtelAttributes) |
| **VRL complex paths** (`.attr[0]`, nested) | Fallback to `to_value_canonical()` full-event rebuild | **100-200% regression** — rare but bad when hit |
| **Codec encoding** (logfmt, GELF, Avro, protobuf) | `to_value_canonical()` per event | ~1 extra allocation per event — acceptable for output path |
| **Disk buffer** | OTLP proto binary | Optimal |

### Remaining Performance Problems (priority order)

**1. `to_value_canonical()` in OtelLog/OtelSpan internal fallbacks (HIGH)**
- 4 call sites in `otel_event.rs` (lines 2506, 2532, 2627, 2637 for OtelLog; line 3739 for OtelSpan)
- Triggered by `convert_to_fields()` and `all_event_fields_skip_array_elements()`
- Full BTreeMap allocation + clone of all proto fields + all attributes per call
- **Fix:** Direct proto field iteration — yield `(key, Value)` pairs without intermediate tree

**2. `to_value_canonical()` on event root get (MEDIUM)**
- Line 1644: `OtelLog::get(event_root())` builds full canonical Value
- Called by VRL decoders, assertions, any code accessing `.`
- **Fix:** Only build when actually needed (most callers want specific fields)

**3. Per-access `Value` clone overhead (LOW — architectural)**
- Every `OtelLog::get()` / `OtelSpan::get()` returns `Option<Value>` (owned clone)
- Legacy `LogEvent` returned `Option<&Value>` (borrowed, zero-alloc)
- ~5% regression for typical VRL workloads
- **Fix requires:** VRL operating on `AnyValue` directly — massive effort, minimal gain

### Performance Targets for Autopilot

| Metric | Current | Target | How to verify |
|--------|---------|--------|---------------|
| VRL typical remap regression | ~5% | ~5% (hold) | Benchmark: `benches/remap.rs` |
| VRL complex path regression | ~100-200% | <20% | Benchmark: remap with `.attr[0]` paths |
| `to_value_canonical()` call sites | 20 (production) | 5 (codec encoders + Lua only) | `grep -rn to_value_canonical` |
| `MetricTags` references | 159 | 0 | `grep -rn MetricTags` |

---

## Remaining Work

### Tier 1 — Code Cleanliness (no behavior change)

| Task | Scope | Risk | Autopilot? |
|------|-------|------|------------|
| **Delete `MetricTags` type** | 42 files. All production code already uses `OtelAttributes`. `MetricTags` only remains in test helpers, `with_metric_tags()` bridge, and metric identity. | Low — tests-only | Yes |
| **Delete `Sample`/`Bucket`/`Quantile` types** | 9 files. Already thin wrappers matching proto semantics exactly. `MetricView` borrows proto slices directly. | Low — types unused in production hot paths | Yes |
| **Split `otel_event.rs`** (7,312 lines) | Extract `OtelMetric` (~3,000 lines) into `otel_metric.rs`. Keep `OtelLog` + `OtelSpan` + shared helpers in `otel_event.rs`. | Low — pure file reorganization | Yes |
| **Clean `vector.statistic` attribute writes** | Grep and remove any remaining `vector.statistic` attribute injection (StatisticKind already deleted). | Low | Yes |

### Tier 2 — Performance (behavior-preserving refactors)

| Task | Scope | Risk | Autopilot? |
|------|-------|------|------------|
| **Eliminate `to_value_canonical()` from OtelLog/OtelSpan internal methods** | 6 call sites in `otel_event.rs`. Replace `convert_to_fields()` and `all_event_fields_skip_array_elements()` with direct proto iteration. | Medium — iterator semantics must match exactly | Yes, with tests |
| **Event root `get()` optimization** | 1 call site. Avoid full canonical rebuild when caller just needs a field count or emptiness check. | Low | Yes |

### Tier 3 — Codec Encoders (breaking change for flat-format consumers)

| Task | Scope | Risk | Autopilot? |
|------|-------|------|------------|
| **logfmt encoder → direct proto** | 1 file. Iterate proto fields + attributes, write `key=value`. | Medium — output format changes (attribute keys become `attributes.key` or stay flat?) | **Needs decision** |
| **GELF encoder → direct proto** | 1 file. Extract GELF-specific fields from proto/attributes. | Medium — GELF spec mapping needs review | **Needs decision** |
| **Avro encoder → OTLP/JSON** | 1 file. Use `Serialize` → `serde_json::to_value()` → Avro. | Medium — user Avro schemas must match OTLP/JSON layout | **Needs decision** |
| **protobuf encoder → direct proto** | 1 file. Use `prost::Message::encode()` directly. | Medium | **Needs decision** |
| **Lua bridge → structured Value** | 1 file. Use `otel_log_event_to_value()` instead of `to_value_canonical()`. | Medium — Lua scripts see different field layout | **Needs decision** |

### Tier 4 — Future (not planned for autopilot)

| Task | Why deferred |
|------|-------------|
| VRL native `AnyValue` support | Massive effort (~100+ stdlib functions), ~3-5% gain |
| Lazy VRL conversion (P38 enhancement) | Complex, diminishing returns after read-only passthrough |
| Delete `to_value_canonical()` entirely | Blocked by Tier 3 decisions |

---

## Autopilot Execution Plan

### Phase A — Clean Delete (Tier 1)

Safe, low-risk deletions. Run all tests after each commit.

1. **Delete `MetricTags` type and bridge.** Replace remaining `MetricTags::from_iter` in tests with `otel_tags!` or `OtelAttributes::from_iter`. Delete `with_metric_tags()` method (callers use `OtelAttributes` constructors directly). Delete `event/metric/tags.rs`. Delete `MetricTags` re-export from `event/mod.rs`.

2. **Delete `Sample`/`Bucket`/`Quantile`.** Inline into callers or replace with `(f64, u64)` tuples. Delete Arbitrary impls. Update `event/metric/mod.rs` exports.

3. **Clean `vector.statistic` writes.** Remove any remaining code that sets this attribute.

4. **Split `otel_event.rs`.** Extract `OtelMetric` + metric helpers into `otel_metric.rs`. Move `OtelAttributes` into `otel_attributes.rs`. Keep `OtelLog` + `OtelSpan` + shared helpers in `otel_event.rs`.

### Phase B — Performance (Tier 2)

Behavior-preserving refactors to eliminate hot-path `to_value_canonical()` calls.

5. **Replace `convert_to_fields()` with direct proto iteration** for OtelLog. Yield `(KeyString, Value)` by walking proto fields (body, severity, timestamps, trace_id, span_id, flags) then `OtelAttributes` entries. No intermediate BTreeMap.

6. **Same for OtelSpan** — `convert_to_fields()` and `all_event_fields_skip_array_elements()`.

7. **Optimize `get(event_root())`** — return `to_value_canonical()` only when actually consumed (not just for emptiness/field-count checks).

### Phase C — Review Checkpoints (Tier 3)

Stop and ask before each. These change output formats.

8. **Decision: logfmt attribute key format** — `body=..., severity_text=..., my_attr=...` (current flat) vs `body=..., severity_text=..., attributes.my_attr=...` (namespaced)?

9. **Decision: GELF field mapping** — which OTel fields map to GELF `host`, `short_message`, `timestamp`, `level`? Current mapping via flat canonical works — is it correct?

10. **Decision: Avro schema compatibility** — accept OTLP/JSON layout as the Avro schema contract? Breaking change for existing users.

### Gate: All tests pass, benchmark regression ≤ 5% on typical remap, `to_value_canonical()` call sites ≤ 10.

---

## Principles (preserved)

1. **OTLP/OTel is the only core protocol.** No vendor types in core.
2. **Two-format rule.** OTLP/proto or OTLP/JSON only. `to_value_canonical()` flat format is transitional.
3. **Vendor logic in adapters only.** Core never depends on adapters.
4. **The compiler enforces the boundary.** `cargo build -p vector-core` clean = correct.
5. **Features preserved.** Tail sampling, load balancing, span_metrics, aggregate — all OTel-native.
6. **`vector.*` attributes are acceptable.** They encode Vector concepts OTLP lacks. Documented, not leaked on passthrough paths.

---

## Architecture

```
Sources (adapters)              Core (OTel-native)                    Sinks (adapters)
──────────────────────────────  ────────────────────────────────────  ───────────────────────
opentelemetry (gRPC + HTTP)     OtelLog  (LogRecord)                  opentelemetry (gRPC+HTTP)
datadog_agent ──────────────►   OtelMetric (Sum/Gauge/Histogram/  ──► prometheus, influxdb
  DD proto → OTel at boundary     ExponentialHistogram/Summary)   ──► kafka, loki, ES, …
vector (OTLP gRPC) ────────►   OtelSpan (Span)
kafka, syslog, … ──────────►   Resource + InstrumentationScope
                                OtelAttributes (BTreeMap wrapper)
                                Disk buffer: otlp_buffer.proto
```

## Key Metrics

| Metric | Value |
|--------|-------|
| Tests passing | 2,170 (1,782 vector + 197 vector-core + 180 codecs + 11 vrl-metrics) |
| `otel_event.rs` | 7,312 lines (target: split to ~3,500 + ~2,500 + ~1,300) |
| `to_value_canonical()` call sites | 20 production (target: 5) |
| `MetricTags` references | 159 (target: 0) |
| Legacy types remaining | `MetricTags`, `Sample`, `Bucket`, `Quantile` (target: 0) |
| VRL typical remap regression | ~5% vs legacy (acceptable) |
| VRL complex path regression | ~100-200% (target: <20% after Phase B) |
