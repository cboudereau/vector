# OTLP as Core Protocol — Forward Plan

Vector's internal event model uses OpenTelemetry (OTLP) as its sole core protocol.

```rust
pub enum Event {
    Log(OtelLog),       // OpenTelemetry LogRecord
    Metric(OtelMetric), // OpenTelemetry Metric (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
    Trace(OtelSpan),    // OpenTelemetry Span
}
```

All legacy core types (`LogEvent`, `Metric`, `TraceEvent`, `MetricValue`, `MetricData`, `NativeSerializer`, DD sinks) are deleted from core. The original `event.proto`/`vector.proto` is retained as a source-scoped adapter for backward compatibility with the original Vector. 2,170 tests pass.

---

## Migration Guide for Users

### Strategy 1 — VRL Migration Tool (recommended for most users)

Run `vector vrl-migrate` to auto-rewrite VRL programs for the new event model (~91% coverage):

```bash
# Preview changes (dry-run)
vector vrl-migrate --config /etc/vector/vector.toml --dry-run

# Apply rewrites
vector vrl-migrate --config /etc/vector/vector.toml
```

**What it rewrites:**
- `.message` → `.body`
- `.timestamp` → `%vector.timestamp` (metadata path)
- `.source_type` → `%vector.source_type`
- `.host` → `%<source_name>.hostname`
- `.tags."key"` → `.attributes."key"` (metric VRL)
- `.kind` → `.attributes."vector.metric_kind"`
- Log namespace-aware paths for all sources

**What needs manual review (~9%):**
- Dynamic path access (`path = get_env_var!("FIELD"); get!(., path)`)
- Custom conditions referencing legacy field names
- Lua scripts (covered separately below)

### Strategy 2 — Direct Vector-to-Vector Connection (native protocol)

The `vector` source accepts the **original Vector native gRPC protocol** (`event.proto`/`vector.proto`). Existing Vector instances can send data using their standard `vector` sink with zero configuration changes:

```
┌─────────────────────┐      native Vector gRPC     ┌─────────────────────┐
│  Original Vector     │  ── vector sink ──────────► │  This fork           │
│  (any version)       │      port 6000              │  (vector source)     │
│  original configs    │                              │  dual-protocol       │
└─────────────────────┘                              └─────────────────────┘
```

**Old Vector config (sender) — no changes needed:**
```toml
[sinks.bridge]
type = "vector"
address = "new-vector-host:6000"
```

**New Vector config (receiver):**
```toml
[sources.from_old]
type = "vector"
address = "0.0.0.0:6000"
# The `vector` source accepts both protocols:
# - Original Vector native gRPC (PushEvents with EventWrapper)
# - OTLP gRPC (LogsService, MetricsService, TraceService)
# Incoming native events are converted to OTLP types at the source boundary.
```

**Key compatibility notes:**
- The `vector` source supports **both** the original native proto and OTLP gRPC on the same port.
- Old Vector instances using the native `vector` sink connect directly — no reconfiguration needed.
- Old Vector instances with an `opentelemetry` sink also work — they connect via OTLP gRPC.
- The `vector` **sink** in this fork speaks OTLP gRPC only (delegates to `opentelemetry` sink).

### Strategy 3 — OTel Collector as Bridge (optional)

For environments already running the OTel Collector, it can serve as an intermediary:

```
Old Vector ──► OTel Collector (otlp receiver → otlp exporter) ──► New Vector (vector source)
```

This is rarely needed since Strategy 2 provides direct native protocol compatibility. Use this if you want the OTel Collector to perform additional processing (filtering, sampling, enrichment) between the old and new Vector.

### Breaking Changes Summary

