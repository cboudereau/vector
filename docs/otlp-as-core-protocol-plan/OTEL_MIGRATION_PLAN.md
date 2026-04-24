# OTLP as Core Protocol — Migration Complete

Vector's internal event model and wire protocol have been fully replaced with
OpenTelemetry (OTLP/OTel) as the sole core protocol and in-memory representation.

---

## Result

The `Event` enum has exactly 3 variants — all OTLP-native:

```rust
pub enum Event {
    Log(OtelLog),       // OpenTelemetry LogRecord
    Metric(OtelMetric), // OpenTelemetry Metric (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
    Trace(OtelSpan),    // OpenTelemetry Span
}
```

- `LogEvent`, `Metric`, `TraceEvent` — deleted.
- `NativeSerializer`, `NativeDeserializer`, `NativeJsonSerializer`, `NativeJsonDeserializer` — deleted.
- `AgentDDSketch` — moved to DD source adapter, not in core.
- `DatadogMetricOriginMetadata` — no longer in core event metadata.
- `use_otlp_decoding` flag — eliminated (source always emits OTel-native).
- DD sinks (`src/sinks/datadog/`) — removed (DD accepts OTLP natively).
- Vector source/sink — migrated from native proto to OTLP gRPC (3-service model).
- DD source — clean OTel adapter: emits `OtelMetric`/`OtelSpan` directly.
- VRL migration tool (`vector vrl-migrate`) — ships with ~91% auto-rewrite coverage.
- OTLP HTTP JSON ingestion — native support in the `opentelemetry` source.
- Zero-conversion OTLP path: OTel source → OTel sink (gRPC + HTTP) for all 3 signals.
- `LogNamespace::Legacy` — removed; only `Vector` namespace remains.
- `LegacyKey` type — deleted along with all `_legacy_key` parameters (~200 call sites).
- `LogSchema` struct + `log_schema()` — deleted.
- `event.proto`, `vector.proto` — deleted; disk buffers use `otlp_buffer.proto` only.
- `BufferFormat` enum (Vector/Otlp/Migrate) — collapsed to OTLP-only.
- `Serialize for OtelLog/OtelSpan` — canonical flat format via `to_value_canonical()` (flat keys, compatible with generic sinks like Elasticsearch, console, HTTP). OTLP-native JSON available via `OtlpJsonLog`/`OtlpJsonSpan` wrappers for OTLP HTTP sinks.
- `EventDataEq for OtelLog/OtelSpan` — direct proto + `OtelAttributes` comparison (deterministic ordering).
- `OtelAttributes` — BTreeMap-backed attribute container for O(log n) lookup. Used by OtelLog (record attrs) and OtelSpan (span attrs). Converts to/from `Vec<KeyValue>` at proto boundaries only.

---

## Breaking Changes

### `log_namespace` config option removed (Legacy namespace no longer exists)

The `LogNamespace::Legacy` variant has been removed. All events now use the **Vector** namespace exclusively:

- **Metadata fields** (source_type, timestamp, host, etc.) are stored in **event metadata** under the source name, not on the event root.
- **Event body** is placed at the event root.

**Impact:** Users who had `log_namespace = false` (Legacy mode) in their source configs will see a different event layout:
- Fields like `source_type`, `timestamp`, `host` that were previously on the event root are now in event metadata.
- VRL programs that access these fields at the root (e.g., `.source_type`, `.timestamp`) must be updated to use metadata paths (e.g., `%vector.source_type`, `%<source_name>.timestamp`).

**Migration:** Use `vector vrl-migrate` to automatically rewrite VRL programs for the new namespace layout (~91% auto-rewrite coverage).

The `log_namespace` config field is still accepted (to avoid parse errors from `deny_unknown_fields`) but is ignored — it always resolves to Vector namespace regardless of the value.

### `Serialize` output format changed for OtelLog/OtelSpan

JSON serialization of `OtelLog` and `OtelSpan` now uses the **canonical flat format** produced by `to_value_canonical()`. This flattens proto fields (body, attributes, resource, timestamps) into a single-level object with snake_case keys — compatible with generic sinks (Elasticsearch, console, HTTP, Kafka with JSON encoding).

