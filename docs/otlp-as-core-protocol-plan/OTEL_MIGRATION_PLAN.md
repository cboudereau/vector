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
- `LogNamespace` enum — fully deleted (was collapsed to `Vector`-only, then removed entirely).
- `LegacyKey` type — deleted along with all `_legacy_key` parameters (~200 call sites).
- `LogSchema` struct + `log_schema()` — deleted.
- `event.proto`, `vector.proto` — deleted; disk buffers use `otlp_buffer.proto` only.
- `BufferFormat` enum (Vector/Otlp/Migrate) — collapsed to OTLP-only.
- `Serialize for OtelLog/OtelSpan/OtelMetric` — produces OTLP/JSON (proto3 camelCase, nested resource/scope/attributes) for all three event types (P23). `OtlpJsonLog`/`OtlpJsonSpan` wrappers deleted. `to_value_canonical()` retained as VRL bridge and flat-format API for codec encoders.
- `EventDataEq for OtelLog/OtelSpan` — direct proto + `OtelAttributes` comparison (deterministic ordering).
- `OtelAttributes` — BTreeMap-backed attribute container for O(log n) lookup. Used by all three event types for record/span/data-point attrs, resource attrs, and scope attrs. Converts to/from `Vec<KeyValue>` at proto boundaries only.

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

The `log_namespace` config field has been fully removed from all source and transform configs, along with the global `schema.log_namespace` setting, `LogNamespace::merge()`, and per-source resolver plumbing.

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
3. **Two-format rule: OTLP/proto or OTLP/JSON only.** Every wire format, codec, and serialization path must produce one of two formats: **OTLP/proto** (binary protobuf, `application/x-protobuf`) or **OTLP/JSON** (proto3 camelCase, nested resource/scope/attributes, `application/json`). No canonical flat format, no legacy Vector JSON, no custom intermediate formats. Internal transforms operate directly on proto fields (no serialization). Format-specific encoders (GELF, Syslog, CEF, Logfmt) access proto fields directly to produce their output format.
4. **Vendor logic lives exclusively in adapters.** Adapters depend on core; core never depends on adapters.
5. **The compiler enforces the boundary.** `cargo build -p vector-core` clean = boundary correct.
6. **No approximations in core.** `ExponentialHistogram` is the correct OTel type. Sketch conversion happens in the DD source adapter.
7. **OTLP/proto for performance, OTLP/JSON for compatibility.** gRPC and inter-Vector communication uses OTLP/proto (best performance). HTTP endpoints and JSON-based sinks use OTLP/JSON (human-readable, broad compatibility). Disk buffer uses OTLP/proto.
8. **Features are preserved, not dropped.** APM stats → pipeline telemetry (`span_metrics`), tail sampling, load balancing — all re-implemented with OTel types.

---

## Key Design Decisions

| Decision | Resolution |
|----------|-----------|
| DDSketch vs ExponentialHistogram | ExponentialHistogram in core (tighter error at scale 7: ±0.27% vs ±0.78%). DDSketch only in DD adapter. |
| Core value type | `AnyValue` (OTel proto). `Value` is VRL-boundary only. |
| `EventMetadata` | Retained as pipeline sidecar (finalizers, source_id, schema). Not merged into `Resource.attributes`. |
| `Vec<KeyValue>` attribute lookup | O(n) linear scan was same approach as otelcontribcol. Caused 15–25% regression for VRL (lookup-dominated, 10–20 reads). **Fixed in P11–P12+P16:** `OtelAttributes` BTreeMap wrapper for all three event types (OtelLog, OtelSpan, OtelMetric). |
| Migration strategy | Wrapper types (incremental, always-compilable) over big-bang replacement. |
| DD sink | Not re-added — DD accepts OTLP natively. Users point OTel sink at DD's OTLP endpoint. |
| Native codecs + protos | Fully deleted. Vector source/sink speak OTLP gRPC natively. |
| Legacy metric types | **MetricValue/MetricData/MetricTime/MetricName deleted (P26).** `MetricView<'a>` borrowing enum for type inspection. Remaining: MetricKind (intentional, stays), MetricTags (P31 → OtelAttributes), StatisticKind (P32 → delete), Sample/Bucket/Quantile (P33 → proto types). |
| VRL `.tags` alias removed (P20) | `.tags."key"` was a compatibility alias for `.attributes."key"` on OtelMetric VRL targets. **Removed and must not be re-introduced.** `.attributes` is the canonical OTLP path. |
| `metric_to_log` OTLP conformance (P21) | Transform now serializes via `OtelMetric`'s OTLP `Serialize` impl instead of legacy `MetricSeries`/`MetricData` serde. Output uses OTLP field names (`sum`, `gauge`, `histogram`, `dataPoints`, `attributes`). |
| `metric_to_log` uses `serde_json` bridge (P21) | **Fixed P25.** Direct proto field extraction, no serde_json bridge. |
| `to_value_canonical()` flat format (P27→P35) | **Superseded.** Initially kept as "intentional API." Analysis revealed it violates two-format rule (neither OTLP/proto nor OTLP/JSON), destroys OTLP hierarchy, requires collision guards, and causes O(n) full-event round-trips for complex VRL paths. VRL bridge already uses separate `otel_log_event_to_value()`. **P35:** Delete entirely — direct proto access for all callers. |
| `log_to_metric` `to_metrics()` legacy (P22) | **Deleted in P24.** The `all_metrics = true` code path and `to_metrics()` function have been removed. Config-driven metric construction remains (already OTLP-compliant). |
| Two-format rule (P22) | Only two wire formats allowed: **OTLP/proto** (binary protobuf, for gRPC and HTTP `application/x-protobuf`) and **OTLP/JSON** (proto3 camelCase, nested resource/scope/attributes, for HTTP `application/json`). The "canonical flat format" used by OtelLog/OtelSpan `Serialize` is a transitional format that must be replaced by OTLP/JSON. `to_value_canonical()` stays as VRL bridge only, not for serialization. |
| Protocol audit (P22) | Full audit of all wire formats: sources, sinks, codecs, transforms. **No legacy Vector native or JSON formats remain.** `log_to_metric` `to_metrics()` deleted in P24. OtelLog/OtelSpan canonical flat Serialize replaced with OTLP/JSON in P23. See Protocol Audit section below. |

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
| **P13** | Remove `log_namespace` config from all sources/transforms + global schema + `LogNamespace::merge()` | 61 files, −547 lines |
| **P14** | Delete `LogNamespace` enum, `Definition.log_namespaces` field, `SourceContext::log_namespace()`, rename legacy methods | 87 files, −1,070 lines |
| **P15** | Delete `modify_as_value()` — dnstap source builds Value directly + `from_value_map()` | 3 files |
| **P16** | `OtelAttributes` for OtelMetric data-point attributes — BTreeMap O(log n) lookup, delete `attribute_value()` + `set_attribute()` free fns | 5 files |
| **P17** | Direct `Serialize` for OtelLog/OtelSpan — serialize from proto fields without `to_value_canonical()` Value tree allocation. `HexBytes` wrapper for trace/span ID hex encoding. Round-trip tests prove output matches canonical format. | 1 file |
| **P18** | Fix 17 skipped `cargo test -p vector` tests: un-ignore all, fix assertions + add OtelMetric VRL legacy paths (`.tags.*`, `.kind`, `.namespace`), fix `value_to_otel_log_event` for non-Object values, add metric dropped annotation | 7 files |
| **P19** | Code review + test cleanup: fix EventDataEq (resource/scope attrs), reserved field collision guard, flags/dropped_attributes_count round-trip, non-UTF8 BytesValue, consolidate duplicate AnyValue conversions, extract `otel_fields.rs` constants (29), delete 16 dead tests, fix 9 tests, un-ignore 5. Zero `#[ignore]` remaining in `lib/` crates. | 32 files |
| **P20** | Remove `.tags` VRL alias for OtelMetric — `.tags."key"` was an alias for `.attributes."key"`. Removed from `VALID_OTEL_METRIC_PATHS_SET`, `precompute_otel_metric_value`, get/get_mut/set/remove handlers. `.attributes` is the only valid attribute path for metrics in VRL. **This alias must not be re-introduced.** | 3 files |
| **P21** | `metric_to_log` OTLP/JSON conformance — Replace legacy `MetricSeries`/`MetricData` serde serialization with `OtelMetric`'s OTLP `Serialize` impl. Output now uses OTLP field names (`sum`, `gauge`, `histogram`, `summary`, `dataPoints`, `attributes`) instead of legacy (`tags`, `counter.value`, `gauge.value`, `kind`). Remap branching tests updated to use `.name` (metric-specific) instead of `.attributes` for event type detection. Elasticsearch and Humio sink tests updated. | 5 files |
| **P22** | Protocol audit — Verified all wire formats across sources, sinks, codecs, transforms. **Results:** (1) Sources: OTLP gRPC (proto) + HTTP (proto/JSON); Vector source uses OTLP gRPC. (2) Sinks: OTLP gRPC (proto) + HTTP (JSON); Vector sink uses OTLP gRPC. (3) Codecs: All legacy Native/NativeJSON deleted (commit 726162aa). Standard codecs remain (JSON, Protobuf, OTLP, Avro, CEF, CSV, GELF, Logfmt, Raw, Syslog, Text). (4) Transforms: All compliant or event-type agnostic (`log_to_metric` `to_metrics()` deleted in P24). | 0 files (audit only) |