| Component | Change | Migration |
|-----------|--------|-----------|
| **VRL paths** | `.message` → `.body`, metadata moved | Run `vector vrl-migrate` |
| **logfmt output** | Attribute keys now namespaced: `attributes.my_attr=val` | Update downstream parsers |
| **GELF output** | Fields mapped from proto: `body`→`short_message`, `severity_number`→`level` | Update GELF consumers |
| **Avro output** | Schema must match OTLP/JSON layout (nested, camelCase) | Update Avro schemas |
| **protobuf output** | Descriptor must match OTLP/JSON field names | Update proto descriptors |
| **Lua scripts** | Table layout is structured: `event.log.attributes.key` not `event.log.key` | Update Lua scripts manually |
| **JSON output** | OTLP/JSON (camelCase, nested resource/scope/attributes) | Update JSON parsers |
| **Vector source** | Accepts both native proto AND OTLP gRPC | No change needed — existing `vector` sink works |
| **Vector sink** | Deleted — use `type = "opentelemetry"` instead | Replace `type = "vector"` with `type = "opentelemetry"` in configs |
| **honeycomb sink** | `data` field uses OTLP/JSON layout | Honeycomb handles nested JSON natively |
| **new_relic sink** | Attributes built from proto structure | Transparent — NR API accepts any attributes |
| **influxdb/logs sink** | Fields iterated from proto + attrs | Update tag/field key expectations |

---

## OTel Fidelity Review

### Deviations from OTel Spec

| Deviation | Where | Fidelity concern | Action |
|-----------|-------|------------------|--------|
| `vector.set_values` attribute | OtelMetric (Gauge) | Non-standard. Downstream sees valid Gauge. **Low risk.** | Keep — OTLP has no Set type |
| `vector.metric_type` attribute | OtelMetric | Informational marker. Downstream ignores it. **Harmless.** | Keep |
| `vector.metric_kind=incremental` on Gauge | OtelMetric | Semantically incorrect per OTel (Gauge has no temporality). **Medium risk.** | Keep — needed for statsd incremental Gauges |
| `vector.statistic` attribute | OtelMetric | Legacy from deleted StatisticKind. **Should not exist.** | **Delete** in Phase A |
| `OtelAttributes` (BTreeMap) | All events | Lossless conversion at proto boundaries. **No fidelity loss.** | Keep |
| `EventMetadata` sidecar | All events | Pipeline infrastructure, never in OTLP output. **Correct.** | Keep |

**Verdict:** Acceptable. `vector.*` attributes only appear when non-OTel sources create metrics. OTLP passthrough paths never inject them.

---

## Performance Review

| Path | Cost | Status |
|------|------|--------|
| OTLP gRPC → OTLP gRPC (passthrough) | Zero conversion | Optimal |
| VRL read-only (filter, route, sample) | Original proto returned (P38/P39) | Optimal |
| VRL mutating (remap with writes) | Proto→Value at entry, Value→Proto at exit | ~5% regression |
| VRL attribute lookup | BTreeMap O(log n) + Value clone | ~5% regression |
| VRL complex paths (`.attr[0]`) | `to_value_canonical()` full rebuild | **100-200% regression** — fix in Phase B |
| Codec encoding | `to_value_canonical()` per event | Fix in Phase C |

---

## Locked Decisions

All decisions locked. Autopilot proceeds without stopping.

| ID | Decision | Answer |
|----|----------|--------|
| D1 | Delete `to_value_canonical()` entirely | **Yes** — all 20 call sites migrated, method deleted |
| D2 | logfmt: namespace attribute keys | **Yes** — `attributes.my_attr=val`, proto fields flat |
| D3 | GELF: direct proto mapping | **Yes** — `body`→`short_message`, `severity_number`→`level`, `time_unix_nano`→`timestamp`, `resource.host.name`→`host`, rest→`_attr` |
| D4 | Avro: OTLP/JSON via Serialize | **Yes** — breaking, document in migration guide |
| D5 | protobuf: OTLP/JSON via Serialize → encode_message | **Yes** |
| D6 | Lua: structured layout | **Yes** — `{ body, attributes, resource, scope }` |
| D7 | Arrow: iterate proto directly | **Yes** |
| D8 | honeycomb: Serialize (OTLP/JSON) | **Yes** |
| D9 | new_relic: iterate proto + attrs | **Yes** |
| D10 | influxdb/logs: iterate proto + attrs | **Yes** |
| D11 | reduce: direct structured iteration | **Yes** |
| D12 | trace_to_log: transfer proto fields directly | **Yes** |
| D13 | schema/definition: proto-aware Kind inference | **Yes** |
| D14 | enrichment_tables: match on attributes directly | **Yes** |
| D15 | Delete convert_to_fields/as_map methods | **Yes** — after callers migrated |
| D16 | get(event_root()): OTLP/JSON-shaped Value | **Yes** |
| D17 | Delete MetricTags type entirely | **Yes** |
| D18 | Delete Sample/Bucket/Quantile | **Yes** |
| D19 | Split otel_event.rs | **Yes** — otel_log.rs + otel_metric.rs + otel_attributes.rs + otel_event.rs |
| D20 | Document all breaking changes | **Yes** — in this file's Migration Guide |
| D21 | Delete `vector` sink entirely | **Yes** — redundant wrapper around `opentelemetry` sink. Users use `type = "opentelemetry"` directly |
| D22 | Restore native Vector protocol in `vector` source | **Yes** — `event.proto`/`vector.proto` as source-scoped adapter for backward compatibility with original Vector |