**Before (legacy LogEvent):** `{"message": "...", "source_type": "syslog", "host": "myhost"}`
**After (canonical OtelLog):** `{"body": "...", "severity_text": "...", "time_unix_nano": 123, "my_attr": "val", "source_type": "syslog"}`

**Impact:** Field names and structure differ from the legacy `LogEvent` layout. `message` → `body`, metadata fields are no longer on the event root (they live in `EventMetadata`). Downstream consumers parsing JSON output must be updated.

For sinks that need OTLP-spec JSON (proto3 camelCase, nested attribute wrappers), use the `OtlpJsonLog` / `OtlpJsonSpan` wrappers directly — the OTLP HTTP sink uses these automatically.

### Native proto wire format removed

The `event.proto` and `vector.proto` wire formats are deleted. The Vector source/sink now speak OTLP gRPC exclusively. Older Vector instances that speak the native proto format cannot communicate with this version.

---

## Architecture

```
Sources (input adapters)       Core (OTel-native only)              Sinks (output adapters)
─────────────────────────────  ────────────────────────────────────  ───────────────────────────
opentelemetry (gRPC + HTTP)    OTel LogRecord                        opentelemetry (gRPC + HTTP)
datadog_agent  ─────────────►  OTel Metric                     ────► prometheus
  DD proto → OTel at boundary    (Sum/Gauge/Histogram/           ────► influxdb, loki, kafka, …
vector (OTLP gRPC)  ────────►    ExponentialHistogram/Summary)
kafka, syslog, …  ──────────►  OTel Span
                               Resource + InstrumentationScope
                               Disk buffer: OtlpBufferBatch proto
```

---

## Guiding Principles (preserved for future contributors)

1. **Baby steps, always green.** Every PR leaves all existing tests passing.
2. **OTLP/OTel is the only core protocol.** No vendor types in core.
3. **Vendor logic lives exclusively in adapters.** Adapters depend on core; core never depends on adapters.
4. **The compiler enforces the boundary.** `cargo build -p vector-core` clean = boundary correct.
5. **No approximations in core.** `ExponentialHistogram` is the correct OTel type. Sketch conversion happens in the DD source adapter.
6. **gRPC internally, HTTP also supported.** OTLP/gRPC for inter-Vector. HTTP at sources and sinks for external integrations.
7. **Features are preserved, not dropped.** APM stats → pipeline telemetry (`span_metrics`), tail sampling, load balancing — all re-implemented with OTel types.

---

## Key Design Decisions

| Decision | Resolution |
|----------|-----------|
| DDSketch vs ExponentialHistogram | ExponentialHistogram in core (tighter error at scale 7: ±0.27% vs ±0.78%). DDSketch only in DD adapter. |
| Core value type | `AnyValue` (OTel proto). `Value` is VRL-boundary only. |
| `EventMetadata` | Retained as pipeline sidecar (finalizers, source_id, schema). Not merged into `Resource.attributes`. |
| `Vec<KeyValue>` attribute lookup | O(n) linear scan was same approach as otelcontribcol. Caused 15–25% regression for VRL (lookup-dominated, 10–20 reads). **Fixed in P11–P12:** `OtelAttributes` BTreeMap wrapper for OtelLog + OtelSpan. OtelMetric data-point attributes remain `Vec<KeyValue>` (deferred). |
| Migration strategy | Wrapper types (incremental, always-compilable) over big-bang replacement. |
| DD sink | Not re-added — DD accepts OTLP natively. Users point OTel sink at DD's OTLP endpoint. |
| Native codecs + protos | Fully deleted. Vector source/sink speak OTLP gRPC natively. |
| Legacy metric types | Retained as internal computation layer — MetricValue/MetricKind/MetricTags provide arithmetic and type-safe operations that OTLP proto lacks. |

---

## Steps Completed

