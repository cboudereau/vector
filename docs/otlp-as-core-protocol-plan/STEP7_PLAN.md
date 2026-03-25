# Step 7 — Re-Integration: DataDog Source as Clean OTel Adapter — COMPLETE

## Context

Steps 0–6 of the OTLP migration are complete. The `Event` enum has 3 OTel-native variants:
`Log(OtelLog)`, `Metric(OtelMetric)`, `Trace(OtelSpan)`. All tests pass.

Step 7 cleaned up the DataDog source to emit OtelMetric/OtelSpan directly (no legacy
intermediates). DD-specific metadata is stored in OTLP resource/record attributes.

No DD sink is needed — DataDog accepts OTLP natively. Users point the existing OTel
gRPC/HTTP sink at DD's OTLP endpoint.

## Decisions

| Decision | Resolution |
|----------|-----------|
| DD sink | Not needed — DD accepts OTLP natively. Users use the OTel sink with DD endpoint. |
| DD source | Clean up metrics + traces to emit OtelMetric/OtelSpan directly. DD-specific data stored in OTLP resource/record attributes. |
| Vector sink | Keep as-is — backward compat with forks and older Vector versions speaking native proto. |
| Vector source | No changes needed — already receives legacy proto and converts to OTel events. |

---

## Phase 1: DD source metrics — emit OtelMetric directly

**Goal**: Remove legacy `Metric` intermediate from DD source metrics path. DD-specific metadata goes into OTLP attributes.

**Current flow** (`src/sources/datadog_agent/metrics.rs`):
```
DD proto → Metric::new() (legacy Counter/Gauge/AggregatedHistogram) → .into() → Event::Metric(OtelMetric via From<Metric>)
```

**Target flow**:
```
DD proto → OtelMetric::new() directly (Sum/Gauge/ExponentialHistogram) → Event::Metric(OtelMetric)
```

**Files to modify**:
- `src/sources/datadog_agent/metrics.rs` — rewrite `decode_ddseries_v1()`, `decode_ddseries_v2()`, `into_vector_metric()` to construct `OtelMetric` directly
  - DD Counter → OTel Sum (is_monotonic=true, delta temporality)
  - DD Gauge → OTel Gauge
  - DD Rate → OTel Sum + `interval_ms` in attributes
  - DD tags → OTel metric attributes
  - DD host/namespace/timestamp → OTel resource attributes + time_unix_nano
- `src/sources/datadog_agent/ddsketch.rs` — add `to_exponential_histogram_data_point()` method
  - DDSketch bins → OTel ExponentialHistogram buckets (scale 6)
  - `count`, `sum` exact; `min`/`max` as `dd.min`/`dd.max` attributes
  - `zero_count` from k==0 bins
- `src/sources/datadog_agent/metrics.rs` — update `decode_ddsketch()` to use the new method

**DD metadata in OTLP attributes**:
- `datadog.origin.product` / `.category` / `.service` → resource attributes
- `dd.api_key` → EventMetadata secrets (unchanged)
- `interval_ms` → metric attribute
- `dd.min` / `dd.max` → resource attributes (from DDSketch)

**Validation**:
- Existing DD source metrics tests pass
- `cargo test -p vector --lib` — all tests pass
- `cargo build -p vector-core` clean (no DD types in core)

---

## Phase 2: DD source traces — emit OtelSpan directly

**Goal**: Remove `TraceEvent` intermediate. Build `OtelSpan` directly from DD trace proto.

**Current flow** (`src/sources/datadog_agent/traces.rs`):
```
DD TracePayload → TraceEvent (LogEvent newtype) → OtelSpan::from_trace_event(te) → Event::Trace(OtelSpan)
```

**Target flow**:
```
DD TracePayload → OtelSpan::new() directly → Event::Trace(OtelSpan)
```

**Files to modify**:
- `src/sources/datadog_agent/traces.rs` — rewrite `handle_dd_trace_payload_v0()` and `handle_dd_trace_payload_v1()`
  - `dd_span.trace_id` (u64) → `span.trace_id` (16 bytes, zero-extended)
  - `dd_span.span_id` (u64) → `span.span_id` (8 bytes)
  - `dd_span.parent_id` → `span.parent_span_id`
  - `dd_span.name` → `span.name`
  - `dd_span.start` (ns) → `span.start_time_unix_nano`
  - `dd_span.start + dd_span.duration` → `span.end_time_unix_nano`
  - `dd_span.error` → `span.status` (error=1 → StatusCode::Error)
  - `dd_span.service` → `service.name` resource attribute
  - `dd_span.resource` → `dd.resource` attribute
  - `dd_span.type` → `dd.span_type` attribute
  - `dd_span.meta` → span attributes
  - `dd_span.metrics` → span attributes (as doubles)
  - DD env/hostname/priority → resource attributes

**Validation**:
- Existing DD source trace tests pass
- `cargo test -p vector --lib` — all tests pass
- Span scope carries DD service/language info

---

## Phase 3: Round-trip tests + documentation update

**Goal**: Integration tests and mark Step 7 complete.

**Tests**:
- DD source → OtelMetric → OTel gRPC sink round-trip (all metric types)
- DD source → OtelSpan → OTel gRPC sink round-trip (scope assertion)
- Vector source (legacy proto) → OtelLog → Vector sink round-trip

**Documentation**:
- Update `CONSOLIDATED_MIGRATION_PLAN.md` — mark Step 7 COMPLETE

**Validation gate** (from migration plan):
- `cargo build -p vector-core` clean — no proprietary types in core
- Round-trip test for all 3 signals including span scope assertion
- `cargo test -p vector --lib` — all tests pass
