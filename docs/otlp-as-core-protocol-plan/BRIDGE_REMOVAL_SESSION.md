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

## Progress (updated 2026-04-03)

### Bridge call counts

| Function | Before session | After session | Removed |
|----------|---------------|---------------|---------|
| `to_log_event()` | 75 | 25 | **50 (67%)** |
| `to_legacy_metric()` | 40 | 20 | **20 (50%)** |
| **Total** | **115** | **45** | **70 (61%)** |

Notes:
- `to_log_event` count excludes 7 test-only assertion calls in transformer.rs
- dedupe MatchFields path is bridge-free; IgnoreFields still bridges
- transformer production code fully rewritten to OtelLog-native (was 8 calls → 0)

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
  Largest single bridge consumer eliminated (8 production calls → 0).
- prometheus scrape: uses OtelMetric::tag_value() + replace_tag() directly

### Remaining 45 bridge calls (by category)

**Core infrastructure (Phase 4) — 12 calls:**
- `proto.rs` (4): OTel → protobuf via legacy types
- `lua/event.rs` (3): Lua API exposes LogEvent/Metric
- `mod.rs` (3): `to_metric()`, `into_metric()`, `try_into_metric()`
- `ref.rs` (2): `EventRef::into_metric()`, `EventMutRef::into_metric()`

**Deeply coupled to LogEvent mutation — 8 calls:**
- `encoding/format/gelf.rs` (2): as_map_mut, rename_key, insert
- `encoding/format/arrow.rs` (1): convert_timestamps
- `sinks/elasticsearch` (2): pipeline around LogEvent
- `sinks/kinesis` (1): process_log takes LogEvent
- `sinks/splunk_hec` (1): render_template_string_from_log
- `sources/docker_logs` (1): partial event merging

**Deeply coupled to Metric mutation — 5 calls:**
- `transforms/aggregate` (1): metric.into_parts()
- `transforms/tag_cardinality_limit` (1): tags_mut().retain()
- `transforms/incremental_to_absolute` (1): make_absolute
- `transforms/metric_to_log` (1): transform_one(Metric)
- `sinks/appsignal` (1): normalizer.normalize(Metric)

**LogEvent field iteration — 2 calls:**
- `transforms/dedupe` (1): all_event_fields + all_metadata_fields (IgnoreFields only)
- `transforms/reduce` (1): Discriminant::from_log_event

**Intentional bridge / API — 5 calls:**
- `transforms/trace_to_log` (1): transform purpose IS span→log
- `api/schema/events/output` (2): GraphQL API wraps legacy types
- `conditions/datadog_search` (2): DD matcher takes &LogEvent

**Test / transitional — 13 calls:**
- `encoding/transformer.rs` (7): test assertions use to_log_event for field checks
- `encoding/format/json` (2): reduce_tags_to_single in Single mode
- `decoding/format/influxdb` (2): test assertions compare full Metric
- `decoding/format/otlp` (1): trace conversion via LogEvent
- `transforms/log_to_metric` (1): to_metrics() test helper

**Blockers for remaining production calls:**
- OtelLog needs `as_map_mut()` for GELF
- OtelMetric needs `tags_mut()` with `retain()` for tag_cardinality_limit
- OtelMetric needs `into_parts()` for aggregate
- OtelMetric needs `make_absolute()` for incremental_to_absolute

## Phased plan — remaining work

### Phase 2 — Add OtelMetric aggregate/absolute APIs

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
