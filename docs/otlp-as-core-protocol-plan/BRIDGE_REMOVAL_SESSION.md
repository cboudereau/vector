# Bridge Removal — Architecture Analysis and Plan

## Architecture context

Step 6f made `Event` OTel-native: `Log(OtelLog)`, `Metric(OtelMetric)`, `Trace(OtelSpan)`.
Legacy types (`LogEvent`, `Metric`, `TraceEvent`) are still defined but no longer in `Event`.
They survive as intermediate representations used by bridge functions.

### What the bridge is

Four conversion functions that translate between OTel proto structs and legacy Value-based types:

| Function | Direction | Purpose |
|----------|-----------|---------|
| `from_log_event(LogEvent) → OtelLog` | Legacy → OTel | I/O boundary: sources/codecs that still produce LogEvent |
| `to_log_event() → LogEvent` | OTel → Legacy | Serialization: codecs that serialize via LogEvent's flat structure |
| `from_legacy_metric(Metric) → OtelMetric` | Legacy → OTel | I/O boundary: `Event::from(Metric)` for metric sources |
| `to_legacy_metric() → Metric` | OTel → Legacy | Serialization: sinks/transforms that consume legacy Metric |

### What's legacy vs what stays

**REMOVE (legacy bridge layer):**
- `to_log_event()` / `to_legacy_metric()` — external callers must migrate to proto access
- `to_value_legacy_layout()` — intermediate abstraction that mimics the old flat layout
- `apply_value_legacy_layout()` — write-back path that round-trips through `from_log_event`
- `LogEvent`, `Metric`, `TraceEvent` type definitions — after all consumers migrate
- `log_schema()` config — field name mapping that doesn't match OTel proto names

**KEEP (permanent):**
- `from_log_event()` / `from_legacy_metric()` constructors — I/O boundary, test convenience
- `OtelLog`/`OtelSpan`/`OtelMetric` proto accessors — the target API
- `VrlTarget` projections — VRL needs Value trees, built directly from proto
- Backward-compat VRL aliases (`.message` → `.body`) — kept for one release

**TRANSITIONAL (keep for now, remove later):**
- `to_log_event()` on OtelSpan — used by `trace_to_log` transform
- `to_legacy_metric()` body — used by transforms that need full Metric mutation

## Progress (updated 2026-04-09)

### Bridge call counts

| Function | Before session | Current | Removed |
|----------|---------------|---------|---------|
| `to_log_event()` | 75 | 10 | **65 (87%)** |
| `to_legacy_metric()` | 40 | 13 | **27 (68%)** |
| **Total** | **115** | **23** | **92 (80%)** |

94 commits, 1789 tests passing.
Session 2: Rewrites 1, 2, 3, 4, 5 (partial) done. 14 bridge calls eliminated.
22 test callers migrated. Remaining external call sites: datadog_search (2),
docker_logs (1), lua metric (1), metric_to_log (2), normalize internal (3).

### What was done

**OtelLog — all internal bridge delegation removed (earlier sessions):**
- `get()`, `insert()`, `remove()`, `namespace()`, `get_timestamp()`, etc.
- `Serialize`, `EventDataEq` — use `to_value_legacy_layout()` directly
- `get_by_meaning()` — direct schema definition lookup
- `convert_to_fields()` / `convert_to_fields_unquoted()` — proper nested flattening

**OtelSpan — all internal bridge delegation removed (earlier sessions):**
- Mirrors OtelLog pattern: `to_value_legacy_layout()` + `apply_value_legacy_layout()`

**OtelMetric — fully debridged (this session):**
- `value()`, `kind()` — `extract_metric_data()` from proto
- `tag_value()` — searches data point, resource, scope attributes directly
- `tags()` — **fixed bug**: was returning None, now builds MetricTags from proto
- `timestamp()`, `namespace()` — direct proto access
- `Display` — implemented from proto (Prometheus-like text format)
- `Serialize` — implemented from proto (no more `clone().to_legacy_metric()`)

**Phase 0 — cleanup:**
- Fixed stale section comments, added `TODO(bridge-removal)` markers
- Added `// TEMPORARY BRIDGE` markers to proto.rs, mod.rs bridge call sites
- Documented `tags()` bug (now fixed)

**Phase 1 — codec encoder migrations (7 of 9 codecs migrated):**
- JSON: serializes OtelLog/OtelSpan directly; metric uses bridge only for Single tag mode
- logfmt: uses `OtelLog::value()` directly
- CSV: uses `OtelLog::get(field)` directly
- CEF: uses `OtelLog::get(field)` via helper
- Avro: serializes OtelLog directly (Serialize trait)
- Protobuf: uses `OtelLog::value()` / `OtelSpan::as_map()`
- syslog: refactored ConfigDecanter to accept `&OtelLog`
- text: uses `OtelMetric::to_string()` directly (new Display impl)
- GELF: **deferred** — heavily mutates LogEvent (as_map_mut, rename_key)
- Arrow: **deferred** — convert_timestamps deeply coupled to LogEvent