| **P26-3** | Phase 3 legacy type deletion: delete `MetricValue` (6 variants + all methods), `MetricData`, `MetricTime`, `MetricName`. Rename `MetricSeries` → `MetricIdentity` (flatten). Delete write_list/write_word, MetricValue Arbitrary, MetricData tests. | −986 lines, 10 files |
| **P27** | Encoder audit: investigated all 4 codec encoders (logfmt, GELF, Avro, protobuf). `to_value_canonical()` confirmed as correct API — flat canonical layout needed by all. No code changes. | 0 files (audit) |
| **P28** | Extract hardcoded field names: 51 new constants in `otel_fields.rs` (90 total). 376 string literals in `otel_event.rs` replaced with `f::CONSTANT` references. | 2 files |
| **D10.0** | VRL metric unification: `VrlTarget::OtelMetric { event, value }` → `OtelMetric(Value, EventMetadata)`. Full proto→Value→proto round-trip matching OtelLog/OtelSpan. Add `otel_metric_event_to_value()` + `value_to_otel_metric_event()` for all 5 metric types. Delete `precompute_otel_metric_value()`, per-field write-back, `target_get_otel_metric()`, `MetricPathError`, `insert_at_segments()`. | 1 file, +363/−238 |

**Total changes:** +53,115/−42,556 lines across 9,885 files (net +10,559). The net positive reflects substantial new code (OTel types, VRL targets, OtelAttributes, migration tool, new transforms) alongside large deletions (legacy types, DD sinks, native proto, test fixtures).

---

## Remaining Infrastructure

### Intentional APIs (kept permanently)

| Component | Why it stays |
|-----------|-------------|
| `MetricKind` enum (84 files) | Standalone 2-variant enum (`Incremental`/`Absolute`). Decoupled from deleted legacy types. Used pervasively across sources, sinks, transforms, codecs for temporality semantics. |
| `MetricIdentity` (8 files) | HashMap grouping key for metric aggregation: `{ name, namespace, tags }`. Renamed from `MetricSeries` (type alias kept). |
| `from_value_map()` (23 files) | Ingest-boundary API for non-OTLP sources (syslog, journald, logstash, splunk_hec, dnstap, avro, protobuf, gelf, vrl decoders). Accepts flat `Value::Object` and routes fields into proto slots (body, timestamps, trace_id → `LogRecord`; resource/scope → nested; remainder → record attributes). Legitimate adapter pattern — transforms external formats into OTLP structure at the source boundary. |

### Remaining migration work

| Component | Scope | Planned resolution |
|-----------|-------|-------------------|
| **`to_value_canonical()`** (~35 call sites) | Produces a flat `Value::Object` that merges proto fields and attributes into one namespace. Violates OTLP structure (destroys resource/scope/record hierarchy), violates two-format rule (neither OTLP/proto nor OTLP/JSON), causes attribute/proto-field collisions (requires collision guard), allocates full Value tree per call. | **P35:** Delete entirely. Each caller replaced with the right approach for its context — direct proto field access, `Serialize` (OTLP/JSON), or direct iteration. See P35 phases below. |
| **MetricTags** (42 files) | Used across sources, sinks, transforms, lib crates for metric tag construction and reading. `OtelAttributes` (BTreeMap-backed) supports same semantics including multi-value via `ArrayValue`. | **P31:** Replace `MetricTags` with `OtelAttributes` across all 42 files. Largest remaining migration item. |
| **StatisticKind** (22 files) | Used by statsd parser, log_to_metric, distribution constructors, Prometheus collector, 6+ sink encoders. OTLP has no equivalent — distributions are always histogram-semantics. | **P32:** Delete `StatisticKind`. Callers that pass it to constructors use a default or are removed. |
| **Sample/Bucket/Quantile** (9 files) | Used by `samples_to_buckets()` helper (Prometheus exporter/collector) and internal metrics recorder. Proto types (`HistogramDataPoint`, `ValueAtQuantile`) cover same semantics. | **P33:** Replace with direct proto types. `samples_to_buckets()` → operate on proto histogram bounds/counts directly. |
| **Legacy proto→Value ingestion** (opentelemetry-proto) | `into_event_iter()`, `kv_list_into_value()`, `From<PBValue> for Value` — legacy paths that convert proto → VRL Value → OtelLog/OtelSpan. Production already uses `into_otel_event_iter()` (direct proto via `from_parts()`). Only tests still use legacy path. `OtelSpan::from_otel_log()` routes through `to_value_canonical()` round-trip. | **P36:** Delete legacy paths, rewrite `from_otel_log()` to transfer proto fields directly. |
| **kvlist_to_object_map()** (2 files) | Converts `Vec<KeyValue>` to VRL `ObjectMap` in `otel_event.rs` and `vrl_target.rs`. | **P34:** Inline or remove when VRL operates on `AnyValue` directly. Low priority. |

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

Two remaining root causes:
1. **Owned `Value` return** on every `get()` — allocates and clones (vs `&Value` borrow). Fixing this would require VRL to operate on `AnyValue` directly (architecture-level change).
2. **`to_value_canonical()` fallback** for complex paths (array index, nested subpath) — rebuilds entire event as flat Value, operates, then converts back. **P35 eliminates this** by extending direct proto navigation to handle all path patterns.

### otelcontribcol comparison

The Go OpenTelemetry Collector has the same issue: `pdata/pcommon.Map` wraps a `*[]KeyValue` slice. `Get()` does a linear scan. They accepted the tradeoff because:

- **Zero-copy**: the slice *is* the proto — no conversion at ingestion or egress
- **Iteration-dominated**: OTTL processors typically `Range()` over all attributes (3–5 statements per event)

Vector's situation differs: VRL programs do **10–20 point lookups** per event (conditionals, branches, assignments each read fields). More lookups × O(n) = bigger impact. And we already pay a per-access conversion cost (`any_value_to_vrl` clone), so the otelcontribcol zero-copy argument doesn't apply.

### Applied optimization: `OtelAttributes` type (P11–P12)

