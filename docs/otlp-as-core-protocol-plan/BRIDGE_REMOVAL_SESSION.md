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

## Progress (updated 2026-04-08)

### Bridge call counts

| Function | Before session | After session | Removed |
|----------|---------------|---------------|---------|
| `to_log_event()` | 75 | 13 | **62 (83%)** |
| `to_legacy_metric()` | 40 | 15 | **25 (63%)** |
| **Total** | **115** | **28** | **87 (76%)** |

73 commits, 1789 tests passing.
OtelLog: all_event_fields/all_metadata_fields added.
Dedupe + metric_to_log test fully debridged.
OtelMetric Serialize → OTLP JSON. OtlpJsonLog/OtlpJsonSpan wrappers ready.
OtelLog Serialize stays legacy — sinks use field paths in serialized JSON at runtime.

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

## Next session — architectural rewrites for remaining 30 calls

Each task below is a self-contained unit of work that removes bridge calls.
Ordered by impact (most calls removed first) and dependency.

---

### Task A — Rewrite ReduceState to accept OtelLog ✅ DONE

**Status:** `add_event(&OtelLog)`, `push_or_new_reduce_state(&OtelLog)`,
`Discriminant::from_otel_log`. `flush()` still returns LogEvent (OK).
Added `all_event_fields_skip_array_elements()` to OtelLog.

---

### Task B — Rewrite Elasticsearch sink pipeline (3 `to_log_event` + 1 `to_legacy_metric`)

