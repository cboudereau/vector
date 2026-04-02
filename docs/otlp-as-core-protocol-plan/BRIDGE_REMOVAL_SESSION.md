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
- `from_log_event()` / `from_legacy_metric()` constructors — needed at I/O boundary until all sources produce OTel natively; even then, useful for `Event::from(LogEvent)` convenience in tests
- `OtelLog`/`OtelSpan`/`OtelMetric` proto accessors — the target API
- `VrlTarget` projections — VRL needs Value trees, built directly from proto
- Backward-compat VRL aliases (`.message` → `.body`) — kept for one release

**TRANSITIONAL (keep for now, remove later):**
- `to_log_event()` on OtelSpan — used by `trace_to_log` transform and text serializers
- `to_legacy_metric()` body — used by 24 external files (sinks, transforms)
- `Serialize` for OtelMetric via `to_legacy_metric()` — wire format risk if changed now

## Current state (after this session)

### OtelLog — no internal bridge delegation

All methods use `to_value_legacy_layout()` or direct proto. No method calls `to_log_event()`.
`to_value_legacy_layout()` is the next layer to eliminate.

### OtelSpan — no internal bridge delegation

Mirrors OtelLog: `to_value_legacy_layout()` + `apply_value_legacy_layout()`.
`to_log_event()` kept only for external callers.

### OtelMetric — value/kind/tag_value direct from proto

`extract_metric_data()` is single source of truth for kind/value/timestamp/dp_tags.
`to_legacy_metric()` calls it. `Serialize` still uses `to_legacy_metric()`.

### Dead code to clean up

| Item | Location | Action |
|------|----------|--------|
| `OtelMetric::timestamp()` | ~line 1851 | Delete — never called |
| `OtelMetric::tags()` | ~line 1872 | Delete — stub returning None |
| `OtelMetric::namespace()` (the one on OtelMetric, not OtelLog) | ~line 1877 | Delete — never called on OtelMetric |
| Section comment "delegate to to_log_event()" | line 660 | Update — no longer true |
| `apply_value_legacy_layout` "for now" comment | line 804 | Add TODO with target state |

### External bridge callers (not yet migrated)

**`to_log_event()` — 53 serialization + 9 transform + 15 test callers:**
- Codecs: json, csv, gelf, cef, avro, protobuf, logfmt, syslog, arrow, text
- Transforms: dedupe, sample, remap, log_to_metric, trace_to_log, reduce
- Sinks: elasticsearch, kinesis, splunk_hec, new_relic, amqp
- Other: enrichment_tables, template, schema definition, proto.rs

**`to_legacy_metric()` — 20 serialization + 15 test + 4 internal callers:**
- Sinks: prometheus, influxdb, statsd, greptimedb, gcp_stackdriver, appsignal
- Transforms: aggregate, tag_cardinality_limit, incremental_to_absolute, metric_to_log
- Core: Event::into_metric(), EventRef, proto.rs

**`from_log_event()` — 8 serialization + 3 transform + 22 test + 1 internal:**
- Codecs: protobuf, avro, vrl, gelf decoders; logstash source
- Core: `From<LogEvent> for Event`, apply_value_legacy_layout, discriminant
- Tests: heavy usage in test setup

**`from_legacy_metric()` — 6 serialization + 2 transform + 25 test + 1 internal:**
- Codecs: influxdb decoder; prometheus scrape; otlp/json/text encoders
- Core: `From<Metric> for Event`
- Tests: round-trip tests

## Phased plan — bridge removal to final architecture

### Phase 0 — Cleanup (no behavioral change)

Delete dead code, fix stale comments, add `// TEMPORARY BRIDGE` markers.

- [ ] Delete dead `OtelMetric` methods: `timestamp()`, `tags()`, `namespace()`
- [ ] Update section comment at line 660 (no longer delegates to `to_log_event`)
- [ ] Add TODO to `apply_value_legacy_layout` explaining target state
- [ ] Add `// TEMPORARY BRIDGE — migrate to direct proto access` to all bridge
      call sites in proto.rs, mod.rs, ref.rs