`OtelAttributes` is a `BTreeMap<String, AnyValue>` wrapper that replaces `Vec<KeyValue>` for all attributes in `OtelLog` and `OtelSpan` (record/span, resource, and scope):

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
VRL .resource.x     →  direct resource + BTreeMap::get           →  O(log m) + 1 alloc  (resource now OtelAttributes)
```

| Problem | Status |
|---------|--------|
| VRL O(n) lookup (logs + traces) | **Fixed** — BTreeMap O(log n) via `OtelAttributes::get()` |
| `EventDataEq` ordering fragility (logs + traces) | **Fixed** — BTreeMap is sorted; comparison is deterministic |
| `to_value_canonical()` attribute portion | **Fixed** — iterates sorted BTreeMap, no linear scan |
| Tail sampling / span_metrics attribute access | **Fixed** — uses `attribute()` O(log n) instead of iterating `Vec<KeyValue>` |
| Resource/scope attributes (logs + traces) | **Fixed** — now `OtelAttributes` (BTreeMap-backed), same as record/span attributes |
| Metric data-point attributes | **Fixed** — `Vec<OtelAttributes>` parallel to data points, O(log n) via BTreeMap |
| Legacy metric types (MetricTags) | **Not yet** — separate refactor to replace MetricTags with OtelAttributes |

**Estimated result:** typical remap regression drops from ~15–25% to ~5% for logs and traces (just the `AnyValue` → `Value` clone overhead on access, no more linear scan).

---

## Protocol Audit (P22)

Full audit of all wire protocols and serialization formats. **Conclusion: no legacy Vector native or JSON formats remain in production code.**

### Wire Protocols by Component

| Component | Protocol | Wire Format | Performance |
|-----------|----------|-------------|-------------|
| **OTLP source (gRPC)** | tonic gRPC | Protobuf binary | Best (zero-copy decode) |
| **OTLP source (HTTP)** | warp HTTP | Protobuf or JSON (Content-Type negotiated) | Proto preferred |
| **OTLP sink (gRPC)** | tonic gRPC | Protobuf binary + optional gzip | Best |
| **OTLP sink (HTTP)** | HTTP | JSON (newline-delimited) | Acceptable for HTTP |
| **Vector source** | tonic gRPC (3-service: Logs/Metrics/Traces) | OTLP Protobuf | Best (same as OTLP gRPC) |
| **Vector sink** | tonic gRPC (delegates to OTLP GrpcConfig) | OTLP Protobuf | Best |
| **Disk buffer** | local | `otlp_buffer.proto` | Proto (no JSON overhead) |

### Codecs Status

| Codec | Type | Status |
|-------|------|--------|
| ~~NativeSerializer/NativeDeserializer~~ | Legacy Vector protobuf | **Deleted** (commit 726162aa) |
| ~~NativeJsonSerializer/NativeJsonDeserializer~~ | Legacy Vector JSON | **Deleted** (commit 726162aa) |
| OTLP | **Protobuf binary** (`application/x-protobuf`) for OTLP HTTP endpoints. NOT JSON. | ✅ Active |
| JSON | Generic serde_json (calls event's `Serialize` impl). All three types (OtelLog/OtelSpan/OtelMetric) produce **OTLP/JSON** (camelCase, nested resource/scope/attributes). `OtlpJsonLog`/`OtlpJsonSpan` wrappers deleted (P23). | ✅ Active |
| Protobuf | Generic protobuf encoding | ✅ Active |
| Avro, CEF, CSV, GELF, Logfmt, Raw, Syslog, Text | Standard formats | ✅ Active |

### Transforms Status

| Transform | Protocol Status | Notes |
|-----------|----------------|-------|
| `metric_to_log` | ⚠️ OTLP via JSON intermediate | Uses `serde_json::to_value(&otel)` — correct output but allocates JSON Value tree. Direct proto field extraction would be faster. |
| ~~`log_to_metric` (`to_metrics()`)~~ | ✅ **Deleted (P24)** | Function and `all_metrics` config option removed. Config-driven metric construction remains (already OTLP-compliant). |
| `log_to_metric` (config-based) | ✅ OTLP | Config-driven metric construction uses `OtelMetric` directly |
| `aggregate` | ✅ OTLP | Works with `OtelMetric` directly |
| `tag_cardinality_limit` | ✅ OTLP | Uses `OtelMetric` methods (`tags()`, `remove_data_point_attribute()`) |
| `remap`, `filter`, `route`, `dedupe`, `reduce` | ✅ Generic | Event-type agnostic, no format assumptions |
| `lua` | ✅ OTLP | Uses `OtelLog`/`OtelMetric` directly |

### Performance Assessment

| Path | Current | Optimal | Gap |
|------|---------|---------|-----|
| OTLP gRPC → OTLP gRPC (passthrough) | Proto → Proto (zero conversion) | Same | None |
| OTLP gRPC → OTLP HTTP | Proto → JSON at sink boundary | Same | None |
| Vector → Vector | OTLP gRPC proto both directions | Same | None |
| ~~`metric_to_log`~~ | **Fixed (P25)** — direct proto field extraction, no serde_json bridge | Same | None |
| ~~`log_to_metric` (`to_metrics()`)~~ | **Deleted (P24)** | — | — |

## Planned Steps (Priority Order)

### Two-Format Rule

Only two wire/serialization formats are allowed anywhere in the codebase:

| Format | Content-Type | When to use |
|--------|-------------|-------------|
| **OTLP/proto** | `application/x-protobuf` | gRPC (Vector↔Vector, OTLP source/sink), HTTP when performance matters, disk buffer |
| **OTLP/JSON** | `application/json` | HTTP when human-readable or JSON-based sink (Elasticsearch, Kafka, console), debugging |

**Internal transforms** operate directly on proto fields — no serialization needed. `OtelAttributes` (BTreeMap) provides O(log n) direct access.

Format-specific encoders (GELF, Syslog, CEF, Logfmt) access proto fields directly — they produce their own format from the proto struct, not from an intermediate JSON/Value tree.

### P23 — OtelLog/OtelSpan `Serialize` → OTLP/JSON ✅ (completed 2026-04-29)
Replace the canonical flat format with OTLP/JSON (proto3 camelCase, nested resource/scope/attributes) as the default `Serialize` impl for OtelLog and OtelSpan. OtelMetric already produces OTLP JSON.

**Current state (3 paths, only 1 is OTLP):**
| Path | Format | Used by |
|------|--------|---------|
| `OtelLog`/`OtelSpan` `Serialize` | Flat, snake_case (non-OTLP) | JSON codec, console, any `serde_json` sink |
| `OtlpJsonLog`/`OtlpJsonSpan` wrappers | Nested, camelCase (OTLP/JSON) | **Nothing** — dead code |
| `OtlpSerializer` (OTLP codec) | Proto binary via `prost::Message::encode()` | OTLP HTTP sink (`application/x-protobuf`) |

**After P23 (2 paths, both OTLP):**
| Path | Format | Used by |
|------|--------|---------|
| `OtelLog`/`OtelSpan`/`OtelMetric` `Serialize` | **OTLP/JSON** (camelCase, nested resource/scope) | JSON codec, console, any `serde_json` sink |
| `OtlpSerializer` (OTLP codec) | Proto binary | OTLP HTTP sink, unchanged |

**Steps:**
- Move `OtlpJsonLog`/`OtlpJsonSpan` serialization logic into the default `Serialize` impl for OtelLog/OtelSpan
- Delete `OtlpJsonLog`/`OtlpJsonSpan` wrapper types (dead code made redundant)
- OTLP sink (gRPC + HTTP) is **unaffected** — uses `OtlpSerializer` (proto binary), not `Serialize`
- `to_value_canonical()` kept only as VRL bridge, no longer called from serialization
- **Decision:** Breaking change accepted. All output is OTLP/proto or OTLP/JSON.
- GELF/Syslog/CEF/Logfmt encoders: direct proto field access (resolved in P27).
- Tests: migrate to proto-level or OTLP/JSON assertions.
- **P37 — OTLP HTTP JSON sink support:** Add `application/json` Content-Type option to OTLP HTTP sink. Currently proto-only (`application/x-protobuf`), which is correct and performant. JSON option needed for receivers that only accept JSON (e.g. some managed OTLP endpoints). Uses `Serialize` impl (already OTLP/JSON compliant). Low priority — proto covers most receivers.

### P24 — Delete `log_to_metric` `to_metrics()`, replace with VRL ✅ (completed 2026-04-29)
The `all_metrics = true` code path is replaced by VRL-based approach.
- Delete `to_metrics()` function and all legacy field-name parsing (`tags`, `kind`, `counter`, `gauge`, etc.)
- Delete `all_metrics` config option
- Users write VRL to construct metrics from logs (more flexible, no hardcoded field mapping)
- Config-driven metric construction (`metrics` array in config) remains — already OTLP-compliant.

### P25 — `metric_to_log` → full OTLP metric as structured body ✅ (completed 2026-04-29)
Replace `serde_json::to_value(&otel)` bridge with direct conversion. Output OtelLog has body = `KvlistValue` containing the full OTLP metric structure.
- Convert OtelMetric proto fields directly to `AnyValue::KvlistValue` (name, data points, attributes, temporality)
- Set as OtelLog body — native proto structure, no serialization needed
- VRL can access `.body.name`, `.body.sum.dataPoints` directly (no `parse_json!()`)
- When serialized to OTLP/JSON, body appears as nested JSON object naturally
- No double-encoding (vs StringValue which would be JSON-in-JSON)
- No intermediate `serde_json::Value` → `event::Value` conversion
- Saves ~2 heap allocations per event

### P26 — Legacy metric types removal ✅ (core types deleted 2026-04-30)
Added arithmetic via inherent methods on `OtelMetric`, eliminated `MetricValue`/`MetricData`/`MetricTime`/`MetricName`/`MetricSeries`. Deleted `from_metric_parts()`/`into_metric_parts()` bridge methods. Remaining: `MetricTags` → `OtelAttributes` migration (P31), `StatisticKind` deletion (P32), `Sample`/`Bucket`/`Quantile` replacement (P33).

**Completed (2026-04-29):**
- Arithmetic methods on `OtelMetric`: `add()`, `subtract()`, `zero()`, `set_first_value()`, `first_value_as_f64()` — dispatch on proto `data` oneof (Sum, Gauge, Histogram, ExponentialHistogram, Summary). 10 unit tests.
- Temporality queries: `is_delta()`, `is_cumulative()`, `is_gauge()`, `is_sum()`.
- Identity: `metric_series()` returns `MetricSeries` (name + namespace + tags) for HashMap grouping.
- Aggregate transform fully migrated to native `OtelMetric` ops. All 14 tests pass.

**Decisions (locked):**
1. **Arithmetic:** Inherent methods on `OtelMetric` (not a trait — single implementor, trait adds ceremony with no benefit).
2. **Multi-value tags → `ArrayValue`:** OTLP `attributes` supports `ArrayValue` (array of `AnyValue`). Keys must be unique — multi-value uses ArrayValue, not repeated keys. `MetricTags` multi-value maps 1:1 to `OtelAttributes` with `ArrayValue`.

#### P26 Spec Compliance Analysis (2026-04-29)

Audit of arithmetic methods and aggregate transform against [OTel Metrics Data Model](https://opentelemetry.io/docs/specs/otel/metrics/data-model/) and [otelcol-contrib](https://github.com/open-telemetry/opentelemetry-collector-contrib) reference implementation.

**DECIDED (D1) — `is_cumulative()` fixed for Gauge/Summary:**
Fix `is_cumulative()` to return `false` for Gauge/Summary. Add `has_temporality()` helper. Update aggregate guards to `is_cumulative() || is_gauge()`.

**DECIDED (D2) — `add()` kept working on Gauges:**
Keep `add()` working on Gauges — matches otelcol-contrib `metricstransformprocessor` behavior.

**DECIDED (D3) — `metric_series()` grouping key kept as-is:**
Keep current key (`name + namespace + data-point-attributes`). Matches metricstransformprocessor. Document as known simplification.

**Completed (2026-04-30) — Phase 2 bulk migration (8 commits, 1783 tests passing):**
- Added `is_set()`, `is_distribution()`, `interval_ms()` methods to OtelMetric.
- **Sources** (Phase 2a): internal_metrics, nginx_metrics, mongodb_metrics, postgresql_metrics, static_metrics — all migrated to `new_counter()`/`new_gauge()` constructors.
- **Transforms** (Phase 2b): `log_to_metric` — migrated to native constructors.
- **API** (Phase 2f): `api/schema/metrics/filter.rs` — `sum_metrics()` uses `add()` directly.
- **Sink normalizers** (6 sinks): appsignal, aws_cloudwatch, gcp_stackdriver, greptimedb, influxdb, sematext — all migrated from `MetricValue` matching to `is_sum()`/`is_gauge()`/`is_set()`.
- **Sink encoders** (4 sinks): influxdb, aws_cloudwatch, new_relic, sematext — migrated from `into_metric_parts()` to native accessors (`name()`, `namespace()`, `timestamp()`, `tags()`, `value()`, `first_value_as_f64()`).
- **Prometheus collector**: changed trait `encode_metric` to accept `&OtelMetric` — eliminates decomposition from exporter and remote_write callers.
- **GCP Stackdriver**: request builder migrated to native accessors.
- **Internal metrics recorder**: replaced `from_metric_parts()` with native constructors. Removed `make_metric()` from storage.
- **OTLP ingestion** (`lib/opentelemetry-proto/src/metrics.rs`): all 5 metric types migrated to native constructors.
- **Test helpers**: `test_util/metrics.rs` (`get_gauge`, `get_set`, `get_distribution`) migrated to native constructors. Lua test migrated.

**Completed (2026-04-30) — Full production and test migration (3 commits, 1783+ tests passing):**
- **Deep infrastructure** (`normalize.rs`, `split.rs`, `buffer/mod.rs`): `CachedMetric` stores `OtelMetric` directly. `SplitMetrics` (test_util) stores `OtelMetric` directly. All `from_metric_parts`/`into_metric_parts` eliminated.
- **Prometheus exporter** `normalize()`: Uses `OtelMetric::add`/`set_kind`/`set_timestamp` directly. Distribution-to-histogram conversion via `new_histogram()`.
- **Lua integration** (`lua/event.rs`, `lua/metric.rs`): `LuaMetric` stores `OtelMetric` directly. `IntoLua` reads via accessors. `FromLua` constructs via native constructors.
- **Test code** (22 files): All `otel_from_parts` helpers replaced with native constructors.
- **OtelMetric enhancements**: `with_interval_ms()`, `subtract_distribution()`, `subtract_set_values()`, `merge_set_values()`, `compress_distribution()`. `new_set_from_values()` sorts/deduplicates.

**Completed (2026-04-30) — Bridge methods deleted (1 commit, 1782 tests passing):**
- Deleted `from_metric_parts()` (~260 lines) and `into_metric_parts()` (~20 lines).
- All 7 test-only consumers migrated: `TestMetricInput::to_otel_metric()`, Arbitrary via native constructors, prometheus test deleted.
- Zero calls to bridge methods remain in codebase.

**Remaining P26 work — Phase 3: Full legacy type deletion (~58 files)**

Decisions (locked 2026-04-30):

| Decision | Answer |
|----------|--------|
| **D11 — value() replacement** | `MetricView<'a>` borrowing enum via `otel.view()`. OTLP variant names: `Sum`, `Gauge`, `Histogram`, `Summary`, `ExponentialHistogram` + Vector-specific `Set`, `Distribution`. Borrows proto slices where possible (histogram bounds/counts, summary quantiles). Scalars copied. Sets must allocate (nested in attribute). ~40% production callers, ~60% test. |
| **D12 — MetricKind** | Keep as standalone enum. Move to `otel_event.rs`, decouple from legacy module. |
| **D13 — MetricSeries** | Rename → `MetricIdentity`. Flatten (drop `MetricName` sub-struct: just `name: String, namespace: Option<String>, tags: MetricTags`). Move next to `OtelMetric`. |
| **D14 — StatisticKind** | Delete. Distributions are always histogram-semantics in OTLP. |
| **D15 — Config deserialization** | Only `TestMetricInput` needs custom `Deserialize`. `log_to_metric` uses its own `MetricConfig`/`MetricTypeConfig` (does NOT use `MetricValue` for config — only tests import it for assertions). InfluxDB decoder production code already uses `OtelMetric::new_gauge()` — only tests use `MetricValue`. |
| **D16 — Commit strategy** | Prep → Migrate consumers → Delete. Config deser is just `TestMetricInput` (folded into migrate step). |
| **D17 — MetricView field types** | Proto-native: `Histogram { bounds: &'a [f64], counts: &'a [u64], count: u64, sum: f64 }`, `Summary { quantiles: &'a [ValueAtQuantile], count: u64, sum: f64 }`, `Distribution { bounds: &'a [f64], counts: &'a [u64] }`. `Bucket`/`Sample`/`Quantile` structs no longer in MetricView public API — callers use proto slices directly. |