**Files:** `src/sinks/elasticsearch/sink.rs`, `src/sinks/elasticsearch/encoder.rs`
**Current:** OtelLog → LogEvent → `process_log(LogEvent)` → `ProcessedEvent{log: LogEvent}`
→ encoder serializes LogEvent.
**Problem:** `process_log` uses `mode.index(&log)`, `mode.bulk_action(&log)`,
`cfg.sync_fields(&mut log)`, `log.remove(key)` for `_id` extraction. Encoder
renames `timestamp` → `@timestamp` in serialized JSON.
**Plan:**
1. Change `ProcessedEvent.log` from `LogEvent` to `OtelLog` (like Kinesis migration)
2. Rewrite `process_log` to accept `OtelLog` — use `OtelLog::get/remove/insert`
3. Encoder: extract timestamp from OtelLog proto before serialization, inject
   `@timestamp` into output (don't rely on field name in serialized JSON)
4. Update `mode.index()`, `mode.bulk_action()` to accept `&OtelLog`
5. Metric path: keep `metric_to_log.transform_one(metric.to_legacy_metric())`
   for now — that's a separate transform rewrite
**Estimate:** ~150 lines changed. High complexity — many call sites in mode/config.

---

### Task C — Rewrite Splunk HEC logs sink (1 `to_log_event`)

**File:** `src/sinks/splunk_hec/logs/sink.rs`
**Current:** `event.into_log_coerce().to_log_event()` → `process_log(LogEvent)`
**Problem:** Uses `render_template_string_from_log(template, &log, field)` which
calls `template.render_string_from_log(&log)` — takes `&LogEvent`.
**Plan:**
1. Add `Template::render_string_from_otel_log(&OtelLog)` (or make Template generic)
2. Rewrite `process_log` to accept `OtelLog`
3. Extract timestamp, host, sourcetype from OtelLog proto before serialization
**Estimate:** ~80 lines changed. Medium complexity — Template API change.

---

### Task D — Rewrite metric transforms (3 remaining of 4 `to_legacy_metric`)

**Files:** `src/transforms/aggregate.rs`, `src/transforms/tag_cardinality_limit/mod.rs`,
`src/transforms/incremental_to_absolute.rs`, `src/sinks/appsignal/sink.rs`
**Current:** All convert `OtelMetric → Metric` for mutation/decomposition.
**Problem:**
- aggregate: `metric.into_parts()` → merge `MetricData.value.add()` → reconstruct
- tag_cardinality: `tags_mut().retain()` — mutable tag iteration with filter
- incremental_to_absolute: `MetricSet.make_absolute(Metric)` — cache + merge
- appsignal: `normalizer.normalize(Metric)` — MetricSet normalization
**Plan:**
1. Add `OtelMetric::merge_value(&mut self, other: &OtelMetric)` for aggregate
2. ~~Add `OtelMetric::retain_tags(...)` for tag_cardinality~~ ✅ DONE — uses tags() + remove_data_point_attribute()
3. For incremental_to_absolute: implement `MetricSet` equivalent on OtelMetric proto
   (cache by metric name+attributes hash, accumulate values)
4. For appsignal: same as incremental_to_absolute (uses `MetricSet::normalize`)
**Estimate:** ~300 lines. High complexity — core metric merging logic.

---

### Task E — Rewrite Docker logs partial merge (1 `to_log_event`)

**File:** `src/sources/docker_logs/mod.rs`
**Current:** `otel_log.to_log_event()` → `LogEventMergeState` for partial line merging
**Problem:** The entire event pipeline is typed on `LogEvent`:
`new_event() → Stream<Item=LogEvent> → line_agg_adapter → add_hostname → send_event_stream`.
Changing just the merge state isn't enough — `add_hostname`, `line_agg_adapter`,
and `log_namespace.insert_source_metadata` all take `&mut LogEvent`.
**API ready:** `OtelLog::merge_body()` added (concatenates string bodies + metadata).
**Actual plan:**
1. Change `new_event()` return type from `Option<LogEvent>` to `Option<OtelLog>`
2. Change `add_hostname` to accept `OtelLog` — use `otel_log.set_resource_attribute`
3. Change `line_agg_adapter` to work with `OtelLog` stream
4. Change `partial_event_merge_state` from `Option<LogEventMergeState>` to `Option<OtelLog>`
5. Update stream type from `Stream<Item=LogEvent>` to `Stream<Item=OtelLog>`
**Estimate:** ~100 lines. High complexity — full pipeline type change.

---

### Task F — Core infrastructure (11 calls)

#### F1 — proto.rs buffer serialization (6 calls)
**File:** `lib/vector-core/src/event/proto.rs`
**Current:** OTel types → legacy types → Vector protobuf for disk buffers
**Plan:** Serialize OTel types directly to OTLP protobuf for disk buffers.
Requires new proto message types or encoding OTel proto bytes directly.
**Estimate:** ~200 lines. High complexity — buffer format change.

#### F2 — Lua bridge (2 calls)
**File:** `lib/vector-core/src/event/lua/event.rs`
**Current:** OtelLog → LogEvent for Lua field access
**Plan:** Expose OtelLog fields to Lua directly via `get/insert` accessors.
**Estimate:** ~60 lines. Medium complexity.

#### F3 — Event::to_metric/into_metric/try_into_metric (3 calls)
**File:** `lib/vector-core/src/event/mod.rs`
**Plan:** These are public API that returns legacy `Metric`. Either:
- Change return type to `OtelMetric` and update all callers (~20 files)
- Or deprecate and add `as_otel_metric()` / `into_otel_metric()`
**Estimate:** ~20 lines in mod.rs, but ~100 lines across callers.

#### F4 — EventRef/EventMutRef::into_metric (2 calls)
**File:** `lib/vector-core/src/event/ref.rs`
**Plan:** Same as F3 — change return type or add new methods.

---

### Task G — Intentional bridge / adapter calls (4 calls)

These are intentional and may stay permanently:
- `api/schema/events/output.rs` (2): GraphQL API wraps legacy types → migrate
  when GraphQL schema changes to OTel-native
- `conditions/datadog_search.rs` (1 production + 1 test): DD search `Filter<LogEvent>`
  → rewrite DD search matcher for `OtelLog` (DD adapter responsibility)

---

### Task H — OtelLog Serialize → OTLP JSON (prerequisite: Tasks B, C, E)

**After sinks decouple from serialized JSON field paths:**
1. Change `Serialize for OtelLog` to use `OtlpJsonLog` (already implemented)
2. Change `Serialize for OtelSpan` to use `OtlpJsonSpan` (already implemented)
3. Update ~68 sink tests to expect OTLP JSON format
4. Delete `to_value_legacy_layout()` from OtelLog/OtelSpan

**Critical insight discovered this session:** Sinks use field paths in
*serialized* JSON at runtime (websocket ack, ES _id, Splunk HEC timestamp).
Changing Serialize breaks runtime behavior, not just tests. Sinks must extract
fields from OtelLog proto BEFORE serialization.

---

### Suggested session order

1. **Task A** (reduce) — quick win, 1 call
2. **Task E** (docker partial merge) — quick win, 1 call  
3. **Task D** (metric transforms) — 4 calls, biggest batch
4. **Task B** (elasticsearch) — 4 calls, complex but high impact
5. **Task C** (splunk hec) — 1 call
6. **Task F** (core infra) — 11 calls, deep changes
7. **Task G** (DD search) — adapter work, can defer
8. **Task H** (OtelLog OTLP JSON) — final step, depends on B+C+E

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