| Step | Description | Key Metric |
|------|-------------|------------|
| **0** | Foundations — buffer toggle, isolation test, span scope fix | — |
| **2** | OTel metric encoder (prerequisite for Step 1) | ~400 lines |
| **1** | DD + Vector sinks removed; OTel sink gRPC added | −10,673 lines |
| **3** | DD source rewritten as clean OTel adapter; DD types leave core | −1,637 lines (DDSketch from core) |
| **5a** | Introduce OTel wrapper types (additive, zero breakage) | +740 lines |
| **5b** | Migrate traces: OTel source/sink emit/accept OtelSpan | +200 lines |
| **5c** | Migrate logs: full pipeline (batch 1 + batch 2, 5c²a–5c²h) | ~55 files |
| **5d** | Migrate metrics: full pipeline (batch 1 + batch 2) | — |
| **5e** | Remove `use_otlp_decoding` flag + legacy deserializer paths | −464 lines |
| **5e²** | OTLP serializer encodes OTel-native events (HTTP sink) | +227 lines |
| **5f** | Ship VRL migration tool (`vector vrl-migrate`) | +1,229 lines |
| **5g** | Rename OtelXxxEvent → OtelXxx + type alias cleanup | net +5 lines |
| **5h** | OTLP HTTP JSON ingestion + dependency upgrades (prost 0.13, tonic 0.12) | +1,464/−559 lines |
| **6a** | Migrate log sources → emit `OtelLog` | 40 files |
| **6b** | Migrate metric sources → emit `OtelMetric` | `From<Metric> for Event` produces OtelMetric |
| **6c** | Verify trace source → `OtelSpan` exclusive | Already done |
| **6d** | Migrate transforms off legacy types | ~14 transforms |
| **6e** | Migrate sinks off legacy types | Via coercion helpers from 5c² |
| **6f** | Remove legacy types from core; rename OtelLog→Log | −4,500/+1,200 lines |
| **6g** | Delete native codecs + ~9,220 test fixtures | −9,400 lines |
| **6h** | Fix remaining test failures | ~103 assertion fixes, 0 failures |
| **7** | DD source metrics/traces → OtelMetric/OtelSpan directly | 47/47 DD tests pass |
| **4a** | Load-balancing sink (consistent hash, static/DNS/K8s resolvers) | — |
| **4b** | Tail sampling transform (8 policy types, decision cache) | 10 tests |
| **4c** | Pipeline telemetry — `span_metrics` transform (RED metrics) | 6 tests |
| **P1-3** | Vector source/sink → OTLP gRPC + delete native proto/event.proto | −7,862 lines |
| **P4** | Collapse LogNamespace to Vector-only (remove Legacy variant) | 90 files, −1,152 lines |
| **P5** | Canonical flat Serialize (`to_value_canonical()`) + direct EventDataEq | — |
| **P8** | Delete log_schema constants, LegacyKey type, clean stale docs | 41 files, −500 lines |
| **P9** | Align test suite with OTLP-native model (value→canonical, schema defs, metadata paths, backpressure timeouts) | 39 files, −738 lines |
| **P10** | Add `protobuf-build` to `sources-opentelemetry` feature (fix Docker build) | 1 file |
| **P11** | `OtelAttributes` BTreeMap for OtelLog — O(log n) attribute lookup | 9 files, +160/−61 lines |
| **P12** | `OtelAttributes` BTreeMap for OtelSpan — O(log n) attribute lookup, tail sampling + span_metrics use direct get | 10 files, +96/−91 lines |

**Net code change:** ~−22,000 lines removed.

---

## Remaining Infrastructure (intentionally kept)

| Component | Why it stays | Planned resolution |
|-----------|-------------|-------------------|
| `to_value_canonical()` / `from_value_map()` | VRL path access and flat-format encoders (GELF, Avro, protobuf, syslog, Lua) depend on Value↔proto bridge. 12 call sites across 8 codec files. | Direct proto access for encoders eliminates most; `OtelAttributes::to_object_map()` already covers attribute portion. |
| `modify_as_value()` | Used by dnstap source/parser for batched mutations (1 production call site + tests). | Replace with direct `OtelAttributes` mutation or sequential `insert()` calls. Low priority. |
| Legacy metric types (~2,164 lines) | `MetricValue` (Counter/Gauge/Set/Distribution/Histogram/Summary arithmetic), `MetricKind` (Incremental/Absolute temporality), `MetricTags` (multi-value tag support, 780 lines), `MetricSeries`/`MetricData` — used by all metric transforms/sinks. | `OtelAttributes` replaces `MetricTags`; arithmetic methods on `OtelMetric` replace `MetricValue` ops. Largest remaining cleanup. |
| `log_namespace: Option<bool>` (37 sources) | Config backward compatibility — parsed but ignored (always Vector namespace). | Remove after deprecation period. Mechanical but touches 37 source files. |
| OtelMetric data-point attributes (`Vec<KeyValue>`) | Each data point (NumberDataPoint, HistogramDataPoint, etc.) has its own `Vec<KeyValue>`. Not a single field like OtelLog/OtelSpan — multiple data points per metric. | Wrap per-data-point attributes in `OtelAttributes`. More complex than Log/Span because metrics have N data points each with their own attribute set. |
| Resource/scope `attribute_value()` free functions | OtelLog and OtelSpan record attributes migrated to `OtelAttributes`, but resource and scope attributes remain `Vec<KeyValue>` with linear scan helpers. Resource/scope attributes are typically small (3–10 entries). | Migrate resource/scope to `OtelAttributes` for consistency. Low perf impact (small lists) but improves code uniformity. |
| `kvlist_to_object_map()` (10 call sites) | Converts `Vec<KeyValue>` to VRL `ObjectMap`. Used at serialization boundaries and VRL target projection. | Replaced by `OtelAttributes::to_object_map()` for record attrs; remaining calls are for resource/scope attrs. |