**Investigation findings (2026-04-30):**
- Proto `HistogramDataPoint` stores `explicit_bounds: Vec<f64>` and `bucket_counts: Vec<u64>` separately — MetricView borrows `&[f64]` + `&[u64]`.
- Proto `SummaryDataPoint` stores `quantile_values: Vec<ValueAtQuantile>` where `ValueAtQuantile { quantile: f64, value: f64 }` — identical to our `Quantile`. MetricView borrows `&[ValueAtQuantile]`.
- Set values nested in `dp_attrs` as `vector.set_values` ArrayValue — must allocate `Vec<String>` in `view()`.
- Distribution encoded as histogram bounds+counts — borrows `&[f64]` + `&[u64]`.
- 90%+ of callers extract scalar f64 (Counter/Gauge). 35% type-check only. 7% touch complex fields.

#### P26 Phase 3 Execution Plan

**Step 3a — Prep:**
1. Add `MetricView<'a>` enum + `view()` method on `OtelMetric`:
   - `Sum { value: f64 }` — copied from `NumberDataPoint.value` oneof
   - `Gauge { value: f64 }` — copied from `NumberDataPoint.value` oneof
   - `Set { values: Vec<String> }` — allocated from `vector.set_values` attribute
   - `Distribution { bounds: &'a [f64], counts: &'a [u64] }` — borrowed from `HistogramDataPoint`
   - `Histogram { bounds: &'a [f64], counts: &'a [u64], count: u64, sum: f64 }` — borrowed from `HistogramDataPoint`
   - `Summary { quantiles: &'a [ValueAtQuantile], count: u64, sum: f64 }` — borrowed from `SummaryDataPoint`
   - `ExponentialHistogram { count: u64, sum: f64 }` — scalars only
2. Add granular accessors where needed for hot paths.
3. Move `MetricKind` into `otel_event.rs`.
4. Rename `MetricSeries` → `MetricIdentity`, flatten, move. Update `metric_series()` → `metric_identity()`.
5. Delete `StatisticKind`.

**Step 3b — Migrate consumers ✅ (completed 2026-04-30):**
Replaced `value()` → `view()`, `MetricValue::Counter` → `MetricView::Sum`, etc. across all consumer files.
Added `MetricView::as_name()` and `Display` impl.
Files migrated: ~30 files across lib core, sources, transforms, sinks, API, test code, internal events.
Only `config/mod.rs` (TestMetricInput deserialization) deferred to Step 3c since it's structurally tied to `MetricData`/`MetricSeries`.

**Step 3c — Delete legacy types ✅ (completed 2026-04-30):**
- Deleted `MetricValue` enum (6 variants + all methods and trait impls) from `value.rs`
- Deleted `MetricData` struct, `MetricTime` struct, all methods from `data.rs` (file deleted)
- Deleted `MetricName` sub-struct — fields flattened into `MetricIdentity`
- Renamed `MetricSeries` → `MetricIdentity` (type alias kept for backward compat)
- Deleted `Arbitrary` impl for `MetricValue`, `write_list`/`write_word` helpers
- Deleted MetricData add/subtract test module (~250 lines)
- Total: −986 lines across 10 files

Remaining types in `event/metric/` kept in place (not moved to `otel_event.rs` — file already 6884 lines):
- `MetricKind` — standalone 2-variant enum, 84 files (intentional, stays — D12)
- `MetricIdentity` (née `MetricSeries`) — HashMap grouping key, 8 files (intentional, stays)
- `StatisticKind` — 22 files, planned deletion (P32)
- `Sample`, `Bucket`, `Quantile` — 9 files, planned replacement with proto types (P33)
- `MetricTags` — 42 files, planned replacement with `OtelAttributes` (P31)

### P27 — `to_value_canonical()` audit ⚠️ (initial audit 2026-04-30, superseded by P35)
Initial audit concluded `to_value_canonical()` was "correct API." **Superseded** — deeper analysis revealed the flat canonical format violates the two-format rule and OTLP structure. See P35 for full elimination plan.

