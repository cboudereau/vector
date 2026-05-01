# OTLP as Core Protocol — Forward Plan

**Sol** (**S**ingle **O**bservability **L**ayer) is a true fork of [Vector](https://github.com/vectordotdev/vector), rebuilt around an OpenTelemetry-centric core. See [MARKET.md](MARKET.md) for the full product vision and market positioning.

Sol's internal event model uses OpenTelemetry (OTLP) as its sole core protocol.

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

### Strategy 2 — Direct Vector-to-Sol Connection (native protocol)

The `vector` source speaks the **original Vector native gRPC protocol** (`event.proto`/`vector.proto`). Existing Vector instances can send data using their standard `vector` sink with zero configuration changes:

```
┌─────────────────────┐      native Vector gRPC     ┌─────────────────────┐
│  Original Vector     │  ── vector sink ──────────► │  Sol                 │
│  (any version)       │      port 6000              │  (vector source)     │
│  original configs    │                              │  native proto only   │
└─────────────────────┘                              └─────────────────────┘
```

**Old Vector config (sender) — no changes needed:**
```toml
[sinks.bridge]
type = "vector"
address = "sol-host:6000"
```

**Sol config (receiver):**
```toml
[sources.from_old_vector]
type = "vector"
address = "0.0.0.0:6000"
# Speaks the original Vector native gRPC protocol.
# Incoming events are converted to OTLP types at the source boundary.
```

For OTLP clients (OTel Collector, other Sol instances, any OTLP agent), use the `opentelemetry` source instead:

```toml
[sources.from_otlp]
type = "opentelemetry"
grpc.address = "0.0.0.0:4317"
http.address = "0.0.0.0:4318"
```

**Key compatibility notes:**
- The `vector` source speaks **only** the original native proto — it is a backward compatibility adapter.
- The `opentelemetry` source handles OTLP gRPC + HTTP — it is the standard ingestion path.
- Old Vector instances with an `opentelemetry` sink can connect to the `opentelemetry` source.
- There is no `vector` sink in Sol — use `type = "opentelemetry"` to send data out.

### Strategy 3 — OTel Collector as Bridge (optional)

For environments already running the OTel Collector, it can serve as an intermediary:

```
Old Vector ──► OTel Collector (otlp receiver → otlp exporter) ──► Sol (opentelemetry source)
```

This is rarely needed since Strategy 2 provides direct native protocol compatibility. Use this if you want the OTel Collector to perform additional processing (filtering, sampling, enrichment) between the original Vector and Sol.

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
| **Vector source** | Native proto only (backward compat adapter) | No change needed — existing `vector` sink works |
| **Vector sink** | Deleted — use `type = "opentelemetry"` instead | Replace `type = "vector"` with `type = "opentelemetry"` in configs |
| **Transformer `only_fields`/`except_fields`** | Paths are OTLP-aware: `body`, `attributes.X`, `resource.X` | Update transformer configs |
| **honeycomb sink** | `data` field uses OTLP/JSON layout (was flat key-value) | Honeycomb handles nested JSON natively |
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
| D18 | ~~Delete Sample/Bucket/Quantile~~ | **No** — keep as convenience constructors. Used in 20+ files, no legacy semantics, never on wire |
| D19 | Split otel_event.rs | **Yes** — otel_log.rs + otel_metric.rs + otel_attributes.rs + otel_event.rs |
| D20 | Document all breaking changes | **Yes** — in this file's Migration Guide |
| D21 | Delete `vector` sink entirely | **Yes** — redundant wrapper around `opentelemetry` sink. Users use `type = "opentelemetry"` directly |
| D22 | Restore native Vector protocol in `vector` source | **Yes** — `event.proto`/`vector.proto` as source-scoped adapter for backward compatibility with original Vector |
| D23 | Delete `metric_tags!` macro, replace with `otel_tags!` | **Yes** — clean break, Sol is a new product. 3 bare-tag sites get manual `AnyValue { value: None }` |
| D24 | Combine A5 (delete vector sink) + A6 (restore native proto) | **Yes** — one coherent step, avoids intermediate broken state |
| D25 | Transformer `only_fields`/`except_fields` paths become OTLP-aware | **Yes** — `body`, `attributes.X`, `resource.X`. Breaking change, documented |
| D26 | `vrl-migrate` tool | **Already built** — `src/vrl_migrate/`, 3-pass rewriter. No new phase needed |
| D27 | Performance gate verified via `cargo bench` | **Yes** — `cargo bench --features remap-benches --bench remap` (VRL), `--features statistic-benches --bench distribution_statistic`. Run before/after Phase B |
| D28 | A6 metric conversion: Counter→Sum, Gauge→Gauge, Set→Gauge+attr, Distribution(H)→Histogram, Distribution(S)→Summary, AggHistogram→Histogram, AggSummary→Summary, Sketch→ExponentialHistogram. Incremental→DELTA, Absolute→CUMULATIVE | **Yes** |
| D29 | A6 log conversion: `Log.value`→body, `Log.fields`→attributes, `message` key promoted to body if value absent | **Yes** |
| D30 | A6 trace conversion: best-effort extraction of trace_id/span_id/name/start_time/end_time from fields, rest→span attributes | **Yes** |
| D31 | A6 `interval_ms` → compute `startTimeUnixNano = timeUnixNano - interval_ms × 1_000_000` | **Yes** |

---

## Autopilot Execution Plan

### Phase A — Clean Deletes and Source/Sink Restructure

Run `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` after each commit.

**A1. Delete `MetricTags` type — replace `metric_tags!` with `otel_tags!`**
- Delete `metric_tags!` macro from `event/metric/mod.rs`
- Replace all ~200 `metric_tags!(...)` call sites with `otel_tags!(...)` across lib/ and src/
- 3 sites with `None` values (bare tags in json.rs, text.rs) → manual `AnyValue { value: None }` construction
- Replace all `.with_metric_tags(Some(MetricTags::from_iter(...)))` with `.with_tags(Some(OtelAttributes::from_iter(...)))`
- Replace all `.with_metric_tags(Some(metric_tags!(...)))` with `.with_tags(Some(otel_tags!(...)))`
- Change `tags_from_key()` in `lib/vector-core/src/metrics/recorder.rs` to return `Option<OtelAttributes>`
- Delete `with_metric_tags()` bridge method on OtelMetric
- Delete `event/metric/tags.rs` (`MetricTags`, `TagValue`, `TagValueSet`)
- Delete `MetricTags` re-export from `event/mod.rs`
- Delete `MetricTags` Arbitrary impl from `event/metric/arbitrary.rs`

**A2. Clean `vector.statistic` attribute writes**
- Remove `VECTOR_STATISTIC` constant from `otel_fields.rs`
- Grep and remove any code that sets `vector.statistic` attribute
- StatisticKind is already deleted — this is residual writes

**A3. Split `otel_event.rs` (7,312 lines)**
- Extract `OtelMetric` + metric helpers → `otel_metric.rs` (~3,000 lines)
- Extract `OtelAttributes` + helpers → `otel_attributes.rs` (~500 lines)
- Keep `OtelLog` + `OtelSpan` + shared helpers in `otel_event.rs` (~3,800 lines)
- Update all imports

**A4. Delete `vector` sink + restore native Vector protocol in `vector` source**

Combined step (D24). The vector sink dies and the vector source becomes a native-proto-only adapter in one coherent commit.

*Delete vector sink:*
- Delete `src/sinks/vector/` entirely (mod.rs, config.rs)
- Remove `sinks-vector` feature from `Cargo.toml`
- Update `component-validation-runner` feature list

*Restore native proto in vector source:*
- Restore `event.proto` and `vector.proto` into `src/sources/vector/proto/`
  - `event.proto`: `EventWrapper`, `Log`, `Metric`, `Trace`, `Value`, etc.
  - `vector.proto`: `service Vector { rpc PushEvents(...) }`, `rpc HealthCheck(...)`
- Add proto compilation via `tonic-build` (in root `build.rs` or source-scoped)
- Implement `proto::vector::Service` for the vector source's `Service` struct:
  - `push_events()`: receives `PushEventsRequest` (Vec<EventWrapper>), converts to `Event` (OtelLog/OtelMetric/OtelSpan), sends through pipeline
- Register `VectorServer` on the gRPC server (the `opentelemetry` source handles OTLP separately)
- Conversion at source boundary (adapter logic in `src/sources/vector/`):
  - **Logs:** `Log.value` → `body` (AnyValue). `Log.fields` → `attributes`. If value absent, promote `message` key from fields to body. If both present, value is body, fields are attributes.
  - **Metrics:** Counter→Sum(isMonotonic=true), Gauge→Gauge, Set→Gauge+`vector.set_values`, Distribution(Histogram)→Histogram, Distribution(Summary)→Summary, AggregatedHistogram→Histogram, AggregatedSummary→Summary, Sketch→ExponentialHistogram. Incremental→DELTA(1), Absolute→CUMULATIVE(2). `interval_ms` → `startTimeUnixNano = timeUnixNano - interval_ms × 1_000_000`.
  - **Traces:** Extract `trace_id`, `span_id`, `parent_span_id`, `name`/`operation_name`, `start_time`, `end_time`, `status` from fields into OtelSpan structured fields. Everything else → span attributes.
  - **Metadata:** `metadata_full` → `EventMetadata` sidecar (source_type, source_id, secrets, upstream_id)
- Rewrite integration tests with native proto test client (no more vector sink dependency)

### Phase B — Eliminate `to_value_canonical()` from internal methods (medium risk)

Run `cargo bench --features remap-benches --bench remap` before Phase B starts (baseline) and after B5 (result).

**B1. Replace `convert_to_fields()` / `convert_to_fields_unquoted()` on OtelLog**
- New implementation: iterate proto fields (body, severity_text, severity_number, time_unix_nano, observed_time_unix_nano, trace_id, span_id, flags, dropped_attributes_count) then `OtelAttributes` entries
- Yield `(KeyString, Value)` pairs without intermediate BTreeMap
- For `_unquoted` variant: same but unquoted keys

**B2. Replace `all_event_fields()` and `all_event_fields_skip_array_elements()` on OtelLog**
- Same proto iteration. The `_skip_array_elements` variant adds a minor filter — same replacement strategy.

**B3. Replace `as_map()` on OtelLog and OtelSpan**
- Build ObjectMap directly from proto fields + attributes
- **Note:** This still allocates, but without the collision guard overhead

**B4. Replace `get(event_root())` on OtelLog**
- Use `Serialize` (OTLP/JSON) → `serde_json::to_value()` → VRL `Value`
- Returns structured OTLP layout, not flat canonical

**B5. Same for OtelSpan** — `convert_to_fields()`, `as_map()`

### Phase C — Migrate external callers (breaking changes)

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

**C14. codec transformer** (`lib/codecs/src/encoding/transformer.rs`)
- Replace `convert_to_fields()` with proto field + attribute iteration for `only_fields`/`except_fields` filtering
- Field paths become OTLP-aware: `body`, `attributes.X`, `resource.X` (breaking change — documented)

**C15. dedupe transform** (`src/transforms/dedupe/transform.rs`)
- Replace `all_event_fields()` with direct proto field + attribute iteration for dedup key extraction

**C16. API trace schema** (`src/api/schema/events/trace.rs`)
- Replace `as_map()` with direct OtelSpan field access

**C17. Test migrations** (batch commit)
- `src/sources/dnstap/mod.rs` — replace `all_event_fields()` in tests
- `src/sinks/postgres/integration_tests.rs` — replace `to_value_canonical()` in tests
- `src/transforms/metric_to_log.rs` — replace `all_event_fields()` in 8 test sites
- `src/sinks/gcp/pubsub.rs` — replace `all_event_fields()` in tests
- `src/sinks/azure_blob/integration_tests.rs` — replace `all_event_fields()` in tests
- `lib/vector-core/src/event/test/{common,mod,serialization}.rs` — replace `as_map()` / `all_event_fields()` in test helpers

### Phase D — Delete `to_value_canonical()` and cleanup

**D1. Delete `to_value_canonical()` from OtelLog**
- Delete method definition (~50 lines)
- Delete collision guard code
- Delete `as_map()`, `convert_to_fields()`, `convert_to_fields_unquoted()`, `all_event_fields()`, `all_event_fields_skip_array_elements()` (all unused after Phase C)

**D2. Delete `to_value_canonical()` from OtelSpan**
- Same cleanup

**D3. Final test sweep + benchmark**
- `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics`
- Verify zero `to_value_canonical` references remain: `grep -rn to_value_canonical lib/ src/`
- Run `cargo bench --features remap-benches --bench remap` — compare with Phase B baseline

### Gate

| Metric | Target | Verification |
|--------|--------|--------------|
| Tests passing | ≥ 2,170 | `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` |
| `to_value_canonical()` call sites | 0 | `grep -rn to_value_canonical lib/ src/` |
| `MetricTags` references | 0 | `grep -rn MetricTags lib/ src/` |
| `vector.statistic` attribute writes | 0 | `grep -rn vector.statistic lib/ src/` |
| `metric_tags!` macro references | 0 | `grep -rn 'metric_tags!' lib/ src/` |
| VRL remap regression | ≤ 5% | `cargo bench --features remap-benches --bench remap` |
| VRL complex path regression | < 20% (was 100-200%) | Same bench, complex path scenarios |

---

## Principles

1. **OTLP/OTel is the only core protocol.** No vendor types in core.
2. **Two-format rule.** OTLP/proto or OTLP/JSON only. No flat canonical format.
3. **Vendor logic in adapters only.** Core never depends on adapters.
4. **`vector.*` attributes are acceptable.** They encode Vector concepts OTLP lacks. Never injected on passthrough paths.
5. **Features preserved.** Tail sampling, load balancing, span_metrics, aggregate — all OTel-native.
6. **Original Vector protocol supported at source boundary.** The `vector` source speaks only the original native gRPC protocol (`event.proto`/`vector.proto`) for backward compatibility. OTLP ingestion is handled by the `opentelemetry` source. The native proto definitions live in the source scope — adapter code, not core types.

---

## Architecture

Sol is a true fork of Vector, rebuilt with OpenTelemetry as its native protocol. The original Vector's proprietary types are gone from core — but Sol retains the original Vector wire protocol as a source adapter, so existing Vector fleets can send data to Sol without any changes.

```
Sources (adapters)              Core (OTel-native)                    Sinks (adapters)
──────────────────────────────  ────────────────────────────────────  ───────────────────────
opentelemetry (gRPC + HTTP)     OtelLog  (LogRecord)                  opentelemetry (gRPC+HTTP)
datadog_agent ──────────────►   OtelMetric (Sum/Gauge/Histogram/  ──► prometheus, influxdb
vector (native gRPC) ─────►     ExponentialHistogram/Summary)   ──► kafka, loki, ES, …
kafka, syslog, … ──────────►   OtelSpan (Span)
                                OtelAttributes (BTreeMap wrapper)
                                Disk buffer: otlp_buffer.proto
```

### What Sol changes from the original Vector

| Aspect | Original Vector | Sol |
|--------|----------------|-----|
| **Core event model** | Proprietary types (`LogEvent`, `Metric`, `TraceEvent`) | OTel-native (`OtelLog`, `OtelMetric`, `OtelSpan`) |
| **Wire protocol** | Custom `event.proto` / `vector.proto` | OTLP (proto + JSON) — the standard |
| **OTLP support** | Partial (source + sink, but not core) | Native — OTLP IS the core |
| **Vendor types in core** | DD sketches, `MetricValue`, `StatisticKind` | None — vendor logic in adapters only |
| **`vector` sink** | Custom proto → another Vector | Deleted — use `opentelemetry` sink |
| **`vector` source** | Custom proto only | Same — original Vector native gRPC only |
| **`opentelemetry` source** | Exists in original | OTLP gRPC + HTTP ingestion (separate from `vector` source) |

### Vector Source: Original Native Protocol

The `vector` source speaks **only** the original Vector native gRPC protocol (`event.proto` / `vector.proto`):

- `service Vector { rpc PushEvents(PushEventsRequest) returns (PushEventsResponse) }`
- Accepts `EventWrapper` (Log, Metric, Trace) from original Vector instances
- Converts at the source boundary: `event.Log` → `OtelLog`, `event.Metric` → `OtelMetric`, `event.Trace` → `OtelSpan`
- **This is an adapter** — the proto definitions live in `src/sources/vector/proto/`, not in core

OTLP ingestion is handled by the separate `opentelemetry` source — each source has one clear protocol.

```
Original Vector ── vector sink (native proto) ──► vector source ──────► Core (OtelLog,
                                                                              OtelMetric,
OTel Collector  ── otlp exporter ──────────────► opentelemetry source ─► OtelSpan)

Sol / any OTLP  ── opentelemetry sink ─────────► opentelemetry source ─►
```

There is **no `vector` sink** — it was a redundant wrapper around `opentelemetry`. To send data to another Sol instance (or any OTLP-compatible receiver), use `type = "opentelemetry"` directly.

### Why keep the original protocol?

The original Vector has **partial** OTLP support — not all versions ship an `opentelemetry` sink, and the OTLP support that exists may not cover all signal types. Supporting the native protocol at the source means:
- **Zero-config migration**: existing Vector fleets can point their `vector` sink at this fork without changing anything
- **No bridge needed**: no OTel Collector middlebox, no sink reconfiguration on the sender side
- **Adapter-scoped complexity**: the `event.proto` / `vector.proto` definitions and conversion logic live entirely within `src/sources/vector/` — core remains pure OTLP
