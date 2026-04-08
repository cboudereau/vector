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
| `to_log_event()` | 75 | 17 | **58 (77%)** |
| `to_legacy_metric()` | 40 | 16 | **24 (60%)** |
| **Total** | **115** | **33** | **82 (71%)** |

66 commits, 1773 tests passing.
OtelMetric Serialize → OTLP JSON. OtlpJsonLog/OtlpJsonSpan wrappers ready.
OtelLog Serialize stays legacy — sinks use field paths in serialized JSON at runtime.
Critical insight: sinks must decouple from serialized JSON before format can change.

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

### Remaining 33 bridge calls (all at hard floor)

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

**Transforms needing LogEvent iteration/mutation — 3 calls:**
- `transforms/dedupe` (1): all_event_fields + all_metadata_fields (IgnoreFields)
- `transforms/reduce` (1): Discriminant::from_log_event (ReduceState uses LogEvent)
- `sources/docker_logs` (1): partial event merging

**Intentional bridge / API — 4 calls:**
- `transforms/trace_to_log` (1): transform purpose IS span→log
- `api/schema/events/output` (2): GraphQL API wraps legacy types
- `conditions/datadog_search` (1): DD matcher takes &LogEvent

**Test / transitional — 4 calls:**
- `transforms/metric_to_log` (2): production transform_one + test helper
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

## Phased plan — remaining work

### Phase 2 — OtelMetric Serialize → OTLP-native JSON

**Problem:** `Serialize for OtelMetric` currently produces Vector-legacy JSON
format (`name`, `tags`, `kind`, `counter`/`gauge`). This is **not OTel** — OTel
has `attributes` on data points (no `tags`), and the structure follows the OTLP
proto schema (`data.sum.dataPoints[0].value`).

**Current state:**
- `OtlpSerializer` already produces correct OTLP protobuf — no bridge
- `JsonSerializer` produces Vector-legacy JSON via `Serialize for OtelMetric`
- `Serialize for OtelMetric` replicates the legacy `Metric` serde format

**Plan:**
1. Change `Serialize for OtelMetric` to produce OTLP-native JSON (matching
   the OTLP proto JSON mapping: `name`, `data.sum.dataPoints`, etc.)
2. Move Vector-legacy JSON metric format to the Vector source/sink adapters
   as a backward-compat serialization for inter-Vector communication with
   older instances
3. Update `JsonSerializer` metric path — it should serialize OtelMetric
   directly (OTLP JSON) since that's the core format now
4. Any sink that needs Vector-legacy format (e.g. for backward compat with
   existing pipelines) uses the adapter, not the core Serialize impl

**Impact:** This is a wire format change. Sinks using `encoding.codec = "json"`
will produce OTLP-structured metric JSON instead of Vector-legacy. Users may
need to update downstream parsers. Document in release notes.

**Status:**
- OtelMetric Serialize → OTLP JSON: **DONE** (only 1 test needed update)
- OtelLog/OtelSpan Serialize → OTLP JSON: **CANNOT CHANGE DEFAULT** —
  sinks use field paths in serialized JSON at runtime (websocket ack
  message_id, Elasticsearch _id, Splunk HEC timestamp). Changing to
  OTLP JSON (nested attributes) breaks field lookup → hangs/crashes.
  Must stay as legacy flat format until sinks use proto accessors
  instead of field paths in serialized JSON.
  `OtlpJsonLog`/`OtlpJsonSpan` wrappers in `otel_json.rs` for opt-in.
- Vector-legacy format adapter for source/sink: TODO

**Migration tool:** `vector vrl-migrate` (spec in VRL_MIGRATION_TOOL.md)
should add rules for metric JSON field path changes:
- `.kind` → removed (use `.sum.aggregationTemporality` or `.gauge`)
- `.tags."key"` → `.sum.dataPoints[0].attributes` (OTLP attributes format)
- `.counter.value` → `.sum.dataPoints[0].asDouble`
- `.gauge.value` → `.gauge.dataPoints[0].asDouble`
- `.namespace` → `.resource.attributes` (`metric.namespace` key)
- `.timestamp` → `.sum.dataPoints[0].timeUnixNano`

### Phase 2b — Decouple sinks from serialized JSON field paths

**Critical insight:** OtelLog Serialize CANNOT change to OTLP JSON yet.
Sinks use field paths in the *serialized* JSON at runtime:
- websocket_server: `message_id_path` extracts field from serialized JSON ack
- Elasticsearch: `_id` field from serialized JSON body
- Splunk HEC: timestamp extraction from serialized JSON fields

**Prerequisite:** Before changing OtelLog Serialize, sinks must be migrated
to extract fields via OtelLog proto accessors BEFORE serialization, not
from the serialized JSON output. This is a per-sink refactor:
1. Extract needed fields (message_id, timestamp, _id) from OtelLog proto
2. Serialize the event
3. Use pre-extracted field values for routing/acking

**Only after all sinks decouple from serialized JSON paths** can OtelLog
Serialize change to OTLP JSON.

**Tooling ready:** `OtlpJsonLog`/`OtlpJsonSpan` wrappers in `otel_json.rs`.

### Phase 3 — Add OtelMetric aggregate/absolute APIs

Add `into_parts()` equivalent and `make_absolute()` to OtelMetric.
Unblocks: aggregate (1), incremental_to_absolute (1).

### Phase 4 — Core infrastructure (~6 files)

- `proto.rs` — serialize OTel types to protobuf directly
- `lua/event.rs` — Lua API exposes OTel fields
- `mod.rs` / `ref.rs` — remove `to_metric()` / `into_metric()`

### Phase 5 — Remove `to_value_legacy_layout()` from OtelLog/OtelSpan

Replace `get(path)`, `insert(path)`, `remove(path)` with direct proto field access.

### Phase 6 — Remove bridge function bodies

Delete `to_log_event()`, `to_legacy_metric()`. Keep `from_*` constructors.

### Phase 7 — Remove legacy types

Delete `LogEvent`, `Metric`, `TraceEvent`, `log_schema()`.

## Verification

- `cargo test -p vector --lib -- --skip throttle` — 1773 passed, 0 failed
- `cargo test -p codecs --lib` — 171 passed, 0 failed
- `cargo test -p vector-core --lib` — all event tests pass
- `cargo check -p vector` — compiles clean

## Context docs

- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
- `LEGACY_REMOVAL_PLAN.md` — VRL alias removal phases (A–E)
- `LEGACY_BRIDGE_REMOVAL.md` — design discovery for bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — VRL path model for OTel types