### Phase 1 — Codecs migrate off `to_log_event()` (~15 files)

Each codec serializer that does `otel_log.to_log_event().serialize(...)` should
instead serialize the OtelLog proto structure directly. This is the biggest batch.

**Order by complexity:**
1. **Text encoder** — just needs `.body()` as string
2. **LogFmt encoder** — iterate attributes as key=value
3. **CSV encoder** — iterate fields from proto
4. **JSON encoder** — serialize `to_value_legacy_layout()` (already done for Serialize impl)
5. **CEF/GELF/Syslog encoders** — map OTel fields to format-specific fields
6. **Avro/Protobuf encoders** — schema-driven, need OTel schema mappings
7. **Arrow encoder** — columnar, needs OTel column definitions

**Gate:** `cargo test -p codecs --lib` passes after each encoder.

### Phase 2 — Transforms migrate off bridges (~9 files)

Transforms that use `to_log_event()` or `to_legacy_metric()`:

- **dedupe** — needs field fingerprinting from OtelLog directly
- **sample** — needs field access for rate conditions
- **log_to_metric** — reads OtelLog fields, produces OtelMetric directly
- **metric_to_log** — reads OtelMetric proto, produces OtelLog directly
- **trace_to_log** — reads OtelSpan proto, produces OtelLog directly
- **aggregate/tag_cardinality/incremental_to_absolute** — need OtelMetric proto access

**Gate:** `cargo test -p vector --lib -- transforms::` passes after each.

### Phase 3 — Sinks migrate off `to_legacy_metric()` (~10 files)

Sinks that consume metrics via `to_legacy_metric()`:

- prometheus exporter/collector, influxdb, statsd, greptimedb,
  gcp_stackdriver, appsignal, splunk_hec, aws_cloudwatch

Each sink should read `OtelMetric` proto directly (name, data points, attributes).

### Phase 4 — Core infrastructure (~6 files)

- `proto.rs` — serialize OtelLog/OtelSpan/OtelMetric to protobuf directly (not via legacy types)
- `mod.rs` — `into_metric()`, `to_legacy_json_value()` bypass bridge
- `ref.rs` — `EventRef::into_metric()` bypass bridge
- `lua/event.rs` — Lua API exposes OTel fields (or keeps bridge as Lua adapter)
- `schema/definition.rs` — introspect OtelLog proto directly

### Phase 5 — Remove `to_value_legacy_layout()` from OtelLog/OtelSpan

After all external callers are migrated, the internal methods (`get`, `insert`,
`remove`, `convert_to_fields`, `as_map`, etc.) can access proto fields directly
instead of building a Value tree.

- Replace `get(path)` with direct proto field lookup
- Replace `insert(path)` with direct proto field mutation
- Remove `to_value_legacy_layout()` and `apply_value_legacy_layout()`

### Phase 6 — Remove bridge function bodies

- Delete `to_log_event()` from OtelLog and OtelSpan
- Delete `to_legacy_metric()` from OtelMetric
- Keep `from_log_event()` / `from_legacy_metric()` as I/O boundary constructors

### Phase 7 — Remove legacy types

- Delete `LogEvent`, `Metric`, `TraceEvent` type definitions
- Delete `log_schema()` config and all call sites
- Delete `MetricValue`, `MetricKind`, `MetricTags` (replaced by proto)

## Verification

- `cargo test -p vector --lib -- --skip throttle` — all tests pass
- `cargo test -p vector-core --lib` — all event tests pass
- `cargo check -p vector-core` — compiles clean

## Context docs

- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
- `LEGACY_REMOVAL_PLAN.md` — VRL alias removal phases (A–E)
- `LEGACY_BRIDGE_REMOVAL.md` — design discovery for bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — VRL path model for OTel types