---

## VRL Performance: Proto ↔ Value Bridge Cost

### Before migration

`LogEvent` stored data as `Value::Object(BTreeMap)` — the same type VRL operates on. VRL field access was **zero-cost**: direct pointer into the existing tree, no allocation, no conversion.

```
VRL .foo  →  BTreeMap::get("foo")  →  &Value (borrowed, zero alloc)
VRL .foo = v  →  BTreeMap::insert("foo", v)  →  in-place O(log n)
```

### After migration (before OtelAttributes optimization)

`OtelLog` stored data as protobuf types (`LogRecord`, `Resource`, `Vec<KeyValue>`). VRL access went through an adapter that converts proto → `Value` per access.

```
VRL .body           →  direct match  →  any_value_to_vrl clone  →  O(1) + 1 alloc
VRL .my_attr        →  linear scan Vec<KeyValue>  →  clone      →  O(n) + 1 alloc
VRL .resource.x     →  direct resource + linear scan             →  O(m) + 1 alloc
VRL .[0] / complex  →  full to_value_canonical() rebuild         →  O(n+m) + many allocs
```

### Regression before OtelAttributes fix

| Workload | Regression | Why |
|----------|------------|-----|
| Body-only (`.body`, `.severity_text`) | ~5% | Clone overhead vs borrow; direct field match is fast |
| Typical remap (5–10 reads, ~15 attrs) | ~15–25% | O(n) linear scan × multiple accesses + per-access alloc |
| Attribute-heavy (20+ reads, 30+ attrs) | ~30–50% | Quadratic-ish: many linear scans over large attribute list |
| Complex paths (array index, unnest) | ~100–200% | Full `to_value_canonical()` per access (rare in practice) |

Two root causes:
1. **O(n) linear scan** of `Vec<KeyValue>` for attribute lookup (vs O(log n) BTreeMap)
2. **Owned `Value` return** on every `get()` — allocates and clones (vs `&Value` borrow)

### After OtelAttributes fix (current, P11–P12)

| Workload | Regression | Why |
|----------|------------|-----|
| Body-only (`.body`, `.severity_text`) | ~5% | Clone overhead vs borrow; direct field match is fast (unchanged) |
| Typical remap (5–10 reads, ~15 attrs) | ~5% | BTreeMap O(log n) + per-access alloc (linear scan eliminated) |
| Attribute-heavy (20+ reads, 30+ attrs) | ~8–10% | BTreeMap scales well; only clone overhead remains |
| Complex paths (array index, unnest) | ~100–200% | Full `to_value_canonical()` per access (rare; unchanged) |

Remaining root cause: **Owned `Value` return** on every `get()` — allocates and clones (vs `&Value` borrow). Fixing this would require VRL to operate on `AnyValue` directly (architecture-level change).

### otelcontribcol comparison

The Go OpenTelemetry Collector has the same issue: `pdata/pcommon.Map` wraps a `*[]KeyValue` slice. `Get()` does a linear scan. They accepted the tradeoff because:

- **Zero-copy**: the slice *is* the proto — no conversion at ingestion or egress
- **Iteration-dominated**: OTTL processors typically `Range()` over all attributes (3–5 statements per event)