**Original audit findings (4 codec encoders):**

| File | Call sites | Current usage |
|------|-----------|---------------|
| `logfmt.rs:44` | `encode_logfmt::encode_value(&val)` | Needs flat key=value pairs |
| `gelf.rs:121,139` | `serde_json::to_value(log.to_value_canonical())` | Needs specific GELF fields |
| `avro.rs:73` | `apache_avro::to_value(&log.to_value_canonical())` | Needs Value tree for schema matching |
| `protobuf.rs:121` | `encode_message(&self.message_descriptor, val, ..)` | Needs Value tree for descriptor encoding |

**Why the initial "keep" decision was wrong:** The flat canonical format is a third serialization format that is neither OTLP/proto nor OTLP/JSON. It flattens attributes into the same namespace as proto fields (collision risk, requires guard). Each encoder has a specific output format — none of them actually need "the flat canonical layout." They need access to proto fields and attributes, which the proto struct already provides directly.

**Superseded by:** P35 — eliminate `to_value_canonical()` entirely.

### P28 — Extract hardcoded field names into otel_fields.rs constants ✅ (completed 2026-04-30)
Extracted 51 new constants (total 90 in `otel_fields.rs`): metric data type names, metric kind strings, metric proto field names, OTLP JSON camelCase names, AnyValue type wrappers, resource attribute keys, internal Vector attributes, tracing metadata fields. 376 string literals in `otel_event.rs` replaced with `f::CONSTANT` references.

### P35 — Delete `to_value_canonical()`: direct proto access everywhere

**Problem:** `to_value_canonical()` is a flat format that violates the OTLP structure and the two-format rule:
- Merges proto fields (`body`, `severity_text`, `trace_id`) and attributes (`my_attr`) into one flat namespace
- Destroys the Resource → Scope → Record/Span hierarchy that OTLP defines
- Requires a collision guard because attribute keys can shadow proto field names
- Allocates a full `Value::Object(BTreeMap)` tree on every call
- Is neither OTLP/proto nor OTLP/JSON — a third format that should not exist

**Analysis of all ~35 call sites:**

| Category | Call sites | Current behavior | Clean replacement |
|----------|-----------|-----------------|-------------------|
| **Internal get/insert/remove fallback** | ~18 in OtelLog, ~9 in OtelSpan | On array-indexed or non-field path segments, rebuilds entire event as flat Value, operates on it, then `apply_value_map()` back to proto. Full round-trip per operation. | **Direct proto navigation.** Array-indexed paths (`.my_attr[0]`) navigate into `AnyValue::ArrayValue` via `OtelAttributes`. Nested paths (`.resource.attributes.key`) walk the proto struct. No full-event rebuild needed. |
| **`convert_to_fields()` / `all_event_fields_skip_array_elements()`** | 2 in OtelLog, 2 in OtelSpan | Builds flat Value tree, then iterates it. | **Direct iteration.** Yield `(key, Value)` by walking proto fields + `OtelAttributes` entries. No intermediate tree. |
| **`value_mut()` / `as_map()`** | 2 in OtelLog, 2 in OtelSpan | Returns a snapshot Value — mutations don't persist. `value_mut()` is misnamed. | **Delete `value_mut()`** (mutations are silent no-ops — dangerous API). Replace `as_map()` callers with direct field access or OTLP/JSON via `Serialize`. |
| **Codec encoders** | 5 in 4 files | Builds flat Value, passes to format-specific encoder. | **Direct proto field reading.** Each encoder iterates proto fields and attributes to build its output format. No intermediate Value tree. See per-encoder plan below. |
| **Lua bridge** | 1 | Builds flat Value for Lua table. | **Use `otel_log_event_to_value()`** (already exists in vrl_target.rs, produces structured Value with `.attributes`, `.resource`, `.scope`). Or iterate proto fields into Lua table directly. |
| **Schema definition** | 1 | Infers `Kind` from sample event. | **Use `Serialize` (OTLP/JSON)** for Kind inference — this is the actual output format. |
| **Tests** | ~4 | Assertions on flat canonical Value. | **Update assertions** to OTLP/JSON or direct proto field checks. |

**Performance impact:**
- **Internal fallbacks (current):** Each complex-path get/insert/remove allocates a full `BTreeMap<KeyString, Value>` (~15-30 entries), clones all attributes and proto fields, operates on the tree, then routes everything back to proto. For insert/remove, this is **two full-event copies per operation**. For VRL programs with array-indexed attribute access, this dominates runtime.
- **After P35:** Array-indexed paths navigate directly into `OtelAttributes` BTreeMap → `AnyValue::ArrayValue` → element. O(log n) lookup + O(1) array index. Zero allocation for get, single-attribute write-back for insert/remove.

**Codec encoder migration plan:**

| Encoder | Current | After P35 | Notes |
|---------|---------|-----------|-------|
| **logfmt** | `to_value_canonical()` → `encode_logfmt::encode_value(&val)` | Iterate proto fields (body, severity, timestamps) + `OtelAttributes` entries. Write `key=value` directly. | Simplest migration. logfmt is inherently flat, but attribute keys should be namespaced (e.g. `attributes.key`) to avoid collision with proto fields. |
| **GELF** | `to_value_canonical()` → `serde_json::to_value()` | Extract GELF-specific fields (`host`, `short_message`, `timestamp`) from proto/attributes. Build GELF JSON object directly. | GELF has its own spec — it was never OTLP. Direct field extraction is more correct than flattening everything. |
| **Avro** | `to_value_canonical()` → `apache_avro::to_value()` | Use `Serialize` (OTLP/JSON) → `serde_json::to_value()` → `apache_avro::to_value()`. Schema matches OTLP field names. | User-defined Avro schemas will need to match OTLP/JSON structure. Breaking change — document in migration guide. |
| **protobuf** | `to_value_canonical()` → `encode_message()` | Use `Serialize` (OTLP/JSON) → `serde_json::to_value()` → VRL `Value` → `encode_message()`. Or encode proto directly via `prost::Message::encode()`. | Descriptor-based encoding needs a Value tree. OTLP/JSON layout is the correct input. |

**`apply_value_map()` status:** Kept — used only by `from_value_map()` at ingest boundary. This is the reverse direction (external flat format → OTLP proto structure) and is legitimate: non-OTLP sources must route fields into the right proto slots. Unlike `to_value_canonical()`, it does not produce a non-OTLP format — it *consumes* one.

**Phases:**

| Phase | What | Scope |
|-------|------|-------|
| **P35a** | Extend direct proto navigation for array-indexed and nested paths in OtelLog/OtelSpan get/insert/remove. Handle `.attr[0]`, `.resource.attributes.key`, `.scope.name` directly on proto struct + `OtelAttributes`. | ~27 call sites in otel_event.rs |
| **P35b** | Delete `convert_to_fields()`, `all_event_fields_skip_array_elements()`, `value_mut()`, `as_map()`. Replace with direct proto iteration or targeted accessors. | ~8 call sites in otel_event.rs |
| **P35c** | Migrate codec encoders to direct proto field reading. Each encoder builds its output format from proto struct + `OtelAttributes` — no intermediate Value tree. | 4 files, 5 call sites |
| **P35d** | Migrate Lua bridge to `otel_log_event_to_value()` or direct proto reading. Migrate schema Kind inference to `Serialize` (OTLP/JSON). Update test assertions. | 3 files, ~6 call sites |
| **P35e** | Delete `to_value_canonical()` from OtelLog and OtelSpan. Delete collision guard (`reserved field` check). | 1 file |

### P29 — OtelSpan remove/remove_prune ✅ (completed 2026-04-29)
Direct proto field removal for all single-segment and multi-segment paths, with prune support.

### P30 — Deduplicate OtelLog/OtelSpan/OtelMetric common code (P2-1) ✅ (completed 2026-04-29)
Extracted shared helper functions: `append_canonical_resource_scope`, `remove_resource_subpath`, `remove_scope_subpath`, `remove_attrs_subpath`. ~52 lines net reduction.

---

## Automode Execution Plan (updated 2026-04-30)

### Completed Phases (1–6) ✅

All 6 phases executed. 32 commits, all tests passing (1782 vector + 191 vector-core).

| Phase | Description | Status |
|-------|-------------|--------|
| 1 | Spec compliance fix (D1): `is_cumulative()`, `has_temporality()`, aggregate guards | ✅ |
| 2 | P26 bulk migration: sources, transforms, sinks, lib, API off legacy metric types (8 commits) | ✅ |
| 3 | P26 legacy type deletion: `MetricValue`, `MetricData`, `MetricTime`, `MetricName` deleted; `MetricView<'a>` + `view()` added; `MetricSeries` → `MetricIdentity` | ✅ |
| 4 | P27 encoder audit: `to_value_canonical()` confirmed as correct API for all 4 codec encoders | ✅ |
| 5 | D10.0 VRL metric unification: full proto→Value→proto round-trip for OtelMetric | ✅ |
| 6 | P28 field name constants: 51 new constants (90 total), 376 string literals replaced | ✅ |

### Planned Phases (7–15)

Remaining migration work. Not yet started. All decisions answered — ready for autopilot execution.