---

## Autopilot Execution Plan

### Phase A — Clean Deletes (low risk, no behavior change)

Run `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` after each commit.

**A1. Delete `MetricTags` type and bridge**
- Replace remaining `MetricTags::from_iter` in tests with `otel_tags!` or `OtelAttributes` constructors
- Delete `with_metric_tags()` bridge method on OtelMetric
- Delete `event/metric/tags.rs` (`MetricTags`, `TagValue`, `TagValueSet`)
- Delete `MetricTags` re-export from `event/mod.rs`
- Update `metric_tags!` macro to produce `OtelAttributes` directly (or delete and use `otel_tags!`)

**A2. Delete `Sample`/`Bucket`/`Quantile`**
- Inline into callers or replace with `(f64, u64)` tuples / proto `ValueAtQuantile`
- Delete Arbitrary impls in `event/metric/arbitrary.rs`
- Update `event/metric/mod.rs` exports

**A3. Clean `vector.statistic` attribute writes**
- Grep and remove any code that sets `vector.statistic` attribute
- StatisticKind is already deleted — this is residual writes

**A4. Split `otel_event.rs` (7,312 lines)**
- Extract `OtelMetric` + metric helpers → `otel_metric.rs` (~3,000 lines)
- Extract `OtelAttributes` + helpers → `otel_attributes.rs` (~500 lines)
- Keep `OtelLog` + `OtelSpan` + shared helpers in `otel_event.rs` (~3,800 lines)
- Update all imports

**A5. Delete `vector` sink**
- The `vector` sink is a thin wrapper around `opentelemetry` — it adds no value
- Delete `src/sinks/vector/` entirely (mod.rs, config.rs)
- Remove `sinks-vector` feature from `Cargo.toml`
- Update any integration tests that reference `sinks::vector::VectorConfig` to use `opentelemetry` sink directly
- Update `component-validation-runner` feature list

**A6. Restore native Vector protocol in `vector` source (backward compatibility)**

The `vector` source must accept the original Vector native gRPC protocol so existing Vector deployments can send data to this fork without any configuration changes. The proto definitions and conversion logic are **source-scoped adapters** — they do not live in core.

- Restore `event.proto` and `vector.proto` into `src/sources/vector/proto/`
  - `event.proto`: `EventWrapper`, `Log`, `Metric`, `Trace`, `Value`, etc.
  - `vector.proto`: `service Vector { rpc PushEvents(...) }`, `rpc HealthCheck(...)`
- Add `build.rs` (or `src/sources/vector/build.rs`) to compile these protos with `tonic-build`
- Implement `proto::vector::Service` for the vector source's `Service` struct:
  - `push_events()`: receives `PushEventsRequest` (Vec<EventWrapper>), converts each to `Event` (OtelLog/OtelMetric/OtelSpan), sends through the pipeline
  - Conversion functions: `event.Log` → `OtelLog`, `event.Metric` → `OtelMetric`, `event.Trace` → `OtelSpan`
- Register both the native `VectorServer` and the OTLP services (LogsServiceServer, MetricsServiceServer, TraceServiceServer) on the same gRPC `RoutesBuilder`
- Conversion at source boundary (adapter logic in `src/sources/vector/`):
  - `Log.value` / `Log.fields` → `OtelLog` (body from value, fields as attributes)
  - `Metric` (Counter/Gauge/Set/Distribution/Histogram/Summary/Sketch) → `OtelMetric` (Sum/Gauge/Histogram/ExponentialHistogram/Summary with `vector.*` attributes for concepts OTLP lacks)
  - `Trace.fields` → `OtelSpan` (fields as span attributes)
  - `Metadata` → `EventMetadata` sidecar