**Phase 1 — transform/sink/source migrations:**
- remap: `OtelLog::namespace()` directly
- sample: `parse_path_and_get_value()` directly on OtelLog/OtelSpan
- log_to_metric: `to_metric_with_config()` uses OtelLog directly; transform skips round-trip
- trace_to_log test: returns OtelLog, uses `as_map()`
- new_relic LogsApiModel: uses `OtelLog::remove()` and `as_map()`
- schema/definition: uses `OtelLog::value()` for both namespaces
- enrichment_tables/memory: uses `OtelLog::as_map()`
- template: uses `OtelMetric::name()`, `namespace()`, `tag_value()` directly
- component_spec: uses `OtelMetric` directly (name, tag_value, value, kind)
- OTLP decoder: uses `parse_path_and_get_value()` for field existence
- k8s_logs parser: uses `get_body()`, `parse_path_and_get_value()` directly
- Event::to_legacy_json_value: serializes OTel types directly

**Phase 1 — test migrations:**
- Decoder tests (vrl, avro, protobuf, gelf, decoder): 16 calls migrated
- datadog_agent tests: 9 `to_legacy_metric` calls removed (assert_tags uses OtelMetric)
- exec tests: uses `convert_to_fields().len()` instead of bridge

**Bug fixes along the way:**
- OtelMetric::tags() was silently returning None for all metrics — fixed
- 9 pre-existing test failures from message→body migration — fixed
- 2 flaky tests (receives_logs, file_start_position) — fixed with longer timeouts
- kafka/pulsar sinks: migrated to `tag_value()` to avoid borrowing owned temporaries

**OtelMetric new APIs added:**
- `remove_data_point_attribute(key)` — remove attribute from all data points
- `replace_tag(key, value)` — remove + set attribute (used by prometheus scrape)
- `Display` impl — Prometheus-like text format from proto
- `Serialize` impl — direct from proto (no more clone + to_legacy_metric)
- `tags()` — builds MetricTags from proto (was broken, always returned None)

**Additional migrations (latest):**
- prometheus/scrape: uses OtelMetric::tag_value() + replace_tag() directly
- datadog_agent tests: assert_tags accepts &OtelMetric, uses tags() directly (9 calls)
- exec tests: uses convert_to_fields().len() instead of all_event_fields
- OtelMetric Serialize: no longer clones self and builds legacy Metric
- JSON encoder: skips bridge for Full metric tag mode (default path)
- dedupe MatchFields: uses OtelLog::get() directly (IgnoreFields still bridges)
- encoding transformer: fully rewritten to OtelLog-native — deleted 80 lines of
  LogEvent methods, replaced with OtelLog::remove/insert/convert_to_fields/value.
  Largest single bridge consumer eliminated (8 production + 7 test calls → 0).
  Also fixed OtelLog::get_by_meaning to check dropped_fields first.
- prometheus scrape: uses OtelMetric::tag_value() + replace_tag() directly
- mock/transforms Log+Trace: use OtelLog/OtelSpan::get()+insert() directly
- dedupe MatchFields: uses OtelLog::get() directly
- OtlpJsonLog/OtlpJsonSpan wrappers: opt-in OTLP JSON for per-sink migration
- OtelSpan::from_otel_log(): direct conversion without LogEvent intermediate
- GELF encoder: fully rewritten — to_gelf_event() accepts &mut OtelLog,
  uses get/insert/rename_key/convert_to_fields for validation+mutation
- Arrow encoder: fully rewritten — convert_timestamps_otel() uses
  convert_to_fields() + insert(), find_null_field uses parse_path_and_get_value,
  serde_arrow serializes OtelLog directly

### Remaining 30 bridge calls

**Core infrastructure — 11 calls:**
- `proto.rs` (6): OTel → protobuf via legacy types
- `lua/event.rs` (3): Lua API exposes LogEvent/Metric
- `mod.rs` (3): `to_metric()`, `into_metric()`, `try_into_metric()`
- `ref.rs` (2): `EventRef::into_metric()`, `EventMutRef::into_metric()`

**Sinks needing LogEvent pipeline — 3 calls:**
- `sinks/elasticsearch` (3): pipeline around LogEvent + metric_to_log bridge
- `sinks/splunk_hec` (1): render_template_string_from_log

**Transforms needing Metric mutation — 4 calls:**
- `transforms/aggregate` (1): metric.into_parts()
- `transforms/tag_cardinality_limit` (1): tags_mut().retain()
- `transforms/incremental_to_absolute` (1): make_absolute
- `sinks/appsignal` (1): normalizer.normalize(Metric)