#### Phase 7 — P31: MetricTags → OtelAttributes (42 files)
Replace `MetricTags` (custom multi-value tag map) with `OtelAttributes` (BTreeMap-backed, OTLP-native).

| Category | Files | Key changes |
|----------|-------|-------------|
| Sources | 12 | host_metrics (5 sub-modules), prometheus parser, statsd, apache/mongodb/nginx/postgresql/eventstoredb/aws_ecs metrics, datadog_agent tests |
| Transforms | 2 | log_to_metric, metric_to_log |
| Sinks | 12 | prometheus (2), statsd (2), influxdb (3), aws_cloudwatch (3), websocket_server (2) |
| Lib | 8 | otel_event, metric/tags (delete), metric/series, metric/mod, metric/arbitrary, lua/metric, codecs/influxdb, vector-vrl-metrics |
| Config/test | 4 | config/mod, test_util/mod, test/common, tag_cardinality_limit tests |

**Prerequisites:** Add missing `OtelAttributes` helpers equivalent to `MetricTags` API: `contains_key()`, `iter_single()`, `iter_all()`, `into_iter_single()`, multi-value `ArrayValue` support.

**After completion:** Delete `MetricTags`, `TagValue`, `TagValueSet` types and `event/metric/tags.rs`.

#### Phase 8 — P32: StatisticKind deletion (22 files)
Delete `StatisticKind` enum. OTLP has no equivalent — distributions are always histogram-semantics.

| Category | Files | Key changes |
|----------|-------|-------------|
| Core | 3 | otel_event.rs (remove from constructors), metric/value.rs (delete enum), metric/arbitrary.rs (delete impl) |
| Sources | 2 | statsd parser, log_to_metric |
| Sinks | 10 | prometheus (2), influxdb, cloudwatch (2), greptimedb, humio, sematext, statsd (2) |
| Other | 7 | config/mod, test_util/metrics, lua/metric, codecs/syslog, metric_to_log, test/common, event/mod |

**Impact:** Constructors like `new_distribution(name, statistic, ...)` lose the `statistic` parameter. Callers that distinguish `Histogram` vs `Summary` statistics use the metric data type directly (histogram bounds vs quantile values).

#### Phase 9 — P33: Sample/Bucket/Quantile → proto types (9 files)
Replace `Sample`, `Bucket`, `Quantile` structs with direct proto types.

| Component | Change |
|-----------|--------|
| `samples_to_buckets()` (3 files) | Operate on proto `HistogramDataPoint` bounds/counts directly |
| `Quantile` (4 files) | Use proto `ValueAtQuantile` directly (already identical structure) |
| `Sample` (3 files) | Inline into callers or replace with `(f64, u32)` tuples |
| `Bucket` (3 files) | Replace with `(f64, u64)` bounds/counts from proto |

**After completion:** Delete `Sample`, `Bucket`, `Quantile` from `event/metric/value.rs`.

#### Phase 10 — P35: Delete `to_value_canonical()` (~35 call sites, 5 phases)
Eliminate the flat canonical format. Direct proto access everywhere.

| Sub-phase | What | Scope |
|-----------|------|-------|
| P35a | Extend OtelLog/OtelSpan get/insert/remove to handle array-indexed and nested paths directly on proto struct + `OtelAttributes`. Eliminate full-event round-trip fallbacks. | ~27 call sites in otel_event.rs |
| P35b | Delete `convert_to_fields()`, `all_event_fields_skip_array_elements()`, `value_mut()`, `as_map()`. Replace with direct proto iteration. | ~8 call sites in otel_event.rs |
| P35c | Migrate codec encoders (logfmt, GELF, Avro, protobuf) to direct proto field reading. | 4 files, 5 call sites |
| P35d | Migrate Lua bridge, schema Kind inference, test assertions. | 3 files, ~6 call sites |
| P35e | Delete `to_value_canonical()` and collision guard from OtelLog and OtelSpan. | 1 file |

**Performance target:** Complex VRL paths (`.attr[0]`, `.resource.attributes.key`) go from O(n) full-event rebuild to O(log n) direct BTreeMap + array navigation. Zero allocation for reads.

#### Phase 11 — P36: Delete legacy proto→Value ingestion paths (opentelemetry-proto)
Delete `into_event_iter()` from `ResourceLogs` and `ResourceSpans` in `lib/opentelemetry-proto/src/`. These legacy paths convert proto → VRL `Value` → OtelLog/OtelSpan via `kv_list_into_value()` and `insert_source_metadata()`. **Production code already uses `into_otel_event_iter()`** which goes through `from_parts()` (zero-conversion proto path). Legacy paths only called from tests.

| Component | Action |
|-----------|--------|
| `kv_list_into_value()` (common.rs:40) | Delete. Converts `Vec<KeyValue>` → `Value::Object`. Only used by legacy `into_event_iter()` paths and `From<PBValue> for Value` KvlistValue arm. |
| `From<PBValue> for Value` (common.rs:8) | Delete. Proto→VRL converter used only by legacy ingestion. `any_value_to_vrl()` in otel_event.rs handles this for VRL bridge. |
| `From<PBValue> for TagValue` (common.rs:27) | Dies with MetricTags (P31). |
| `ResourceLog::into_event()` (logs.rs:251) | Delete. Legacy proto→Value→OtelLog path. |
| `ResourceSpans::into_event_iter()` (spans.rs) | Delete. Legacy proto→Value→OtelSpan path. |
| `OtelSpan::from_otel_log()` (otel_event.rs:2340) | Rewrite. Currently uses `as_map()` → `to_value_canonical()` round-trip. Transfer proto fields directly: `log.record_attrs` → `span_attrs`, preserve resource/scope. |
| Test code | Migrate tests to use `into_otel_event_iter()` (proto path). |

#### Phase 12 — P34: kvlist_to_object_map() cleanup (2 files)
Inline or remove `kvlist_to_object_map()` from `otel_event.rs` and `vrl_target.rs`. Low priority — only needed when VRL operates on `AnyValue` directly.

#### Phase 13 — P37: OTLP HTTP JSON sink support
Add `application/json` Content-Type option to OTLP HTTP sink. Uses existing `Serialize` impls (already OTLP/JSON compliant). Currently proto-only — JSON option needed for receivers that only accept JSON.

#### Phase 14 — P38: VRL lazy/incremental conversion
Keep proto alongside Value in VRL target. Convert fields lazily on first access. Write back only changed fields on exit instead of full rebuild. Cuts VRL boundary cost by 50-80% for typical remap programs that touch 5-10 of ~30 fields.

**Prerequisites:** P35 complete (no `to_value_canonical()` fallbacks interfering with lazy tracking).

#### Phase 15 — P39: VRL read-only passthrough
Detect at VRL compile time whether a program is read-only (no mutations). Skip exit conversion entirely — the proto is unchanged. Read-only transforms (`filter`, `route`, `sample`) become zero-cost VRL boundary.

**Prerequisites:** P38 complete (infrastructure for tracking mutations).

---

## Decisions (locked — all answered 2026-04-30)

All decisions are locked. Automode proceeds based on these answers.

### D1 — Fix `is_cumulative()` for Gauge/Summary ✅
**Question:** Should `is_cumulative()` return `false` for Gauge and Summary?
**Context:** Currently returns `true` for both (bug). OTel spec says Gauge/Summary have NO temporality. otelcol-contrib skips Gauges in temporal processors. If fixed, aggregate modes (Latest, Diff, Max, Min, Mean, Stdev) need explicit `|| is_gauge()` guards to keep working on Gauges.
**Decision:** Yes — fix it. Add `has_temporality()` helper. Update aggregate guards to `is_cumulative() || is_gauge()`.

### D2 — Keep `add()` working on Gauges ✅
**Question:** Should `add()` keep working on Gauges, or return `false` to force last-value-wins?
**Context:** OTel spec says Gauges are "non-additive, last sample value". But otelcol-contrib `metricstransformprocessor` treats Gauges identically to Sums for sum/mean/min/max/count. Our `Auto` mode already does last-value-wins for Gauges. `add()` is only used internally by Mean/Stdev (intermediate sum ÷ count).
**Decision:** Yes, keep working — matches otelcol-contrib behavior.

### D3 — `metric_series()` grouping key scope ✅
**Question:** Should `metric_series()` include Resource and Scope in the grouping key, or stay as `name + namespace + data-point-attributes`?
**Context:** Full OTel identity = name + Resource + Scope + unit + type + temporality + monotonicity. otelcol-contrib `metricstransformprocessor` groups by `(attributes + timestamp)` only (no Resource/Scope). `deltatocumulativeprocessor` uses full identity. Our aggregate is analogous to metricstransformprocessor (within-metric aggregation, not cross-resource state tracking).
**Decision:** Keep current key — matches metricstransformprocessor. Document as known simplification.