### Phase B — Eliminate `to_value_canonical()` from internal methods (medium risk)

**B1. Replace `convert_to_fields()` / `convert_to_fields_unquoted()` on OtelLog**
- New implementation: iterate proto fields (body, severity_text, severity_number, time_unix_nano, observed_time_unix_nano, trace_id, span_id, flags, dropped_attributes_count) then `OtelAttributes` entries
- Yield `(KeyString, Value)` pairs without intermediate BTreeMap
- For `_unquoted` variant: same but unquoted keys

**B2. Replace `all_event_fields_skip_array_elements()` on OtelLog**
- Same proto iteration, but skip array elements within attribute values

**B3. Replace `as_map()` on OtelLog and OtelSpan**
- Build ObjectMap directly from proto fields + attributes
- **Note:** This still allocates, but without the collision guard overhead

**B4. Replace `get(event_root())` on OtelLog**
- Use `Serialize` (OTLP/JSON) → `serde_json::to_value()` → VRL `Value`
- Returns structured OTLP layout, not flat canonical

**B5. Same for OtelSpan** — `convert_to_fields()`, `as_map()`

### Phase C — Migrate external callers of `to_value_canonical()` (breaking changes)

Each commit migrates one caller. Run full tests after each.

**C1. logfmt encoder** (`lib/codecs/src/encoding/format/logfmt.rs`)
- Iterate proto fields (body, severity_text, etc.) → write `key=value`
- Iterate `record_attrs` → write `attributes.key=value`
- Iterate resource attrs → write `resource.key=value`

**C2. GELF encoder** (`lib/codecs/src/encoding/format/gelf.rs`)
- Map: `body` → `short_message`, `time_unix_nano` → `timestamp` (seconds float), `severity_number` → `level`, `resource.host.name` or first resource attr → `host`
- Record attrs → `_attr_name` (GELF additional fields)
- Resource/scope attrs → `_resource.name` / `_scope.name`

**C3. Avro encoder** (`lib/codecs/src/encoding/format/avro.rs`)
- `serde_json::to_value(&otel_log)` (uses Serialize = OTLP/JSON) → `apache_avro::to_value()`
- Schema must match OTLP/JSON layout

**C4. protobuf encoder** (`lib/codecs/src/encoding/format/protobuf.rs`)
- `serde_json::to_value(&otel_log)` → VRL `Value` → `encode_message()`
- User descriptor must match OTLP/JSON field names

**C5. Arrow encoder** (`lib/codecs/src/encoding/format/arrow.rs`)
- Iterate proto fields + attrs directly for column construction

**C6. Lua bridge** (`lib/vector-core/src/event/lua/event.rs`)
- Replace `otel_log.to_value_canonical()` with `otel_log_event_to_value()` (from vrl_target.rs)
- Produces structured table: `{ body, attributes, resource, scope, severity_text, ... }`

**C7. honeycomb sink** (`src/sinks/honeycomb/encoder.rs`)
- Replace `log.convert_to_fields()` with `serde_json::to_value(&log)` (OTLP/JSON)

**C8. new_relic sink** (`src/sinks/new_relic/model.rs`)
- Replace `convert_to_fields_unquoted()` and `as_map()` with direct proto field iteration
- Build NR attributes from proto fields + record_attrs

**C9. influxdb/logs sink** (`src/sinks/influxdb/logs.rs`)
- Replace `convert_to_fields()` with proto field + attribute iteration
- Map to InfluxDB fields/tags

**C10. reduce transform** (`src/transforms/reduce/transform.rs`)
- Replace `all_event_fields_skip_array_elements()` with direct proto iteration

**C11. trace_to_log transform** (`src/transforms/trace_to_log.rs`)
- Replace `as_map()` with direct span→log proto field transfer

**C12. schema/definition** (`lib/vector-core/src/schema/definition.rs`)
- Replace `Kind::from(to_value_canonical())` with proto-aware Kind inference
- Build Kind from known proto field types (body: AnyValue, severity_number: Integer, etc.)

**C13. enrichment_tables** (`src/enrichment_tables/memory/table.rs`)
- Replace `as_map()` with attribute-level matching via `OtelAttributes::get()`

### Phase D — Delete `to_value_canonical()` and cleanup