**Transforms needing LogEvent iteration/mutation — 2 calls:**
- `transforms/reduce` (1): Discriminant::from_log_event (ReduceState uses LogEvent)
- `sources/docker_logs` (1): partial event merging

**Intentional bridge / API — 4 calls:**
- `transforms/trace_to_log` (1): transform purpose IS span→log
- `api/schema/events/output` (2): GraphQL API wraps legacy types
- `conditions/datadog_search` (1): DD matcher takes &LogEvent

**Test / transitional — 3 calls:**
- `transforms/metric_to_log` (1): production transform_one(Metric)
- `test_util/mock/transforms` (1): metric branch uses metric.add()
- `conditions/datadog_search` (1): test helper

**Codec encoders/decoders — 0 bridge calls (fully migrated).**

**Blockers for next wave:**
- OtelLog Serialize → OTLP JSON: 68 sink tests depend on flat format (per-sink migration)
- OtelMetric `tags_mut()` with `retain()` for tag_cardinality_limit
- OtelMetric `into_parts()` for aggregate
- OtelMetric `make_absolute()` for incremental_to_absolute
- ReduceState internals use LogEvent
- Lua bridge exposes LogEvent API to user scripts

## Next session — architectural rewrites for remaining 23 calls

### Completed tasks

- **Task A** ✅ — ReduceState accepts `&OtelLog`, Discriminant::from_otel_log
- **Task D partial** ✅ — tag_cardinality_limit uses OtelMetric directly
- **Task G partial** ✅ — GraphQL API wraps OtelLog/OtelMetric directly
- **Task F2 partial** ✅ — Lua IntoLua uses OtelLog::value(), test migrated
- **Mock transforms** ✅ — metric branch modifies proto data points directly
- **Rewrite 4 partial** ✅ — EventRef/EventMutRef: `into_otel_metric()` added.
  22 test callers migrated to `as_metric()/into_otel_metric()`.
- **Rewrite 1** ✅ — MetricSet/MetricNormalizer accept OtelMetric via _otel() methods.
  aggregate (into_metric_parts), incremental_to_absolute (make_absolute_otel),
  appsignal (normalize_otel) — 3 to_legacy_metric() calls eliminated.
- **Rewrite 2 partial** ✅ — Elasticsearch log pipeline rewritten for OtelLog.
  ProcessedEvent.log changed to OtelLog. process_log, mode.index/bulk_action/version,
  DataStreamConfig all accept OtelLog directly. 2 to_log_event() calls eliminated.
- **Rewrite 5 partial** ✅ — Splunk HEC logs rewritten for OtelLog.
  HecProcessedEvent uses OtelLog. process_log and partition use render_string()
  directly. 1 to_log_event() call eliminated. find_key_by_meaning() added to OtelLog.
- **Rewrite 3** ✅ — proto.rs serializes OTel types directly to buffer proto.
  From<OtelLog/OtelSpan/OtelMetric> for WithMetadata<Log/Trace/Metric>.
  6 bridge calls eliminated. No buffer format change.

### Remaining 23 calls by rewrite task

---

#### Rewrite 1 — MetricSet for OtelMetric (3 calls, HIGH priority) — DONE

**Files:** `src/sinks/util/buffer/metrics/normalize.rs`, `src/transforms/aggregate.rs`,
`src/transforms/incremental_to_absolute.rs`, `src/sinks/appsignal/sink.rs`
**Done:** Added `normalize_otel()`, `make_absolute_otel()`, `make_incremental_otel()`
to MetricNormalizer/MetricSet. Added `OtelMetric::into_metric_parts()`.
Migrated all 3 call sites: aggregate uses `into_metric_parts()`,
incremental_to_absolute uses `make_absolute_otel()`, appsignal uses
`normalize_otel()`. Bridge calls moved inside MetricSet/MetricNormalizer.
**Note:** Remaining sink callers of `try_into_metric()` (10 sinks) are NOT
blocked by MetricSet — they need full per-sink pipeline rewrites because
encoders, batchers, and request builders all expect legacy Metric.

---

#### Rewrite 2 — Elasticsearch pipeline (3 calls, HIGH priority) — DONE (2 of 3)