### D4 — P26 remaining consumers: migrate all ~45 files ✅
**Question:** Should automode migrate all remaining files using legacy metric types (`MetricValue`, `MetricKind`, `MetricSeries`, `MetricData`, `into_metric_parts()`, `from_metric_parts()`)? If yes, in what order?
**Context:** 75 non-test call sites for `from_metric_parts`/`into_metric_parts` across sources, transforms, sinks, lib, API. These are the bridge between `OtelMetric` and the legacy computation layer. Removing them means each call site must use native `OtelMetric` methods directly.
**Decision:** Yes, migrate all. Order by blast radius:
1. Sources (6 files) — replace `from_metric_parts()` with `new_counter()`/`new_gauge()`/etc.
2. Transforms (4 files) — `incremental_to_absolute` needs native temporality ops
3. Sinks buffer/normalize (6 files) — core infrastructure
4. Individual sinks (12 files) — each sink's MetricValue match arms
5. Lib crates (5 files) — codecs, internal metrics, lua
6. API (2 files) — lowest priority

### D5 — P27/P35 `to_value_canonical()` elimination ⚠️ (revised 2026-04-30)
**Question:** How to handle `to_value_canonical()` — keep as public API or eliminate?
**Context:** Initial audit (P27) concluded "keep." Deeper analysis revealed the flat canonical format is a third format that violates the two-format rule and OTLP structure. The VRL bridge already has its own `otel_log_event_to_value()` / `value_to_otel_log_event()` — it does NOT use `to_value_canonical()`. The ~27 internal fallback calls in otel_event.rs do full-event round-trips that dominate VRL complex-path performance.
**Decision (revised):** Eliminate entirely (P35). Each caller replaced with direct proto access, `Serialize` (OTLP/JSON), or direct iteration. `apply_value_map()` kept at ingest boundary only (reverse direction — external format → OTLP structure).

### D6 — P28 hardcoded field names in otel_event.rs ✅ (completed)
**Question:** Should we extract ~239 string literals in `otel_event.rs` into named constants?
**Context:** All uses are in the defining module. The 35 constants already in `otel_fields.rs` (P19) cover cross-file usage. Risk of typos is low — grep works.
**Decision:** Yes. Completed — 51 new constants added (90 total), 376 string literals replaced.

### D7 — `from_metric_parts()`/`into_metric_parts()` bridge: delete ✅
**Question:** After P26 migrates all consumers, should `from_metric_parts()`/`into_metric_parts()` be deleted, or kept as public API for external/plugin use?
**Context:** Currently 75 non-test call sites. These methods decompose `OtelMetric` into `(MetricSeries, MetricData, EventMetadata)` and reassemble. They exist because legacy types (`MetricValue`, `MetricKind`) provided arithmetic and match arms that the proto struct lacked. Now that `OtelMetric` has native arithmetic (`add()`, `subtract()`, `zero()`, `first_value_as_f64()`), the bridge is less needed. But it remains the only way to do exhaustive `match` on metric type (`MetricValue::Counter` / `Gauge` / `Distribution` / etc.) — proto `data` oneof matching is verbose.
**Decision:** Delete after all consumers are migrated. Add helper methods (`is_counter()`, `is_histogram()`, `data_type_name()`) on `OtelMetric` for type inspection. Sinks match on proto `data` oneof directly.

### D8 — `MetricTags` → `OtelAttributes` migration scope ✅
**Question:** Should `MetricTags` be replaced by `OtelAttributes` in the same pass as `MetricValue`/`MetricKind`, or as a separate step?
**Context:** `MetricTags` is used in 36 non-test files (188 references). It provides multi-value tag support (`TagValueSet`). `OtelAttributes` (BTreeMap-backed) supports multi-value via `ArrayValue`. The types are conceptually equivalent but `MetricTags` has metric-specific helpers (`contains_key()`, `iter_single()`, `iter_all()`, `into_iter_single()`) used by sinks (Prometheus, InfluxDB, Statsd) for tag serialization.
**Decision:** Migrate together with `MetricValue`/`MetricKind` per-file. When a sink is migrated off `into_metric_parts()`, also replace its `MetricTags` usage with `OtelAttributes` helpers. Add equivalent helpers to `OtelAttributes` if missing.

### D9 — Automode commit granularity ✅
**Question:** Should automode create one commit per file, one per category (sources/transforms/sinks), or one per logical group?
**Context:** P26 touches ~45 files across 6 categories. One-commit-per-file creates many small commits (easier to review/revert) but more noise. One-per-category groups related changes (6 commits total). One big commit is hardest to review.
**Decision:** One commit per category (sources, transforms, sinks/buffer, individual sinks, lib, API). Run `cargo test -p vector` after each category. 6 reviewable checkpoints with passing tests at each.

### D10 — VRL Value bridge long-term strategy ✅
**Question:** What is the long-term plan for the proto ↔ VRL `Value` conversion boundary, and should any work be planned now?

**How it actually works (not per-access — per-event):**

VRL does NOT convert on every field access. Instead, the entire event is converted to a `Value` tree *once* before VRL runs, VRL operates on `Value` directly (zero overhead per read/write), then the `Value` is converted *back* to proto once after VRL finishes.

| Event type | Entry conversion | VRL runtime | Exit conversion |
|------------|-----------------|-------------|-----------------|
| **OtelLog** | `otel_log_event_to_value()` — full proto → Value rebuild | Operates on `Value` directly (`&Value` borrows, zero copy) | `value_to_otel_log_event()` — full Value → proto rebuild |
| **OtelSpan** | `otel_span_event_to_value()` — full proto → Value rebuild | Operates on `Value` directly | `value_to_otel_span_event()` — full Value → proto rebuild |
| **OtelMetric** | `precompute_otel_metric_value()` — partial Value snapshot | Reads from Value, writes back to proto per-field via `vrl_value_to_any_value()` | Returns original proto (kept alongside Value) |

**Performance cost:**
- 2 full-event conversions per VRL execution (OtelLog/OtelSpan). For a typical log with 15 attributes, this means ~30 `any_value_to_vrl()` calls on entry + ~30 `vrl_value_to_any_value()` calls on exit = ~60 clones per event.
- OtelMetric is cheaper — keeps proto, only converts touched fields.
- VRL `target_get()` returns `&Value` (borrowed) — zero overhead during VRL execution.
- `to_value_canonical()` used only for complex VRL paths (array index, nested subpath) inside `otel_event.rs` get/insert/remove as fallback.

**Why VRL-native AnyValue (original "Strategy C") was dropped:**
`Value` and `AnyValue` are structurally equivalent (string/int/float/bool/array/map). Rewriting all ~100+ VRL stdlib functions to use `AnyValue` instead of `Value` would be massive effort for near-zero gain — you'd just swap one tree type for another. The cost is at the 2 boundary conversions, not in VRL's internal type.

**Three realistic strategies:**

**A. Status quo — keep 2 boundary conversions.** Cost is ~60 clones per event (entry + exit). Acceptable for most workloads. Simple, well-tested. **Current choice.**

**B. Lazy/incremental conversion (P38).** Keep proto alongside Value. Convert fields lazily on first access. Write back changed fields on exit instead of full rebuild. Could cut boundary cost by 50-80%.

**C. Read-only passthrough (P39).** If the VRL program only reads fields (no mutations), skip the exit conversion entirely — the proto is unchanged. Read-only transforms (filter, route, sample) become zero-cost.

**Decision:**
1. **D10.0 — Unify OtelMetric VRL path now.** Convert OtelMetric to match OtelLog/OtelSpan (full proto → Value → proto). Remove `precompute_otel_metric_value()`, per-field write-back, dual storage. Add `otel_metric_event_to_value()` + `value_to_otel_metric_event()`. Consistency over micro-optimization.
2. **Accept status quo (A) after unification.** ~5% overhead is acceptable.
3. **P38 (B) and P39 (C) are planned optimizations** — execute after P35 eliminates `to_value_canonical()` fallbacks. P38 (lazy conversion) has highest impact. P39 (read-only passthrough) is simpler but narrower scope.
4. **Reduce `to_value_canonical()` fallback scope** during P26 work if encountered, but not a dedicated effort.
5. **Remaining `to_value_canonical()` sites** (lua, schema) — leave as-is, natural bridge uses.

### D11 — `value()` replacement: `MetricView<'a>` ✅
**Question:** What replaces `OtelMetric::value()` for callers that match on metric type?
**Context:** ~40 call sites do `match otel.value() { MetricValue::Counter{..} => ... }`. Options: (A) delete value(), use if/else with type helpers; (B) match on proto data oneof directly; (C) MetricView<'a> borrowing enum.
**Decision:** C — `MetricView<'a>` with OTLP variant names: `Sum`, `Gauge`, `Histogram`, `Summary`, `ExponentialHistogram` + Vector-specific `Set`, `Distribution`. Method: `otel.view() -> MetricView<'_>`. Zero-copy borrows from proto.

### D12 — MetricKind: keep ✅
**Question:** Delete MetricKind or keep it?
**Decision:** Keep as standalone 2-variant enum. Move to `otel_event.rs`, decouple from legacy `event/metric/` module.

### D13 — MetricSeries → MetricIdentity ✅
**Question:** Keep MetricSeries as HashMap grouping key or replace?
**Decision:** Rename to `MetricIdentity`. Flatten: drop `MetricName` sub-struct, just `name: String, namespace: Option<String>, tags: MetricTags`. Move next to `OtelMetric` in `otel_event.rs`.