Vector's situation differs: VRL programs do **10–20 point lookups** per event (conditionals, branches, assignments each read fields). More lookups × O(n) = bigger impact. And we already pay a per-access conversion cost (`any_value_to_vrl` clone), so the otelcontribcol zero-copy argument doesn't apply.

### Applied optimization: `OtelAttributes` type (P11–P12)

`OtelAttributes` is a `BTreeMap<String, AnyValue>` wrapper that replaces `Vec<KeyValue>` for record-level attributes in `OtelLog` and `OtelSpan`:

```rust
pub struct OtelAttributes {
    inner: BTreeMap<String, AnyValue>,
}
```

**Conversion boundary:** `from_key_values(Vec<KeyValue>)` at source ingestion (one-time O(n log n)), `to_key_values()` / `record_to_proto()` / `span_to_proto()` at sink egress (one-time O(n)). Amortized once per event lifetime vs N times per VRL access before.

**After optimization — OtelLog and OtelSpan:**
```
VRL .body           →  direct match  →  any_value_to_vrl clone  →  O(1) + 1 alloc
VRL .my_attr        →  BTreeMap::get  →  clone                  →  O(log n) + 1 alloc  (was O(n))
VRL .resource.x     →  direct resource + linear scan             →  O(m) + 1 alloc  (resource still Vec<KeyValue>)
```

| Problem | Status |
|---------|--------|
| VRL O(n) lookup (logs + traces) | **Fixed** — BTreeMap O(log n) via `OtelAttributes::get()` |
| `EventDataEq` ordering fragility (logs + traces) | **Fixed** — BTreeMap is sorted; comparison is deterministic |
| `to_value_canonical()` attribute portion | **Fixed** — iterates sorted BTreeMap, no linear scan |
| Tail sampling / span_metrics attribute access | **Fixed** — uses `attribute()` O(log n) instead of iterating `Vec<KeyValue>` |
| Metric data-point attributes | **Not yet** — multiple data points per metric, each with `Vec<KeyValue>` |
| Resource/scope attributes | **Not yet** — still `Vec<KeyValue>` (small lists, low impact) |
| Legacy metric types (MetricTags) | **Not yet** — separate refactor to replace MetricTags with OtelAttributes |

**Estimated result:** typical remap regression drops from ~15–25% to ~5% for logs and traces (just the `AnyValue` → `Value` clone overhead on access, no more linear scan).

---

## Deferred to Future Release

1. **`OtelAttributes` for OtelMetric data points** — Each data point has its own `Vec<KeyValue>`. More complex than Log/Span (N data points per metric × M attributes). Needed for O(log n) metric tag operations.
2. **`OtelAttributes` for resource/scope attributes** — Currently `Vec<KeyValue>` with linear scan helpers. Low perf impact (small lists) but improves code uniformity.
3. **Legacy metric types removal** (~2,164 lines) — Replace `MetricTags` with `OtelAttributes`, add arithmetic methods to `OtelMetric`, eliminate `MetricValue`/`MetricKind`/`MetricSeries`/`MetricData`. Largest remaining cleanup.
4. **Replace `to_value_canonical()` bridge** — Direct proto access in GELF/Avro/protobuf/syslog encoders. 12 call sites across 8 codec files.
5. **Remove `log_namespace: Option<bool>`** config fields once deprecation period ends (37 source files).
6. **Remove `modify_as_value()`** — Replace dnstap usage with direct `OtelAttributes` mutation or sequential inserts.

---

## Reference Documents

| Document | Purpose |
|----------|---------|
| `GUIDELINES.md` | Architectural principles for contributors |
| `VRL_MIGRATION_TOOL.md` | VRL migration tool specification and rewrite rule catalogue |
| `VRL_OTEL_NATIVE_TARGETS.md` | OTel-native VRL target design for all 3 signals |
| `VRL_DATAPOINT_CONTEXT.md` | VRL broadcast for metric data points (`.attributes` shorthand) |
| `PERFORMANCE_AND_TRADEOFFS.md` | Performance analysis, DDSketch vs ExponentialHistogram, otel-collector-contrib comparison |
| `PROTOCOL_GAP_ANALYSIS.md` | Field-by-field gap: Vector native protocol vs OTLP |
| `MIGRATION_STUDY.md` | Component-by-component complexity analysis (historical reference) |
| `MARKET.md` | Market study — observability SaaS competitive landscape (separate concern) |