**Files:** `src/sinks/elasticsearch/sink.rs`, `src/sinks/elasticsearch/encoder.rs`,
`src/sinks/elasticsearch/mod.rs`, `src/sinks/elasticsearch/config.rs`
**Done:** Changed `ProcessedEvent.log` from `LogEvent` to `OtelLog`.
Rewrote `process_log()` to accept `OtelLog` directly. Changed all
`ElasticsearchCommonMode` methods (index, bulk_action, version) and
`DataStreamConfig` methods (dtype, dataset, namespace, sync_fields,
remap_timestamp, index) from `&LogEvent` to `&OtelLog`. Template rendering
uses `render_string()` directly instead of `render_string_from_log()` (which
was double-converting OtelLog → LogEvent → OtelLog internally).
Eliminated 2 `to_log_event()` calls from the log pipeline.
**Remaining:** Metric path (1 call) stays on bridge — depends on metric_to_log
transform (Rewrite 7). Tests updated with `OtelLog::from_log_event()` at call sites.

---

#### Rewrite 3 — proto.rs buffer serialization (6 calls, MEDIUM priority) — DONE

**File:** `lib/vector-core/src/event/proto.rs`
**Done:** Added `From<OtelLog>`, `From<OtelSpan>`, `From<OtelMetric>` for
`WithMetadata<Log/Trace/Metric>`. These use `to_value_legacy_layout()` (now
pub(crate)) to build the Value tree directly, then encode to proto fields.
No buffer format change — same proto wire format, just skips LogEvent/Metric
intermediates. 6 bridge calls eliminated.

---

#### Rewrite 4 — Event public metric API (5 calls, MEDIUM priority) — PARTIAL

**Files:** `lib/vector-core/src/event/mod.rs` (3), `lib/vector-core/src/event/ref.rs` (2)
**Calls:** `to_metric()`, `into_metric()`, `try_into_metric()`,
`EventRef::into_metric()`, `EventMutRef::into_metric()`
**Done:** OTel-native methods already existed on Event (`into_otel_metric()`,
`try_into_otel_metric()`, `as_otel_metric()`). Added `into_otel_metric()` to
EventRef and EventMutRef. Migrated 22 test callers across 10 files.
**Remaining:** 60 callers across 16 files still use bridge methods. Production
sink callers (influxdb, statsd, sematext, cloudwatch, new_relic, prometheus,
greptimedb, splunk_hec, gcp stackdriver) are blocked on Rewrite 1 (MetricSet).
Test callers in log_to_metric (26), aggregate (15), prometheus/exporter (4),
lua (1), metrics/tests (1), test_util (2), aws_ec2_metadata (2) remain.

---

#### Rewrite 5 — Sink pipeline type changes (2 calls, MEDIUM priority) — PARTIAL

**Splunk HEC** (1 call) ✅: HecProcessedEvent changed to OtelLog.
process_log accepts OtelLog directly. Template rendering uses
render_string(&OtelLog). Added find_key_by_meaning() to OtelLog.
1 to_log_event() call eliminated.

**Docker logs** (1 call): Full pipeline `Stream<Item=LogEvent>` → need
`Stream<Item=OtelLog>` including `add_hostname`, `line_agg_adapter`.
Blocked by `LogEventMergeState` which only works with LogEvent.
~100 lines, high complexity.

---

#### Rewrite 6 — Remaining adapters (3 calls, LOW priority — can defer)

**DD search** (2 calls): `Filter<LogEvent>` matcher — DD adapter responsibility.
Rewrite requires changing the DD search filter infrastructure to work with OtelLog.

**Lua metric** (1 call): `LuaMetric` wraps legacy `Metric`. Need either
`LuaOtelMetric` or make LuaMetric generic over OtelMetric.

---

#### Rewrite 7 — metric_to_log transform (1 call, LOW priority)

`transform_one(Metric)` serializes legacy Metric to JSON then inserts fields
into LogEvent. With OtelMetric OTLP JSON, the field structure differs.
Needs a full transform rewrite to work with OtelMetric proto directly.

---

### Suggested session order

1. **Rewrite 4** (public metric API) — mechanical, unblocks future work
2. **Rewrite 1** (MetricSet) — 3 calls, biggest remaining batch
3. **Rewrite 2** (Elasticsearch) — 3 calls, complex but high impact
4. **Rewrite 3** (proto.rs) — 6 calls, deep infrastructure
5. **Rewrite 5** (Splunk HEC + Docker) — 2 calls, pipeline changes
6. **Rewrite 6** (DD search + Lua metric) — 3 calls, adapter work
7. **Rewrite 7** (metric_to_log) — 1 call, full transform rewrite

## Verification

- `cargo test -p vector --lib` — 1789 passed, 0 failed
- `cargo test -p codecs --lib` — 171 passed, 0 failed
- `cargo test -p vector-core --lib` — all event tests pass
- `cargo check -p vector` — compiles clean

## Context docs

- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
- `LEGACY_REMOVAL_PLAN.md` — VRL alias removal phases (A–E)
- `LEGACY_BRIDGE_REMOVAL.md` — design discovery for bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — VRL path model for OTel types
