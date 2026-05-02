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
| `vector.statistic` attribute | OtelMetric | Distinguishes histogram vs summary distributions. Sinks (prometheus, statsd, influxdb) actively read it. | Keep — functional, no OTel-native alternative |
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
| VRL complex paths (`.attr[0]`) | `as_map()` builds ObjectMap from proto | ✅ Fixed — `to_value_canonical()` deleted |
| Codec encoding | `as_map()` per event | ✅ Fixed — direct ObjectMap, no Value wrapper |

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

**A2. Keep `vector.statistic` attribute** ✅ DONE
- `vector.statistic` is functional (not residual): sinks (prometheus, statsd, influxdb) actively read it via `distribution_statistic()` to distinguish histogram vs summary
- No OTel-native alternative exists — both map to `Histogram` in the proto
- Decision table updated: attribute is kept alongside `vector.metric_type`

**A3. Split `otel_event.rs` (7,213 lines)** ✅ DONE
- Extracted `OtelAttributes` → `otel_attributes.rs` (422 lines)
- Extracted `OtelMetric` + `MetricView` → `otel_metric.rs` (1,852 lines)
- Remaining `otel_event.rs` (5,028 lines): shared helpers, OtelLog, OtelSpan, tests
- All 196 vector-core tests pass

**A4. Delete `vector` sink + restore native Vector protocol in `vector` source** ✅ DONE

*Vector sink deleted:*
- Deleted `src/sinks/vector/` entirely
- Removed `sinks-vector` feature from `Cargo.toml`
- Replaced `VectorSinkConfig` with `OtelSinkConfig` (GrpcConfig) in validation runner config.rs + telemetry.rs

*Native proto restored in vector source:*
- Restored `proto/vector/event.proto` and `proto/vector/vector.proto`
- Added proto compilation to `build.rs` (tonic-build)
- Created `src/sources/vector/convert.rs` — full conversion layer (Log→OtelLog, Metric→OtelMetric with all types, Trace→OtelSpan)
- Created `src/sources/vector/service.rs` — NativeVectorService implementing Vector gRPC trait
- Vector source now speaks both OTLP and native Vector protocol on the same gRPC port

### Phase B — Eliminate `to_value_canonical()` from internal methods (medium risk)

Run `cargo bench --features remap-benches --bench remap` before Phase B starts (baseline) and after B5 (result).

**B1-B5. Eliminate `to_value_canonical()` from internal methods** ✅ DONE
- Extracted `build_canonical_map()` on both OtelLog and OtelSpan
- `to_value_canonical()` is now a thin wrapper: `Value::Object(self.build_canonical_map())`
- `as_map()`, `convert_to_fields()`, `convert_to_fields_unquoted()`, `all_event_fields_skip_array_elements()` all use `build_canonical_map()` directly
- No internal method calls `to_value_canonical()` anymore — only the method definition and one `get()` fallback remain
- All 196 vector-core tests pass

### Phase C — Migrate external callers ✅ DONE

All `to_value_canonical()` call sites migrated to `as_map()`:
- **C1** logfmt encoder — uses `as_map().unwrap_or_default()`
- **C2** GELF encoder — uses `as_map().unwrap_or_default()`
- **C3** Avro encoder — uses `as_map().unwrap_or_default()`
- **C4** protobuf encoder — uses `Value::Object(as_map().unwrap_or_default())`
- **C6** Lua bridge — uses `Value::Object(as_map().unwrap_or_default())`
- **C10** reduce transform tests — uses `Value::Object(as_map().unwrap_or_default())`
- **C12** schema/definition — uses `Value::Object(as_map().unwrap_or_default())`
- **C13** enrichment_tables test — uses `as_map().unwrap_or_default().is_empty()`
- **C17** postgres integration test + otel_event test — migrated

### Phase D — Delete `to_value_canonical()` ✅ DONE

- Deleted `to_value_canonical()` from both OtelLog and OtelSpan
- Deleted `build_canonical_map()` intermediary — logic inlined into `as_map()`
- `as_map()` is now the canonical map builder (was previously a thin wrapper)
- Zero `to_value_canonical` references remain in `lib/` or `src/`
- All 196 vector-core tests pass, full workspace compiles cleanly

### Gate

| Metric | Target | Verification |
|--------|--------|--------------|
| Tests passing | ≥ 2,170 | `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` |
| `to_value_canonical()` call sites | 0 | `grep -rn to_value_canonical lib/ src/` |
| `MetricTags` references | 0 | `grep -rn MetricTags lib/ src/` |
| `vector.statistic` attribute | kept | Functional — used by prometheus/statsd/influxdb sinks |
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
| **`vector` source** | Custom proto only | Dual-protocol: OTLP + original native gRPC on same port |
| **`opentelemetry` source** | Exists in original | OTLP gRPC + HTTP ingestion (separate from `vector` source) |

### Vector Source: Original Native Protocol

The `vector` source speaks **both OTLP and the original Vector native gRPC protocol** on the same port:

- **OTLP**: LogsService, MetricsService, TraceService (for OTel Collector / Sol / any OTLP client)
- **Native Vector**: `service Vector { rpc PushEvents(...) }` (for legacy Vector instances)
- Native proto events are converted at the source boundary: `event.Log` → `OtelLog`, `event.Metric` → `OtelMetric`, `event.Trace` → `OtelSpan`
- The proto definitions live in `proto/vector/event.proto` and `proto/vector/vector.proto` — not in core

The `opentelemetry` source also handles OTLP (gRPC + HTTP) as a dedicated ingestion path.

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