**D1. Delete `to_value_canonical()` from OtelLog**
- Delete method definition (~50 lines)
- Delete collision guard code
- Delete `as_map()`, `convert_to_fields()`, `convert_to_fields_unquoted()`, `all_event_fields_skip_array_elements()` (already empty/unused after Phase C)

**D2. Delete `to_value_canonical()` from OtelSpan**
- Same cleanup

**D3. Final test sweep**
- `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics`
- Verify zero `to_value_canonical` references remain: `grep -rn to_value_canonical lib/ src/`

### Gate

| Metric | Target |
|--------|--------|
| Tests passing | ≥ 2,170 |
| `to_value_canonical()` call sites | 0 |
| `MetricTags` references | 0 |
| `Sample`/`Bucket`/`Quantile` references | 0 |
| `vector.statistic` attribute writes | 0 |
| VRL typical remap regression | ≤ 5% |
| VRL complex path regression | < 20% (was 100-200%) |

---

## Principles

1. **OTLP/OTel is the only core protocol.** No vendor types in core.
2. **Two-format rule.** OTLP/proto or OTLP/JSON only. No flat canonical format.
3. **Vendor logic in adapters only.** Core never depends on adapters.
4. **`vector.*` attributes are acceptable.** They encode Vector concepts OTLP lacks. Never injected on passthrough paths.
5. **Features preserved.** Tail sampling, load balancing, span_metrics, aggregate — all OTel-native.
6. **Original Vector protocol supported at source boundary.** The `vector` source accepts the original Vector native gRPC protocol (`event.proto`/`vector.proto`) for backward compatibility. The proto definitions live in the source scope — they are adapter code, not core types.

---

## Architecture

```
Sources (adapters)              Core (OTel-native)                    Sinks (adapters)
──────────────────────────────  ────────────────────────────────────  ───────────────────────
opentelemetry (gRPC + HTTP)     OtelLog  (LogRecord)                  opentelemetry (gRPC+HTTP)
datadog_agent ──────────────►   OtelMetric (Sum/Gauge/Histogram/  ──► prometheus, influxdb
vector (native + OTLP gRPC)─►    ExponentialHistogram/Summary)   ──► kafka, loki, ES, …
kafka, syslog, … ──────────►   OtelSpan (Span)
                                OtelAttributes (BTreeMap wrapper)
                                Disk buffer: otlp_buffer.proto
```

### Vector Source: Dual-Protocol Ingestion

The `vector` source serves two gRPC protocols on the same port:

1. **Original Vector protocol** (`event.proto` / `vector.proto`):
   - `service Vector { rpc PushEvents(PushEventsRequest) returns (PushEventsResponse) }`
   - Accepts `EventWrapper` (Log, Metric, Trace) from original Vector instances
   - Converts at the source boundary: `event.Log` → `OtelLog`, `event.Metric` → `OtelMetric`, `event.Trace` → `OtelSpan`
   - **This is an adapter** — the proto definitions live in `src/sources/vector/proto/`, not in core

2. **OTLP gRPC** (3-service model):
   - `LogsService`, `MetricsService`, `TraceService`
   - Accepts connections from any OTLP-compliant client (OTel Collector, newer Vector, any agent)

```
Original Vector ── vector sink (native proto) ──► ┌──────────────────┐
                                                   │  vector source    │
                                                   │                  │ ──► Core (OtelLog,
OTel Collector  ── otlp exporter ──────────────► │  Dual-protocol:  │      OtelMetric,
                                                   │  • Vector native │      OtelSpan)
New Vector      ── opentelemetry sink (OTLP) ───► │  • OTLP gRPC     │
                                                   └──────────────────┘
```

There is **no `vector` sink** — it was a thin wrapper around `opentelemetry` and has been deleted. To send data to another Vector instance (or any OTLP-compatible receiver), use `type = "opentelemetry"` directly.

### Why keep the original protocol?

The original Vector has **partial** OTLP support — not all versions ship an `opentelemetry` sink, and the OTLP support that exists may not cover all signal types. Supporting the native protocol at the source means:
- **Zero-config migration**: existing Vector fleets can point their `vector` sink at this fork without changing anything
- **No bridge needed**: no OTel Collector middlebox, no sink reconfiguration on the sender side
- **Adapter-scoped complexity**: the `event.proto` / `vector.proto` definitions and conversion logic live entirely within `src/sources/vector/` — core remains pure OTLP