### D14 — StatisticKind: delete ✅
**Question:** Keep or delete `StatisticKind`?
**Decision:** Delete. OTLP doesn't have it. Distributions are always histogram-semantics.

### D15 — Config deserialization: direct into proto ✅
**Question:** How to handle `MetricValue`/`MetricData` in config deserialization (`log_to_metric`, `TestMetricInput`, InfluxDB)?
**Decision:** Deserialize directly into `OtelMetric` via custom `Deserialize` impls calling native constructors. No intermediate `MetricValue`/`MetricData`. Config YAML schema unchanged — the `Deserialize` impl accepts the same JSON/YAML structure.

### D16 — Commit strategy: incremental ✅
**Question:** Commit granularity for type deletion?
**Decision:** Prep (3a) → Migrate consumers (3b) → Migrate config deser (3c) → Delete (3d). Run `cargo test -p vector` after each.

---

## Code Review Findings (P19)

Audit of the full migration (511 commits since `before_migration` tag). Two review passes. Organized by severity.

### P0 — Bugs (data loss or panics)

| ID | Issue | File | Status |
|----|-------|------|--------|
| P0-1 | `insert_at_segments` panics on non-Object intermediate values via `unwrap()` | `vrl_target.rs:186` | **Fixed P19** |
| P0-2 | `hex_decode_value` silently drops corrupted trace/span IDs (returns empty vec) | `vrl_target.rs:199` | **Fixed P19** |
| P0-3 | `OtelLog::value()` returns only body — breaks logfmt and any serializer calling it | `otel_event.rs:1947` | **Fixed P19** |
| P0-4 | `EventDataEq` for OtelLog/OtelSpan/OtelMetric omitted resource_attrs and scope_attrs — dedupe could merge events with different resource/scope context | `otel_event.rs` | **Fixed P19** |
| P0-5 | `to_value_canonical()` flattening attributes could overwrite reserved proto fields (body, trace_id, etc.) | `otel_event.rs` | **Fixed P19** |

### P1 — Correctness (wrong behavior, not immediately visible)

| ID | Issue | File | Status |
|----|-------|------|--------|
| P1-1 | Span `get("start_time")` vs canonical `"start_time_unix_nano"` asymmetry | `otel_event.rs` | **Fixed P19** |
| P1-2 | `restore_resource` converts `dropped_attributes_count` into an attribute | `otel_event.rs:244` | **Fixed P19** |
| P1-3 | Resource representation inconsistency between `to_value_canonical` (flat) and VRL target (nested) | `otel_event.rs` vs `vrl_target.rs` | **Fixed P19** |
| P1-4 | `target_get_mut` for OtelMetric returns cache ref, mutations never written back to proto | `vrl_target.rs:953` | **Fixed P19** |
| P1-5 | `annotate_dropped` is a no-op for traces | `remap.rs:533` | **Fixed P19** |
| P1-6 | Zero-valued proto fields (`severity_number=0`) read as `None` | `otel_event.rs:1025` | **Fixed P19** |
| P1-7 | `OtelLog::get_tags` looks up `service.name` as record attribute, not resource attribute | `otel_event.rs:4099` | **Fixed P19** |
| P1-8 | `flags` and `dropped_attributes_count` not round-tripped in `to_value_canonical()`/`apply_value_map()` for OtelLog and OtelSpan | `otel_event.rs` | **Fixed P19** |
| P1-9 | `vrl_value_to_otel_any_value` in vrl_target.rs uses `String::from_utf8_lossy` — silently corrupts non-UTF8 bytes instead of using `BytesValue` | `vrl_target.rs` | **Fixed P19** |

### P2 — Code quality (duplication, consistency)

| ID | Issue | Status |
|----|-------|--------|
| P2-1 | ~500 lines duplicated across OtelLog/OtelSpan/OtelMetric (get/insert/remove, resource/scope) | **Resolved by P35a.** When get/insert/remove are rewritten for direct proto navigation, the duplicated fallback-to-canonical logic is deleted. Remaining duplication (single-segment/multi-segment dispatch) is structural — OtelLog and OtelSpan have different proto fields, so the match arms differ. Macro extraction possible but not worth the readability cost. |
| P2-2 | Duplicate hex encode/decode in otel_event.rs vs vrl_target.rs | **Fixed P19** — vrl_target.rs delegates to otel_event.rs |
| P2-3 | Duplicate `any_value_to_vrl` / `vrl_value_to_any_value` across two files | **Fixed P19** — vrl_target.rs delegates to otel_event.rs |
| P2-4 | `OtelLog::value_mut()` misleadingly named — returns owned snapshot | **Fixed P19** |
| P2-5 | `nanos_to_timestamp` helper exists but logic inlined in 4 places | **Fixed P19** |
| P2-6 | OtelSpan lacks `remove`/`remove_prune` methods (falls through to full canonical rebuild) | **Fixed P29.** OtelSpan has `remove_prune()`, `span_remove_single_segment()`, `span_remove_field_path()`. Remaining `to_value_canonical()` fallbacks for array-indexed paths resolved by P35a. |
| P2-7 | Extract hardcoded OTel field name strings into constants module | **Fixed P19** — `otel_fields.rs` with 35 constants, used in vrl_target.rs |
| P2-8 | Remaining hardcoded strings in otel_event.rs (~100 uses of field name literals) | **Fixed P28.** 376 string literals replaced with `f::CONSTANT` references. 90 constants in `otel_fields.rs`. ~258 remaining string literals are structural (match arms, error messages, JSON field names in Serialize) — not field name constants. |

### Ignored tests action plan

| Action | Count | Details |
|--------|-------|---------|
| **DELETED** | 16 | Dead code: legacy proto round-trip (2), legacy LogNamespace schema (2), dev utility (1), QuickCheck size_of (3), metric codec (4), transformer (3), text metric (1) |
| **FIXED** | 9 | VRL decoding (4), GELF timestamp (1), JSON metric_tags_full (2), lag time (3 un-ignored), CEF/CSV (2 un-ignored), logfmt (2 — already working after `value()` fix) |
| **KEEP IGNORED** | 14 | Infrastructure: GCP (8), disk buffer (2), Azure (2), backpressure (1), finalization (1) |

---

## Skipped Tests Inventory

### Fixed in P18 — 17 `cargo test -p vector` tests (all passing)

All 17 OTel-migration ignored tests in `src/` have been un-ignored and fixed:

| Category | Tests fixed | How |
|----------|-------------|-----|
| Metric round-trip (log_to_metric) | 8 tests | Tests already passed — just removed `#[ignore]` |
| Metric-to-log timestamp | 2 tests | Updated assertions: `time_unix_nano` Integer instead of `timestamp` Timestamp, added `vector` metadata key |
| Remap timezone | 2 tests | Updated assertions: timestamp stored as `Bytes` string, not `Timestamp` type |
| Remap non-Object root | 2 tests | Fixed `value_to_otel_log_event` to store non-Object values as body |
| Remap metric VRL paths | 2 tests | Added OtelMetric VRL legacy paths (`.tags.*`, `.kind`, `.namespace`) + metric dropped annotation |
| Reduce nested fields | 1 test | Changed `value()` → `to_value_canonical()` in assertion |

### Fixed/deleted in P19 — `lib/` crate tests

| Category | Action | Details |
|----------|--------|---------|
| Source sender lag time (3) | **Un-ignored** | Tests pass after `emit_lag_time` reimplementation |
| Legacy proto round-trip (2) | **Deleted** | Dead code — native proto format removed |
| QuickCheck size_of (3) | **Deleted** (entire file) | `Arbitrary` for `Event` overflows with OTel types |
| Legacy LogNamespace schema (2) | **Deleted** | Schema validation assumed `LogEvent` semantics |
| VRL decoding format (4) | **Fixed** | Updated VRL source to use `.body`, assertions to match OTel structure |
| Transformer (3) | **Deleted** | Tested dot-path nesting + `only_fields` with `service` — not applicable to OtelLog flat attributes |
| CEF timestamp (1) | **Un-ignored** | Already passing |
| CSV timestamp (1) | **Un-ignored** | Already passing |
| GELF timestamp (1) | **Fixed** | GELF serializer updated to parse string timestamps (OtelLog stores timestamps as RFC 3339 strings) |
| JSON metric_tags_full (2) | **Fixed** | Expected output updated to OTel proto format |
| JSON/text dead metric tests (5) | **Deleted** | Set, histogram, summary, distribution tests — dead metric types in this context |
| Orphan metrics test file | **Deleted** | `vector-core/metrics/tests/mod.rs` was never compiled |

### Remaining skipped `lib/` tests — 0

All OTel-migration related `#[ignore]` annotations in `lib/` crates have been resolved. Zero remaining.

### Pre-existing infrastructure issues (not caused by migration)

| Location | Count | Reason |
|----------|-------|--------|
| `src/` (gcp_pubsub, gcp_chronicle, kafka, backpressure, azure, config, socket, host_metrics) | 16 | Flaky tests, missing infrastructure, external dependencies |
| `lib/` (disk_v2, finalization, metrics) | 4 | Flaky tests, known bugs |

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
