# Plan: Remove Legacy Backward Compat + Update VRL Migration Tool

> **Part of** [`CONSOLIDATED_MIGRATION_PLAN.md`](CONSOLIDATED_MIGRATION_PLAN.md).
> That document is the master plan; this document tracks the
> **Legacy-type removal** workstream (LogEvent / TraceEvent / Metric
> deletion, bridge removal, legacy-layout round-trip elimination).
> Cross-links: `BRIDGE_REMOVAL_SESSION.md`, `VRL_MIGRATION_TOOL.md`,
> `VRL_OTEL_NATIVE_TARGETS.md`.

## Status (audit 2026-04-13)

**Phase C (OTel → Legacy bridge removal) — COMPLETE**

| Function | Direction | State |
|----------|-----------|-------|
| `OtelLog::to_log_event()` | OTel → Legacy | **DELETED** — 0 callers |
| `OtelSpan::to_log_event()` / `to_trace_event()` | OTel → Legacy | **DELETED** — 0 callers |
| `OtelMetric::to_legacy_metric()` | OTel → Legacy | **DELETED** — 0 callers |
| `Event::{to,into,try_into}_metric` | OTel → Legacy | **DELETED** |
| `Template::render_string_from_log/metric` | OTel → Legacy | **DELETED** |

All ~250 lines of OTel → Legacy bridge code were removed. Reverse-direction
access goes through `to_value_canonical` (read) / proto accessors.

**Phase E gap (write-back symmetry) — FIXED 2026-04-13**

`apply_value_map` (formerly `apply_value_legacy_layout`) extracts native
proto fields symmetrically with `to_value_canonical` (formerly
`to_value_legacy_layout`):

- **OtelLog**: `severity_text`, `severity_number`, `trace_id` (hex →
  bytes), `span_id` (hex → bytes), plus existing `body`/`message`,
  `timestamp`, `source_type`/`host` → resource
- **OtelSpan**: `name`, `trace_id`, `span_id`, `parent_span_id`,
  `start_time`, `end_time`, `kind`, `status` (with nested `code`/`message`)

Malformed hex IDs fall back to attribute storage rather than dropping
data. `from_log_event` and `from_trace_event` now delegate to
`apply_value_map`, so both round-trip paths share one field-routing
implementation.

5 previously-ignored gap tests now pass; plus new
`insert_preserves_corrupt_trace_id_as_attribute` covers the fallback.

## What's still legacy

### Forward-direction bridge (Legacy → OTel) — kept intentionally

These are I/O boundary constructors. They stay until sources/transforms
that still produce legacy types are migrated (out of scope for bridge
removal; tracked separately under "Source emission" in
`CONSOLIDATED_MIGRATION_PLAN.md`).

Accurate caller inventory (2026-04-13 audit):

| Function | Call sites | Role |
|----------|------------|------|
| `OtelLog::from_log_event(LogEvent)` | 20 | 17 in test modules, 1 in `Event::from(LogEvent)` impl (permanent bridge), 1 definition + doc, 1 `From<Log>` production path now migrated away |
| `OtelLog::from_value_map(Value, meta)` | 10 | Direct Value-tree construction at I/O — preferred entry point |
| `OtelSpan::from_trace_event(TraceEvent)` | 2 | Definition + `Event::from(TraceEvent)` impl |
| `OtelSpan::from_value_map(Value, meta)` | 2 | Direct Value-tree construction — new as of this session |
| ~~`OtelMetric::from_legacy_metric(Metric)`~~ | **0** | **DELETED** (T1, commit `727c801`) — inlined into `from_metric_parts`. Zero callers in `.rs` sources. |

**All actively-called non-bridge production uses are gone.** The remaining
callers are either:
- Tests that construct test inputs via the ergonomic legacy API
- The three `Event::from(LogEvent/Metric/TraceEvent)` bridge impls
  (permanent until source emission goes native OTel)
- `prometheus/remote_write/sink.rs` — already marked `TODO(otlp-migration)`,
  blocked on `MetricRef`/`BatchedMetrics` migration

The previous version of this plan claimed "~5 production callers"; that
count was wrong. On close inspection all sites are either test modules
(`#[cfg(test)]`, `mod tests`) or bridge `impl From<...>` definitions.

### Legacy types — current status (2026-04-20)

| Type | Status | Note |
|------|--------|------|
| `LogEvent` | **DELETED** (`80ff2fb`) | 1217 lines removed |
| `TraceEvent` | **DELETED** (`1236e8e`) | 191 lines removed |
| `Metric` struct | **DELETED** (`9bd0d06`) | 1040 lines removed across 21 files. T13b complete 2026-04-20. |
| `prometheus_parser::Metric` | Unrelated | External crate, not Vector's legacy type |

### VRL aliases / paths

| Alias | Target | Still live |
|-------|--------|-----------|
| `.message` | `.body` | Yes — deprecation notice only |
| `.timestamp` | `.time_unix_nano` | Yes — schema-meaning mapping |
| `.tags."key"` | `.attributes."key"` | Yes (OtelLog); OtelMetric `.tags` also live |
| `.host` | `.resource.attributes."host.name"` | Yes — fast-path alias in get/insert |
| `.source_type` | `.resource.attributes."source_type"` | Yes — fast-path alias in get/insert |
| `%vector.*` | `%pipeline.*` | Metadata namespace prefix — TBD |

These aliases are what the `vector vrl-migrate` tool (Phase A, blocked —
see `VRL_MIGRATION_TOOL.md`) should rewrite before being removed.

## What we missed / previously not tracked

1. **Write-back symmetry gap** (FIXED this session)
   — `apply_value_legacy_layout` was not symmetric with
   `to_value_legacy_layout`. Discovered during code review, fixed with
   explicit field extraction + unified `from_log_event`/`from_trace_event`
   delegation.

2. **Span fields entirely dropped on VRL insert** (FIXED this session)
   — OtelSpan's write-back collapsed all 8 native fields into attributes.
   Now routes correctly.

3. **`from_trace_event` duplicated the attribute sweep**
   (FIXED) — now delegates to `apply_value_legacy_layout`.

4. **Multi-value metric tags silently dropped by `with_tags`**
   (FIXED previous commit) — now mirrors `from_legacy_metric`
   (iter_sets + ArrayValue for N>1).

5. **Normalizer `_otel` method docs lied about avoiding round-trip**
   (FIXED previous commit) — now clearly documented as transitional shims.

6. **Prometheus remote_write double-convert**
   — `TODO(otlp-migration)` marker added; blocked on
   BatchedMetrics/MetricRef migration. Not easily fixable in isolation.

7. **Proto disk-buffer decode went through LogEvent/TraceEvent**
   (FIXED this session) — `From<EventWrapper> for Event` no longer
   constructs intermediate LogEvent/TraceEvent. `impl From<Log> for
   OtelLog` and `impl From<Trace> for OtelSpan` decode proto fields
   straight into OTel types. Removed ~50 lines including the now-unused
   `From<Log> for LogEvent` and `From<Trace> for TraceEvent` impls.
   Shared `decode_proto_metadata` helper replaces duplicated decoding.

8. **Pre-1970 span timestamps silently wrapped to huge u64 values**
   (FIXED this session) — `OtelSpan::apply_value_legacy_layout` cast
   negative i64 nanos directly to u64. Now rejects negative values and
   preserves the original as an attribute. Covered by
   `otel_span_pre_epoch_timestamp_preserved_as_attribute`.

9. **Lua adapter still constructed LogEvent** (FIXED this session)
   — `lua/event.rs` `FromLua for Event` now builds `OtelLog` directly
   via `from_value_map(Value::from_lua(...), EventMetadata::default())`.
   Deleted `lua/log.rs` entirely (80 lines, zero external callers).
   This removes the last non-proto, non-bridge-impl production site
   that constructed `LogEvent`.

## Remaining phases

- **A — VRL migration tool rules**: **DONE** (B4)
- **B — Remove VRL aliases**: **OPEN** — T15 eliminated internal alias
  infrastructure; user-facing VRL aliases remain until `vector vrl-migrate`
  rewrites configs. Product decision: approved.
- **C — OTel-to-Legacy bridge**: **DONE**
- **D — Legacy-to-OTel bridge**: Folded into F → G.
- **E — Remove legacy types from production**: **DONE**
- **F — Delete LogEvent + TraceEvent**: **DONE**
  - F.1-F.6 complete. `log_event.rs` deleted (`80ff2fb`).
  - `trace.rs` deleted (`1236e8e`).
- **G — Delete Metric struct + buffer compat + legacy layout**: **DONE**
  All 3 legacy types deleted. Legacy layout eliminated (T16 + T15 + T23).
  T24 closed (OTLP JSON decode already exists via proto serde).
  Post-cleanup: `to_value_legacy_layout` / `apply_value_legacy_layout`
  renamed to `to_value_canonical` / `apply_value_map`. `log_event!` macro
  renamed to `otel_event!`. DRY helpers extracted for resource/scope.

## Phase F — Delete legacy types entirely

Goal: remove `LogEvent`, `TraceEvent`, and `Metric` (the struct, not
its primitive sub-types) from the codebase. After Phase E the types
exist only as test ergonomics + a thin set of `From` bridges.

### Scope (audit 2026-04-15)

| Surface | Sites | Effort |
|---------|-------|--------|
| `Event::from(LogEvent)` impl | 1 prod (`http_client/client.rs:464`) + 248 test | small + 248 mechanical |
| `Event::from(Metric)` impl | 0 prod + 18 test | 18 mechanical |
| `Event::from(TraceEvent)` impl | 0 prod + 1 test | 1 mechanical |
| `LogEvent::from`/`default`/`from_str_legacy`/etc. test sites | 524 | ~1 day mechanical |
| `Metric::new` test sites | 390 | needs OtelMetric Histogram/Summary/Distribution/Set constructors first |
| `TraceEvent::from` test sites | 2 | 2 mechanical |
| `LogEvent` type definition | `lib/vector-core/src/event/log_event.rs` (1217 lines) | delete after callers gone |
| `TraceEvent` type definition | `lib/vector-core/src/event/trace.rs` (192 lines) | delete after callers gone |
| `Metric` struct + impls | `lib/vector-core/src/event/metric/mod.rs` (~1180 lines) | delete after callers gone |

### Types that **stay** even after Phase F

The metric subsystem has primitive types that `OtelMetric` depends on
heavily — these are NOT removable:

- `MetricKind` (Absolute/Incremental enum)
- `MetricValue` (Counter/Gauge/Distribution/Set/Histogram/Summary)
- `MetricTags`, `TagValue`, `TagValueSet`
- `Bucket`, `Quantile`, `Sample`, `StatisticKind`
- `MetricSeries`, `MetricName`, `MetricData`, `MetricTime`

`OtelMetric::into_metric_parts()` returns these; `with_tags`,
`with_namespace`, `with_timestamp` consume them. They are the OtelMetric
public API.

### Hidden dependencies to address first

- `Discriminant::from_log_event(&LogEvent)` (`event/discriminant.rs:27`)
  — used by `transforms/aggregate`, `transforms/dedupe`, `transforms/reduce`.
  Needs `Discriminant::from_otel_log(&OtelLog)` equivalent.
- `LogEvent::merge` — already migrated to `OtelLog::merge` for production
  paths via `LogEventMergeState`. Test convenience uses still exist.
- `OtelMetric` lacks constructors for Histogram, Summary, Distribution,
  Set. Need `OtelMetric::new_histogram`, `new_summary`, etc. so tests
  don't need `Metric::new(name, kind, MetricValue::AggregatedHistogram { ... })`.
- `lib/vector-core/src/event/proto.rs` disk buffer `From<Log> for OtelLog`
  is already direct (no LogEvent intermediate); same for `From<Trace> for OtelSpan`.
- `lib/vector-core/src/event/array.rs:336` `Arbitrary` impl uses
  `OtelLog::from_log_event(LogEvent::arbitrary(g))`. Could be rewritten
  to construct OtelLog directly via `OtelLog::from_value_map(Value::arbitrary(g), ...)`.

### Execution sequence

Phase F is **not a session** — it is an iterative campaign across many
sessions. Recommended order:

**F.1 — One real production caller** — **DONE** (`b53ce0c`)
- `http_client/client.rs:464` now uses `OtelLog::new(Default::default())`.

**F.2 — `Discriminant::from_otel_log`** — **ALREADY DONE** (pre-existing)
- `from_otel_log` already existed and was used by reduce transform.
- `from_log_event` has zero external callers (only discriminant.rs tests).

**F.3 — Extend OtelMetric constructors** — **DONE** (`eb6dfbf`)
- Added `new_histogram`, `new_summary`, `new_set`, `new_distribution`.
- Parity tests: histogram + summary match `from_legacy_metric` output.

**F.4 — Delete `Event::from(TraceEvent)` + bridge** — **DONE** (`b5befe5`)
- 10 test callers migrated to `OtelSpan::from_value_map`.
- GraphQL API `Trace` struct now holds `OtelSpan` directly.
- Deleted `to_trace_event`, `from_trace_event`, `From<TraceEvent> for Event`,
  `From<TraceEvent> for EventArray`.
- `trace.rs` type kept for now (proto.rs backward compat encoding).

**F.5 — Delete `Event::from(Metric)` bridge** — **DONE** (`ffe8423`)
- 60+ callers across 40 files migrated to explicit
  `Event::Metric(OtelMetric::from_legacy_metric(...))` or direct
  OtelMetric constructors.
- `impl From<Metric> for Event` deleted.
- `Metric` struct kept (used by MetricSet normalizer + OtelMetric
  round-trip internally).

**F.5 original plan (for reference):**
- 18 test callers. Each one does `Event::from(Metric::new(...))`.
- Replace with `Event::Metric(OtelMetric::new_<variant>(...))`.
- Delete `Event::from(Metric)` impl.
- Delete `OtelMetric::from_legacy_metric`.
- Delete `Metric::new` and its 390 test convenience callers (mechanical
  but high volume — split across multiple sessions or apply via macro
  refactor).
- Delete `lib/vector-core/src/event/metric/mod.rs` `Metric` struct
  + impls. Keep the primitive sub-types.

**F.6 — Scope `Event::from(LogEvent)` bridge** — **DONE** (`059f908`)
- Production code: all `Event::from(LogEvent)` calls replaced with
  direct `Event::Log(OtelLog::...)` construction.
- Bridge impl kept for test convenience (~150 test files use it).
  No feature flag needed — when tests are migrated to use
  `OtelLog::from_bytes/from_value_map` directly, both the bridge
  AND LogEvent type can be deleted together.
- `otel_event!` macro (renamed from `log_event!`) uses `OtelLog::new` directly.
- Zero production LogEvent imports remain outside vector-core/src/event/.
- **LogEvent migration COMPLETE** — 555 → 4 sites remaining.
  All test code migrated from LogEvent:: to OtelLog:: constructors.
  4 intentional holdouts:
  - `splunk_hec/logs/{encoder,sink}.rs` — LogEvent as HecData struct
    field type (requires HEC protocol change to remove)
  - `opentelemetry-proto/{logs,buffer_codec}.rs` — legacy disk buffer
    decode path (remove when old buffer format is sunset)
- `log_event.rs` (1217 lines) can be deleted once these 4 holdouts
  are addressed + the `From<LogEvent> for Event` bridge is removed.

**F.6 original plan (for reference):**
- 248 test callers. Highest volume.
- Replace with `Event::Log(OtelLog::from_bytes(...))` or
  `OtelLog::from_value_map(...)` patterns.
- Migrate the 524 `LogEvent::from`/`default`/etc. test convenience
  sites. Most are simple: `LogEvent::from("text")` → `OtelLog::from_bytes(Bytes::from_static(b"text"))`.
- Delete `OtelLog::from_log_event`.
- Delete `LogEvent::*` type definition + impls (1217 lines).
- Delete `Discriminant::from_log_event`.

**F.7 — Final cleanup (~1 hour)**
- Delete `lib/vector-core/src/event/array.rs` `LogEvent::arbitrary`
  call; rewrite as direct OtelLog construction.
- Delete `MetricEvent` type alias if any.
- Audit re-exports in `lib/vector-lib/src/lib.rs` and `vector_core`.
- Update `BRIDGE_REMOVAL_SESSION.md` final report.

### Estimated total
**~4–6 sessions** of focused work. Lowest-risk site (F.4 trace) first;
highest-volume site (F.6 log) last. Each session leaves the codebase
green.

### Risk
LOW per session, MEDIUM cumulative. Mostly mechanical text replacement
with clear failure modes (compile errors). Main risk:
`LogEvent::from_str_legacy` adds a Legacy-namespace timestamp; naive
replacements may forget it, causing test assertions on `timestamp`
fields to fail. Mitigation: provide an `OtelLog::from_str_legacy`
helper with the same semantics during the migration window.

## VRL Migration Rules (Phase A, reference)

| Rule | Old | New |
|------|-----|-----|
| LOG-01 | `.message` | `.body` |
| LOG-02 | `.timestamp` | `.time_unix_nano` |
| LOG-03 | `.severity` / `.level` | `.severity_text` |
| LOG-05 | `.tags` | `.attributes` |
| LOG-06 | `.tags."key"` | `.attributes."key"` |
| MET-06 | `.value.counter.value` | `.data.sum.data_points[0].value` |
| MET-07 | `.value.gauge.value` | `.data.gauge.data_points[0].value` |

## Full legacy-code audit (2026-04-13)

A codebase-wide audit revealed the **~14 source/codec files** estimate
was wrong — ~80 production files still construct or accept legacy
types. Grouped below so nothing is lost.

### Group A — Dead code, safe to delete right now

| Item | Location | Note |
|------|----------|------|
| `VrlTarget::LogEvent` variant | `lib/vector-core/src/event/vrl_target.rs:633` | `VrlTarget::new` never constructs it; 23 match arms exist but no write site |
| `VrlTarget::Trace` variant | `vrl_target.rs:639` | same — dead |
| `VrlTarget::Metric { metric, .. }` variant | `vrl_target.rs:634` | same — dead |
| `TargetIter<LogEvent>` + `TargetIter<TraceEvent>` iterators | `vrl_target.rs:669, 686` | only consumed by the dead variants |
| `create_log_event` helper | `vrl_target.rs:663` | only used by dead TargetIter |

**Action**: delete these and prune 23 match arms. Reduces `vrl_target.rs`
by ~300 lines and narrows the mental model of VRL targets. **Tracked as
task #30.**

### Group B — Codecs (the Deserializer/Encoder trait layer)

The `Deserializer::parse` trait already returns `Event` (which wraps
`OtelLog`), so callers unavoidably go through `Event::from(LogEvent)`.
17 codec files still construct `LogEvent`:

```
lib/codecs/src/decoding/format/
  avro.rs, gelf.rs, protobuf.rs, syslog.rs, vrl.rs
lib/codecs/src/encoding/
  encoder.rs, transformer.rs, format/
    arrow.rs, avro.rs, cef.rs, csv.rs, gelf.rs, json.rs, logfmt.rs,
    raw_message.rs, syslog.rs, text.rs
```

**Migration path**: update each decoder to return an `OtelLog` (or
`OtelSpan` for trace codecs) directly via `from_value_map`. Encoders
read from `&OtelLog` already (via `Event::as_log() → &OtelLog`) but some
still pass through `LogEvent` internally.

### Group C — Sources (I/O boundary, LogEvent for field assembly)

14 sources build `LogEvent` before `.into()` into `Event`:

```
src/sources/
  dnstap/, docker_logs/, fluent/, heroku_logs.rs, http_client/,
  journald.rs, kubernetes_logs/{parser/cri, parser/docker,
  namespace_metadata_annotator, node_metadata_annotator,
  pod_metadata_annotator, partial_events_merger},
  socket/, splunk_hec/, syslog.rs, util/framestream,
  util/http/{headers, query}
```

**Migration path**: replace `LogEvent::default(); log.insert(...)` +
`.into()` patterns with direct `OtelLog::from_value_map(...)`.
Mechanical but high volume.

### Group D — Sinks (reading LogEvent; mostly test helpers)

13 sink files mention `LogEvent`; most are test/util:

```
src/sinks/
  amqp/config.rs, aws_cloudwatch_logs/request_builder.rs,
  azure_blob/test.rs, console/sink.rs, file/mod.rs,
  gcp/cloud_storage.rs, humio/logs.rs, influxdb/logs.rs,
  kafka/request_builder.rs, loki/sink.rs, mezmo.rs,
  papertrail.rs, socket.rs, util/encoding.rs,
  webhdfs/test.rs, websocket_server/sink.rs
```

Most use `event.as_log() → &OtelLog` correctly now; remaining sites
create LogEvent for template rendering or test setup. Low priority.

### Group E — Transforms (VRL, log_to_metric, metric_to_log, etc.)

12 transform files. Key ones:

```
src/transforms/
  aws_ec2_metadata.rs (tests), dedupe/config.rs, filter.rs,
  log_to_metric.rs, lua/v1/mod.rs, lua/v2/mod.rs,
  metric_to_log.rs, reduce/{transform, merge_strategy},
  remap.rs, throttle/transform.rs, window/transform.rs
```

`remap.rs` is the VRL entry point — gated on Phase A.
`log_to_metric.rs` / `metric_to_log.rs` are explicit bridges that
should stay until source emission is native OTel.

### Group F — Internal infrastructure

| Site | Role | Priority |
|------|------|----------|
| `src/trace.rs` | Vector's own log-event emission for internal tracing | Low — isolated subsystem |
| `src/template.rs` | Template rendering (test helpers) | Low |
| `src/common/http/server_auth.rs` | HTTP auth VRL evaluation | Low — calls VrlTarget::new |
| `src/config/unit_test/mod.rs` | Unit test config plumbing | Low |
| `lib/opentelemetry-proto/src/logs.rs:255-261` | **OTLP→LogEvent conversion (ironic)** | Medium — should go OTLP→OtelLog direct |
| `lib/opentelemetry-proto/src/buffer_codec.rs` | Disk buffer OTLP codec | Medium |
| `lib/vector-core/src/config/mod.rs` | log_schema field hoisting | Low |
| `lib/vector-core/src/transform/mod.rs` | FunctionTransform trait uses LogEvent | Medium — trait-level change |
| `lib/vector-core/src/fanout.rs` | Internal plumbing (tests) | Low |
| `lib/vector-core/src/schema/definition.rs` | Schema definition (tests) | Low |
| `lib/vector-vrl-metrics/src/common.rs` | VRL-metrics converter | Medium |

### Group G — Legacy Metric API

1. **`api/schema/metrics/*` (8 files)** — Vector's GraphQL API wraps
   legacy `Metric` for component observability. Untracked previously.
   Not in any hot path but a full-width legacy dependency.

2. **`src/sinks/util/normalizer.rs`** — helper; feeds into the
   `MetricNormalizer` / `MetricSet` infrastructure.

3. **`src/sinks/prometheus/{mod, remote_write/*}.rs`** — still use
   legacy Metric for BatchedMetrics/MetricRef collector path (blocked).

4. **`lib/vector-vrl-metrics/src/common.rs`** — VRL metric
   manipulation functions.

### Group H — Bridges (formerly "permanent", now targeted for deletion)

| Function | Status |
|----------|--------|
| `Event::from(LogEvent)` | **DELETED** — `log_event.rs` deleted |
| `Event::from(TraceEvent)` | **DELETED** — `trace.rs` deleted |
| `Event::from(Metric)` | **DELETED** — bridge removed in F.5 |
| `OtelLog::from_log_event` | **DELETED** — `LogEvent` no longer exists |
| `OtelSpan::from_trace_event` | **DELETED** — `TraceEvent` no longer exists |
| `OtelMetric::from_legacy_metric` | **DELETED** — Phase G task T1 (commit `727c801`): inlined into `from_metric_parts`, zero remaining `.rs` callers |

## Definitive blocker inventory (2026-04-15)

Per the comprehensive codebase audit, these are the **only** remaining
production blockers. All other LogEvent/Metric references are either
test convenience, permanent `Event::from(...)` bridge impls, or
cosmetic (e.g. internal GraphQL schema names).

### Real blockers

| # | Blocker | Location | Scope | Priority | Plan doc |
|---|---------|----------|-------|----------|----------|
| B1 | ~~dnstap parser~~ | `lib/dnstap-parser/src/parser.rs` | **DONE** (`c8e02e1`): 17 fns now take `&mut Value`. Source uses `OtelLog::modify_as_value` to amortize round-trip. VRL function drops LogEvent intermediate. | ~~HIGH~~ **DONE** | `DNSTAP_PARSER_MIGRATION.md` |
| B2 | ~~Prometheus `MetricRef` dedup key~~ | `src/sinks/prometheus/exporter.rs` | **UNBLOCKED** (`887b657`): `MetricRef::from_otel_metric` added + parity test. Designates the OTel-native entry point; exporter's input path can migrate without touching dedup logic. | ~~MEDIUM~~ **DONE (unblock)** | — |
| B3 | `BatchedMetrics` + `MetricSet` | `src/sinks/util/buffer/metrics/normalize.rs` | **DONE** — external OTel API migrated (`MetricNormalize` trait, `make_absolute`/`make_incremental`/`insert_update` all take `OtelMetric`). Internal `MetricEntry` stores `(MetricSeries, MetricData, EventMetadata)` tuple — no `Metric` struct dependency. Remaining goal: full deletion of `Metric` struct (Phase G task T13). | **DONE (unblock)** | — |
| B4 | ~~VRL migration tool (Phase A)~~ | `src/vrl_migrate/` | **DONE** — discovered fully implemented. 22 rules across 3 passes (10 structural + 7 semantic + 5 metric), CLI wired into `vector vrl-migrate` subcommand, 29/29 tests passing. Blocks Phase B (alias removal) only on the user-facing decision to remove aliases. | ~~MEDIUM~~ **DONE** | `VRL_MIGRATION_TOOL.md` |
| B5 | ~~`src/trace.rs`~~ | `src/trace.rs` | **DONE** (`234eb6d`): migrated to `OtelLog`. Added `OtelLog::from_tracing_event` (visitor-based build-once). | ~~LOW~~ **DONE** | — |
| B6 | ~~`components/validation/resources/event.rs`~~ | `src/components/validation/resources/event.rs` | **DONE** (`3f2e957`): `EventData::into_event` uses `OtelLog::from_bytes` / `from_value_map`. | ~~LOW~~ **DONE** | — |

### Beyond blockers — Phase F goal: delete legacy types entirely

All B1–B6 blockers are resolved. The remaining work is the **Phase F
campaign** to delete `LogEvent`, `TraceEvent`, and the `Metric` struct
themselves (~2600 lines of type definitions + ~916 mechanical test
migrations). See the dedicated "Phase F" section below.

### Dead code (cleanable, not really blockers)

| Item | Location | Why dead |
|------|----------|----------|
| `logs_to_export` | `lib/opentelemetry-proto/src/buffer_codec.rs:184` | Zero external callers; production path uses `otel_logs_to_export` |
| `log_event_to_resource_logs` | `buffer_codec.rs:192` | Only called from dead `logs_to_export` |
| `log_event_to_log_record` | `buffer_codec.rs:197` | Only called from dead `log_event_to_resource_logs` |
| `trace_event_to_span` / `read_scope_from_trace_event` | `buffer_codec.rs` | Already `#[allow(dead_code)]`, 0 external callers |

### Corrections to earlier assessments

| Item | Reclassification |
|------|------------------|
| `FunctionTransform` trait | **NOT a blocker** — takes `Event`, not `LogEvent`; already OTel-compatible |
| `api/schema/metrics/*` (GraphQL) | **NOT a blocker** — internal observability API, runtime-invisible |
| k8s annotators "48 inserts" | **NOT a perf issue** — production uses `set_resource_attribute` (O(1) proto), tests use `insert()` |
| Benchmarks, test-only sites | **NOT blockers** — no runtime impact |

### Blocker unblock actions (review 2026-04-15)

**B1 — dnstap parser** (HIGH)
- Recommended: `modify_as_value` at top-level entry point — change internal
  helpers to take `&mut Value`, wrap in a single `modify_as_value` call
  site. One round-trip per frame vs. 17+.
- Action: write `DNSTAP_PARSER_MIGRATION.md` specifying field mapping
  and test strategy before execution. ~1 day effort.

**B2 — Prometheus `MetricRef`** (MEDIUM)
- Recommended: add `impl From<&OtelMetric> for MetricRef`. Keeps the
  existing exporter de-dup logic untouched. ~2 hours. Unblocks
  prometheus exporter from requiring legacy `Metric` inputs.

**B3 — `BatchedMetrics` + `MetricSet`** (MEDIUM) — **DONE**

Resolved via option 2: full native-OtelMetric `MetricSet` (commit
`3856582`, Campaign A). `MetricNormalize` trait and all public
methods (`make_absolute`, `make_incremental`, `insert_update`) now
take `OtelMetric`. `MetricEntry` stores `(MetricSeries, MetricData,
EventMetadata)`; `from_otel` / `into_otel` are the constructors.
The `_otel` wrapper shims were deleted as dead code (T21). Only
`normalize_otel` remains as a `OtelMetric → Event` convenience at
the sink boundary.

**B4 — VRL migration tool (Phase A)** (MEDIUM)
- Spec exists in `VRL_MIGRATION_TOOL.md` (8 log + 6 metric rules).
  Scaffolding exists in `src/vrl_migrate/`.
- First action: audit `src/vrl_migrate/rules/` for existing
  implementations — may find work already done.
- Then: implement MVP rules (LOG-01, MET-06, MET-07) with fixture tests.

**B5 — `src/trace.rs`** (LOW, quick win)
- Action: add `impl From<&tracing::Event> for OtelLog`, swap
  `Vec<LogEvent>` → `Vec<OtelLog>` throughout. ~1 hour.

**B6 — `components/validation/resources/event.rs`** (LOW, quick win)
- Action: switch `Log(String)` → `OtelLog::from_bytes()`,
  `LogBuilder(HashMap)` → build `ObjectMap` + `OtelLog::from_value_map`.
  ~30 min.

### Recommended execution order

1. **B5 + B6** — 1.5h of quick wins, no design needed
2. **B2 (MetricRef option 1)** — 2h, isolated
3. **B3 option 1 or 3** — 4h or 0h depending on appetite
4. **B4 audit** — 1h discovery before committing to implementation
5. **B1 dnstap** — biggest remaining, needs design doc first

## Phase G — Final legacy type + buffer compat removal (2026-04-20)

### Goal

Delete **all** remaining legacy Vector types:
- ~~`LogEvent`~~ — **DELETED** (`80ff2fb`, Phase F.6)
- ~~`TraceEvent`~~ — **DELETED** (`1236e8e`, Phase G.1)
- ~~`Metric` struct~~ — **DELETED** (`9bd0d06`, Phase G T13b)

Delete all backward buffer compatibility code and bridge functions.

### Current state (2026-04-20 end-of-session)

| Counter | Value | Note |
|---------|-------|------|
| `Metric::new` sites | **0** | **ALL DELETED** — T12 + T13b complete |
| `Metric` struct | **DELETED** | `9bd0d06` — 723 lines of struct + impls + trait impls removed |
| `from_legacy_metric` sites | **0** | **DELETED** — T1 (`727c801`) |
| `from_metric_parts` sites | 91 | Canonical OtelMetric constructor — stays |

### Complete task list — full migration to OTel-native types

Status key: **DONE** / **PARTIAL** / **OPEN** / **BLOCKED**

---

#### Workstream 0: Delete legacy types (LogEvent, TraceEvent)

| # | Task | Status | Commit | Note |
|---|------|--------|--------|------|
| F.6 | Delete `log_event.rs` (1217 lines) | **DONE** | `80ff2fb` | LogEvent type fully removed |
| G.1 | Delete `TraceEvent` type (191 lines) | **DONE** | `1236e8e` | trace.rs, proto.rs impls, buffer_codec dead code |
| G.2 | Remove proto backward buffer compat | **DONE** | `9363568` | Old decoders, dual encoding, deprecated fields |

#### Workstream 1: Source/transform production code → OtelMetric

| # | Task | Status | Commit | Note |
|---|------|--------|--------|------|
| G.3-boundary | Migrate 14 production boundary callers | **DONE** | `a696cd1`..`00f420b` | host_metrics, internal_metrics, nginx, mongodb, postgresql, static_metrics, aggregate, log_to_metric, statsd, apache, aws_ecs, eventstoredb |
| G.3a | Source parsers return OtelMetric directly | **DONE** | `8382733` | prometheus (61), apache (61), aws_ecs (30), statsd (19), eventstoredb (9) — 180 Metric::new eliminated |

#### Workstream 2: Delete Metric struct — replace with OtelMetric (T1-T14)

Previously done:

| # | Task | Status | Commit |
|---|------|--------|--------|
| T1 | Inline `from_legacy_metric` into `from_metric_parts` | **DONE** | `727c801` |
| T2 | Proto decode bypasses Metric struct | **DONE** | `727c801` |
| T7 | Sink/transform test `from_legacy_metric` migration (137 sites) | **DONE** | `4a56724`..`656da3c` |
| T8 | VRL metrics → OtelMetric | **DONE** | `4a56724` |
| T9 | Lua bindings → parts-based | **DONE** | `8a77309` |
| T10 | Delete proto encode `From<Metric>` parity test | **DONE** | `5a673ca` |
| T11 | OtelMetric parity tests | **DONE** | `656da3c` |
| T14 | Arbitrary property tests | **DONE** | `ac88015` |

Remaining — **replace Metric with OtelMetric everywhere, then delete
the struct**:

| # | Task | Status | Note |
|---|------|--------|------|
| T3 | Change `MetricNormalize` trait: `Metric` → `OtelMetric` | **DONE** | `3856582` |
| T4 | `MetricSet` internals: `Metric` → `OtelMetric` | **DONE** | `3856582` |
| T5 | Prometheus collector/exporter: direct field access | **DONE** | `34e915a` |
| T6 | Split iterator: `Metric` → `OtelMetric` | **DONE** | `3856582` |
| T12 | Test code: `Metric::new` → `OtelMetric` constructors | **DONE** | `43ede6e`..`9bd0d06` | **All `Metric::new` sites migrated or deleted.** Per-file: statsd, prometheus, vrl-metrics, lua, codecs, sinks, transforms. `from_legacy_value` deleted (`21d1694`). |
| T20 | `capture_metrics()` → return `Vec<OtelMetric>` | **DONE** | `acc456c` | Internal metrics now OtelMetric natively. 48 callers updated. |
| T13a | Eliminate Metric from all production code | **DONE** | `1b188d6`..`edd3329` | **Metric struct is ABSENT from all production code paths.** GraphQL API, sink boundaries, MetricsBuffer all OtelMetric. |
| T13b | Delete `Metric` struct + all `impl Metric` + `Display` / `Arbitrary` / `FromLua` / `From<proto::Metric>` / proto encode impls | **DONE** | `cf9a713`..`9bd0d06` | **Metric struct fully deleted.** -1040 lines across 21 files. metric_to_log uses inline `MetricJson` for serde. `TestMetricInput` replaces `Metric` in config unit tests. `metric/mod.rs` tests rewritten to use `MetricData` directly. |
| T21 | Delete `make_absolute_otel` / `make_incremental_otel` / `insert_update_otel` wrappers | **DONE** | `3856582` — real methods now take `OtelMetric` directly. `normalize_otel` kept intentionally as the `OtelMetric → Event` convenience for sink pipelines. |
| T22 | Delete `MetricEntry::from_metric` / `into_metric` | **DONE** | `3856582` — replaced with `from_otel` / `into_otel` |

#### Workstream 3: Eliminate legacy field model (layout round-trip)

| # | Task | Status | Commit | Note |
|---|------|--------|--------|------|
| T16 | Fast-path get/insert/remove bypassing legacy round-trip | **DONE** | `5198ea7`..`a86b435` | Phases 1-3 complete. All-field get/insert/remove for OtelLog + OtelSpan bypass legacy layout. Phase 4 (batch methods) deferred to T15/T23. |
| T15 | Remove VRL aliases + Serialize → proto-canonical | **DONE** | Phases 1-5 complete | Phase 1: aliases removed. Phase 2: Serialize → OTLP JSON. Phase 3: value()/keys()/as_map() canonical. Phase 4: 78 test fixes (`940da7e`). Phase 5: alias infrastructure deleted. |
| T23 | Simplify/rename legacy layout functions | **DONE** | | `to_value_legacy_layout` deleted (all callers → `to_value_canonical`). `apply_value_legacy_layout` renamed → `apply_value_map`; restores resource/scope via shared `restore_resource`/`restore_scope` helpers. `hoist_resource_fields`/`hoist_scope_fields` deleted. `message_path()`/`host_path()` deleted. `log_event!` macro → `otel_event!`. |

#### Workstream 4: Runtime safety + correctness

| # | Task | Status | Commit | Note |
|---|------|--------|--------|------|
| T17 | ~~Implement real `Deserialize` for OTel types~~ | **REVERTED** | `070e536` reverts `7040e7b` + `acf50cc` | serde Deserialize on Vector types silently dropped OtelMetric `data` field. Not needed — proto serde + `from_parts` covers the JSON decode path (see T24). |
| T19 | Fix VrlTarget::OtelMetric remove (write-back) | **DONE** | `7040e7b`+ | Write-back for name, description, unit, resource, scope, attributes — same paths as target_insert. Returns removed value. |
| T24 | ~~OTLP JSON → OtelMetric parse helpers~~ | **NOT NEEDED** | — | The OTLP JSON decode path already exists via proto serde: `serde_json::from_slice::<ExportMetricsServiceRequest>` → `OtelMetric::from_parts(metric, resource, scope, metadata)`. See `src/sources/opentelemetry/http.rs:369-391`. Same for logs (line 343) and traces (line 395). The `otel_json.rs` Serialize side has no mirror needed — prost's `#[derive(Deserialize)]` on proto types IS the parser. |

#### Workstream 5: Cleanup

| # | Task | Status | Commit | Note |
|---|------|--------|--------|------|
| T18 | Stale names and dead aliases | **DONE** | `d920e8a`..`5a673ca` | `MergeState` renamed, dead aliases deleted, span helpers deleted, `try_into_log_coerce`/`into_log_coerce` replaced (17 callers) + aliases deleted. `log_event!` macro renamed to `otel_event!`. **Remaining cosmetic:** `event.proto` deprecated field markers. |

---

#### T1 — Inline `from_legacy_metric` into `from_metric_parts` (BLOCKER)

**File:** `lib/vector-core/src/event/otel_event.rs:1955-2204`

`from_metric_parts` currently rebuilds a temporary `Metric` struct:
```rust
let m = super::Metric::from_parts(series, data, metadata);
Self::from_legacy_metric(m)
```
Inline the ~250 lines of conversion logic so `from_metric_parts` works
without the `Metric` struct. This unblocks deleting the struct.

**Effort:** Medium — move code, remove intermediate. No logic changes.

---

#### T2 — Proto decode: bypass Metric struct (BLOCKER)

**File:** `lib/vector-core/src/event/proto.rs:68-75, 208-213`

The proto→Event decode path currently goes:
`proto::Metric → super::Metric → OtelMetric::from_legacy_metric()`

Create a direct `From<proto::Metric> for OtelMetric` that decodes
proto fields straight into OtelMetric (similar to how `From<Log> for
OtelLog` already works). Then delete `From<proto::Metric> for
super::Metric` and `From<super::Metric> for proto::Metric`.

**Effort:** Medium — rewrite ~80 lines of metric proto decode.

---

#### T3 — MetricSet / Normalizer: remove Metric dependency — **DONE** (was B3)

**File:** `src/sinks/util/buffer/metrics/normalize.rs`

Done via Campaign A (commit `3856582`). MetricSet now operates
natively on `OtelMetric` and stores `(MetricSeries, MetricData,
EventMetadata)` tuples internally (`MetricEntry`). The `_otel`
wrapper shims (`make_absolute_otel`, `make_incremental_otel`,
`insert_update_otel`) were deleted — the real methods take
`OtelMetric` directly. Only `normalize_otel` remains, as the
`OtelMetric → Event` convenience for sink-side plumbing.

Left to do under T13: delete the `Metric` struct itself (last
in-tree legacy metric type).

---

#### T4 — Prometheus collector: `encode_metric(&Metric)` → OtelMetric

**File:** `src/sinks/prometheus/collector.rs`

`encode_metric()` takes `&Metric` and accesses `.name()`, `.namespace()`,
`.kind()`, `.tags()`, `.value()`, `.timestamp()`. Change to accept
`(&MetricSeries, &MetricData)` or `&OtelMetric` with accessor wrappers.

**Effort:** Medium — ~50 lines, mechanical accessor replacement.

---

#### T5 — Prometheus exporter: Metric aggregation logic

**File:** `src/sinks/prometheus/exporter.rs`

Uses `metric.series()`, `metric.add()`, `metric.value()` in the
metric collection/dedup path. Depends on T3 (MetricSet refactor).

**Effort:** Medium — coupled to T3.

---

#### T6 — Split iterator: `AggregatedSummarySplitter`

**File:** `src/sinks/util/buffer/metrics/split.rs`

`split()` takes `Metric`, calls `input.into_parts()`, rebuilds multiple
`Metric::from_parts()` objects. Change to accept/return tuples.

**Effort:** Low-Medium — ~70 lines, mechanical.

---

#### T7 — Sink test code: migrate `Metric::new` in tests (~118 sites)

**Files:** `influxdb/metrics.rs` (18), `prometheus/*` (23),
`statsd/encoder.rs` (10), `buffer/metrics/*` (16),
`sematext`, `humio`, `greptimedb`, `cloudwatch`, etc. (~25+)

Mechanical: replace `Metric::new(name, kind, value)` with
`OtelMetric::new_counter/new_gauge/...` or `from_metric_parts`.

**Effort:** High volume but mechanical — bulk sed + manual fixups.

---

#### T8 — VRL metrics: `vector-vrl-metrics/src/common.rs` (29 sites)

**File:** `lib/vector-vrl-metrics/src/common.rs`

`MetricsStorage` returns `Vec<Metric>`. VRL metric manipulation
functions create/inspect `Metric` structs.

Change to store and return `OtelMetric` (or tuples). Requires
understanding VRL function signatures.

**Effort:** Medium — 29 sites, need to trace VRL function call paths.

---

#### T9 — Lua bindings: `lua/metric.rs` (17 sites)

**File:** `lib/vector-core/src/event/lua/metric.rs`

`LuaMetric` holds `Metric`. `IntoLua` / `FromLua` impls serialize/
deserialize Metric fields. ~80 lines of pattern matching on
`MetricValue` variants.

Change `LuaMetric` to hold `(MetricSeries, MetricData, EventMetadata)`
or reference `OtelMetric`.

**Effort:** Medium — extensive serialization logic to rewrite.

---

#### T10 — Proto encode: `From<super::Metric> for proto::Metric`

**File:** `lib/vector-core/src/event/proto.rs`

`encode_metric_proto()` already takes `(MetricSeries, MetricData,
EventMetadata)` — the `From<super::Metric>` impl just calls
`metric.into_parts()` then delegates. Once T3 is done and no code
constructs `Metric` anymore, delete this impl.

**Effort:** Trivial — delete dead code after T1-T9 complete.

---

#### T11 — OtelMetric parity tests (15 sites in otel_event.rs)

**File:** `lib/vector-core/src/event/otel_event.rs`

Tests like `new_counter_matches_from_legacy_metric` construct
`Metric::new(...)` to verify OtelMetric constructors match. Once
`from_legacy_metric` is inlined (T1), rewrite tests to verify
`from_metric_parts` directly.

**Effort:** Low — 15 mechanical test updates.

---

#### T12 — Metric struct internal tests (30 sites in metric/mod.rs)

**File:** `lib/vector-core/src/event/metric/mod.rs`

Tests for `Metric::update`, `Metric::subtract`, `Metric::add`,
serialization, display. These test `MetricData` methods through
the Metric wrapper. Rewrite to test `MetricData` directly.

**Effort:** Low — tests for methods that already exist on MetricData.

---

#### T13b — Delete Metric struct (final step)

**File:** `lib/vector-core/src/event/metric/mod.rs`

After T1-T12 are done:
- Delete `pub struct Metric` (~68 lines)
- Delete all `impl Metric` methods (~330 lines)
- Delete trait impls: `AsRef<MetricData>`, `Display`, `EventDataEq`,
  `ByteSizeOf`, `EstimatedJsonEncodedSizeOf`, `Finalizable`,
  `GetEventCountTags`, `Configurable`
- Delete `Metric::new`, `new_with_metadata`, `from_parts`, `into_parts`
- Delete all builder methods: `with_name`, `with_namespace`,
  `with_timestamp`, `with_interval_ms`, `with_tags`, `with_value`
- Remove `pub use metric::Metric` from `event/mod.rs`
- ~~Delete `from_legacy_metric`~~ **DONE** (T1, `727c801`)
- ~~Delete `from_metric_parts` bridge~~ **N/A** — `from_metric_parts`
  is the canonical OtelMetric constructor, not a bridge, and stays.

**Effort:** Trivial — just deletion, all callers already migrated.

---

---

#### T14 — `Arbitrary for Metric` in property tests

**Files:** `lib/vector-core/src/event/test/common.rs:63,110`,
`lib/vector-core/src/event/array.rs:331`

Property tests generate random `Metric` via `Arbitrary` then wrap in
`OtelMetric::from_legacy_metric`. Rewrite to generate `OtelMetric`
directly (or generate `MetricSeries` + `MetricData` + `EventMetadata`
then call `from_metric_parts`).

**Effort:** Low — 3 sites.

---

#### T15 — Phase B: Remove VRL aliases (product decision required)

**Files:** `lib/vector-core/src/event/otel_event.rs` (`to_value_legacy_layout`)

The legacy field aliases are still live in VRL:

| Alias | Target | Status |
|-------|--------|--------|
| `.message` | `.body` | Live — `to_value_legacy_layout` hoists body→message |
| `.timestamp` | `.time_unix_nano` | Live — schema-meaning mapping |
| `.tags."key"` | `.attributes."key"` | Live |
| `.host` | `.resource.attributes."host.name"` | Live — hoisted |
| `.source_type` | `.resource.attributes."source_type"` | Live — hoisted |

`vector vrl-migrate` (Phase A, DONE) can rewrite user configs.
Removing aliases is a **breaking change** gated on product decision.

**Effort:** Medium — ~15-25 test breakages expected.

---

#### T16 — Eliminate legacy layout round-trip from get/insert/remove — DONE

**File:** `lib/vector-core/src/event/otel_event.rs`

This is the **deepest remaining legacy pattern**. Every `OtelLog::insert()`,
`get()`, `remove()` call does a full round-trip through a flat `Value` tree:
1. `to_value_legacy_layout()` — clones entire event into ObjectMap
2. Mutate the ObjectMap
3. `apply_value_legacy_layout()` — reparses back into proto

**Known issues from code review (2026-04-17):**
- Destroys `scope` (set to None on every write-back)
- Zeros `observed_time_unix_nano` on every write-back
- O(event_size) per insert/get/remove call
- Resource/scope sub-objects get demoted to attributes after round-trip

**Depends on:** T15 (VRL aliases removal) — the legacy layout exists
specifically to support the legacy field names. Once aliases are gone,
OtelLog can expose proto fields directly and eliminate the round-trip.

**Effort:** Very High — touches the core mutation API of OtelLog.

---

#### T17 — ~~Implement real `Deserialize` for OTel types~~ **REVERTED** (`070e536`)

Audit found **zero live callers** of serde `Deserialize` on
`OtelLog`/`OtelSpan`/`OtelMetric`; the short-lived partial
implementation (`7040e7b`+`acf50cc`) silently dropped the `data`
field. The entire `Deserialize` chain was removed (-119 lines).

**Why not needed:** The OTLP JSON decode path uses prost-generated
`serde::Deserialize` on the **proto types** (ExportLogsServiceRequest,
etc.), then `OtelLog/OtelMetric/OtelSpan::from_parts(proto, resource,
scope, metadata)`. See `src/sources/opentelemetry/http.rs:343-415`.
Adding Deserialize on the Vector wrapper types was redundant and
dangerous (silent data loss).

---

#### T18 — Cleanup: stale names and dead aliases

| Item | Location | Action |
|------|----------|--------|
| `LogEventMergeState` | `merge_state.rs`, `docker_logs/mod.rs` | Rename to `MergeState` |
| `OtelLogEvent`/`OtelMetricEvent`/`OtelSpanEvent` aliases | `event/mod.rs:45-47` | Delete (0 callers) |
| `try_into_log_coerce`/`into_log_coerce` | `event/mod.rs:161-166` (17 callers) | Rename to `try_into_log`/`into_log` (already exist, these are duplicates) |
| ~~`log_event!` macro~~ | `test_util/mod.rs:84` | **DONE** — renamed to `otel_event!` |
| `event.proto` deprecated fields | `proto/event.proto` | Remove deprecated `metadata` + `fields` field declarations |

**Effort:** Low — mechanical renames and dead code deletion.

---

### Complete dependency graph (all tasks)

```
DONE — All legacy types deleted:
  T1..T14, T18..T22
  F.6 (LogEvent), G.1 (TraceEvent), G.2 (proto compat)
  T12 + T13b (Metric struct DELETED — 9bd0d06)  ←── MILESTONE ✓

DONE — Fast-path mutation optimization:
  T16 (get/insert/remove bypass legacy layout for all-field paths) — DONE

DONE — Legacy layout elimination:
  T15 (remove VRL aliases + switch Serialize/value/keys to proto-canonical) — DONE
  T23 (delete to_value_legacy_layout, rename apply_value_map, DRY helpers) — DONE

CLOSED:
  T17 (serde Deserialize) — REVERTED, not needed
  T24 (OTLP JSON parse) — NOT NEEDED, already covered by proto serde + from_parts
```

---

#### T19 — Fix VrlTarget::OtelMetric remove (no-op on proto)

**File:** `lib/vector-core/src/event/vrl_target.rs:914-920`

`target_remove` on an OtelMetric modifies the VRL projection `value`
but never writes back to the underlying proto `event`. A VRL program
doing `del(.name)` on a metric appears to succeed but the actual
event is unchanged. Also always returns `None` instead of the removed
value.

Design a write-back mechanism similar to `apply_value_legacy_layout`
for OtelMetric, or at minimum return an error/warning when mutation
is attempted on an OtelMetric field.

**Effort:** Medium — needs design decision on metric mutability in VRL.

---

### Trade-offs and architectural decisions

**TD-1: RESOLVED — Metric struct will be deleted (T3-T13)**

Previously: "kept as internal normalizer type." User decision:
OtelMetric can replace Metric everywhere. The normalizer trait
will take/return OtelMetric. Metric struct will be deleted entirely.

**TD-2: RESOLVED — `FromLua for Metric` will be deleted (T13)**

Breaking change accepted. VRL migration tool handles the use case.
`FromLua for Metric` will be deleted when the struct is deleted.

**TD-3: RESOLVED — proto is canonical, JSON is codec**

Serialize for OtelLog stays as legacy layout until T16 covers all
paths. Each sink chooses its codec format. `OtlpJsonLog` wrapper
exists for OTLP-aware sinks. VRL aliases will be removed when
the round-trip is fully eliminated (T23).

**TD-4: RESOLVED (T17 reverted)**

Deserialize implementations are NOT needed — zero callers exist.
T17 was reverted (`070e536`) after discovering the impl silently
dropped OtelMetric's `data` field. See T24 for trigger-based plan.

**TD-5: RESOLVED (T19 done)**

VrlTarget metric remove has write-back for all mutable paths.

### Types that STAY after all tasks complete

These are OtelMetric's public API — NOT legacy types:

- `MetricKind` (Absolute/Incremental enum)
- `MetricValue` (Counter/Gauge/Distribution/Set/Histogram/Summary)
- `MetricTags`, `TagValue`, `TagValueSet`
- `Bucket`, `Quantile`, `Sample`, `StatisticKind`
- `MetricSeries`, `MetricName`, `MetricData`, `MetricTime`

`OtelMetric::into_metric_parts()` returns these; `with_tags`,
`with_namespace`, `with_timestamp` consume them.

### Completed work log (2026-04-20 → 2026-04-21)

All legacy types deleted (LogEvent, TraceEvent, Metric). Legacy layout
elimination complete — the O(event_size) round-trip for get/insert/remove
has been replaced by direct proto fast-path accessors. All 4 tasks done:

#### T16 — Eliminate legacy-layout fallbacks in get/insert/remove

**Goal:** Make OtelLog and OtelSpan get/insert/remove operate directly
on proto fields, never calling the legacy layout round-trip.

**File:** `lib/vector-core/src/event/otel_event.rs`

**Current state (Phase 1 DONE):** OtelLog single-segment fast-path fully
covers get/insert/remove for ALL single-segment paths — proto fields
(body, severity_text, severity_number, trace_id, span_id, time_unix_nano,
observed_time_unix_nano), aliases (message, timestamp, host, source_type),
and generic attribute/KvList-body lookup. Single-segment paths never fall
through to legacy layout. Multi-segment paths still fall through.

**Production path audit (2026-04-20):**
- **Single-segment** `.message` (112 uses via log_schema): alias for
  `.body` — NOT handled by fast-path, falls through.
- **Multi-segment** `kubernetes.*` (24 uses), `data_stream.*` (9 uses),
  `counter.value`/`gauge.value` (10 uses), `origin.*` (3 uses): all
  stored as record attributes with dotted keys; the legacy layout
  inserts them flat into the ObjectMap so VRL's multi-segment path
  navigator finds them by splitting the dotted key.
- **Batch methods** `modify_as_value` (3 production callers: splunk_hec,
  dnstap), `as_map` (8 production callers: elasticsearch, new_relic,
  log_to_metric, enrichment tables).
- **OtelSpan**: no fast-path at all (3 methods call legacy layout).

**Phase 1 — Close single-segment gaps — DONE:**
- Added `nanos_to_timestamp` helper
- Extracted `get_single_segment`/`insert_single_segment`/`remove_single_segment`
- All known proto fields + aliases + generic attributes handled
- Multi-segment paths fall through to legacy layout (Phase 2)

After Phase 1: **single-segment paths never fall through.** ✓

**Phase 2 — Multi-segment proto navigation — DONE:**
- Added `get_field_path`/`insert_field_path`/`remove_field_path`
- Added `navigate_value`/`insert_value_at`/`remove_value_at` helpers
- Routes by first segment:
  - `"resource"` → resource attributes (hoisted fields return None)
  - `"scope"` → scope proto fields (name, version, attributes)
  - anything else → nested KvList attribute/body navigation
- 3+ segment paths supported (e.g., `["kubernetes", "pod_labels", "app"]`)
- Prune support for empty parent cleanup
- Index segments (array access) still fall through to legacy layout

After Phase 2: **all-field multi-segment paths use direct proto access.** ✓

**Phase 3 — OtelSpan fast-path — DONE:**
- Added `span_get_single_segment`/`span_insert_single_segment` for all
  proto fields: name, trace_id, span_id, parent_span_id, start_time,
  end_time, kind, status (with nested code/message), plus legacy aliases
  (source_type → resource, host → resource.host.name)
- Added `span_get_field_path`/`span_insert_field_path` for multi-segment:
  resource, scope, status sub-fields, nested attributes
- Single-segment + multi-segment fast-path, index segments fall through

After Phase 3: **OtelSpan get/insert bypass legacy layout for all-field paths.** ✓

**Phase 4 — Batch methods — DONE (T15/T23):**
- `value()`, `keys()`, `as_map()`, `convert_to_fields*()` switched to
  `to_value_canonical()` — proto-canonical field names throughout.
- `event_data_eq()` uses `normalize_for_eq` for storage-independent
  comparison (hoists source_type/host.name from resource, strips
  observed_time_unix_nano).
- `modify_as_value()` callers (dnstap, splunk_hec) use `to_value_canonical`
  → mutate → `apply_value_map` round-trip for bulk parser APIs.

T16 impact: **get/insert/remove (the hot mutation paths) fully bypass
legacy layout for all-field paths.** Read-only snapshot methods switched
to `to_value_canonical()` in T15/T23.

**Effort:** ~2 sessions (8-12h). No breaking changes.
**Depends on:** nothing.

---

#### T15 — Remove VRL aliases + switch Serialize to proto-canonical

**Goal:** Remove the legacy field name aliases that exist solely for
backward compatibility with pre-OTLP Vector. Users run
`vector vrl-migrate` (Phase A, DONE) to update their configs first.

**Breaking change:** YES. Gated on `vector vrl-migrate` availability
(already shipped).

**Aliases to remove:**

| Alias | Canonical | Action |
|-------|-----------|--------|
| `.message` | `.body` | Remove from fast-path; delete `message_path()` alias |
| `.timestamp` | `.time_unix_nano` | Remove from fast-path; change type from Timestamp to Integer (nanos) |
| `.host` | `.resource.attributes."host.name"` | Remove hoisting from `hoist_resource_fields` |
| `.source_type` | `.resource.attributes."source_type"` | Remove hoisting from `hoist_resource_fields` |
| `.tags."key"` | `.attributes."key"` | OtelMetric only — remove alias in VrlTarget |

**Phase 1 — Remove aliases from get/insert/remove — DONE:**
- Removed `.message` alias from get/insert/remove (only `.body` works)
- Removed `.timestamp` → replaced with `.time_unix_nano` (Integer nanos)
  and `.observed_time_unix_nano` as direct proto field accessors
- Removed `.host` and `.source_type` top-level alias cases
- Removed `source_type`/`host.name` guards from `get_field_path` and
  `span_get_field_path` — these resource attributes are now accessible
  at their canonical `resource.*` paths
- Removed KvList body key expansion to top-level in get/remove fast-path
- Deleted `get_timestamp_value()` helper (no callers)
- Updated `timestamp_path()` to return `time_unix_nano`
- `host_path()` and `source_type_path()` now return `None`
- `message_path()` kept as alias for `body_path()` (3 callers)

**Phase 2 — Switch Serialize to proto-canonical — DONE:**
- OtelLog `Serialize` now delegates to `OtlpJsonLog` (OTLP JSON spec)
- OtelSpan `Serialize` now delegates to `OtlpJsonSpan`
- JSON output now uses proto3 field names: `body` (AnyValue-wrapped),
  `severityText`, `timeUnixNano`, `resource.attributes` array, etc.

**Phase 3 — Update value()/keys()/as_map() — DONE:**
- Added `to_value_canonical()` for OtelLog and OtelSpan
- `value()`, `keys()`, `as_map()`, `convert_to_fields()`,
  `convert_to_fields_unquoted()`, `value_mut()` all switched from
  `to_value_legacy_layout()` to `to_value_canonical()`
- Proto-canonical field names: `body`, `time_unix_nano`,
  `severity_text`, `resource.source_type`, `resource.host.name`, etc.

**Phase 4 — Fix test assertions — DONE (`940da7e`):**
- 78 test failures fixed across 23 files
- All assertions updated to canonical field names (body, time_unix_nano,
  resource.host.name, record attribute source_type)
- Fixed 3 core accessor bugs: `get_source_type()`, `remove_timestamp()`,
  `source_type_path()`
- Fixed k8s parser `transform_otel_event` to copy `time_unix_nano` from proto
- 1791 tests pass, 0 failures

**Phase 5 — Delete alias infrastructure — DONE:**
- Deleted `hoist_resource_fields` / `hoist_scope_fields` helper functions
- Deleted `message_path()` alias (callers → `body_path()`) and `host_path()`
- `to_value_legacy_layout` deleted — all callers switched to `to_value_canonical`
- `apply_value_legacy_layout` renamed to `apply_value_map`; restores
  resource/scope sub-objects via shared `restore_resource`/`restore_scope`
  helpers; handles `time_unix_nano`/`observed_time_unix_nano` as Integer;
  keeps "message"/"timestamp" fallbacks for `from_value_map` decoder compat
- `event_data_eq` uses `normalize_for_eq` for storage-independent comparison
- Removed dead `host_key` field from InfluxDB logs sink
- `log_event!` macro renamed to `otel_event!` (392 sites)

**Effort:** ~1-2 sessions (6-10h), mainly test assertion updates.
**Depends on:** T16 complete (fast-path covers all paths).

---

#### T23 — Simplify/rename legacy layout functions — DONE

**Completed actions:**
1. `to_value_legacy_layout` deleted — all 22 callers (20 internal +
   2 in proto.rs) switched to `to_value_canonical`
2. `apply_value_legacy_layout` renamed to `apply_value_map`
3. Resource/scope restoration extracted into shared `restore_resource` /
   `restore_scope` helpers (DRY — eliminated 40 lines of duplication)
4. `hoist_resource_fields` / `hoist_scope_fields` deleted (T15 Phase 5)
5. `coerce_to_timestamp` kept — still used by `get_timestamp()` for
   RFC3339 string→Timestamp conversion at I/O boundaries
6. `log_event!` macro renamed to `otel_event!` (392 sites across 4 files)
7. Stale comments referencing old function names updated across 5 files

---

#### T24 — OTLP JSON parse helpers — NOT NEEDED (closed)

The OTLP JSON decode path already exists in the opentelemetry source
(`src/sources/opentelemetry/http.rs`):

```rust
// json_decode_metrics (line 369):
let request: ExportMetricsServiceRequest = serde_json::from_slice(&body)?;
// → OtelMetric::from_parts(metric_proto, resource, scope, metadata)

// json_decode_logs (line 343):
let request: ExportLogsServiceRequest = serde_json::from_slice(&body)?;
// → OtelLog::from_parts(record, resource, scope, metadata)

// json_decode_traces (line 395):
let request: ExportTraceServiceRequest = serde_json::from_slice(&body)?;
// → OtelSpan::from_parts(span, resource, scope, metadata)
```

The prost-generated `#[derive(serde::Deserialize)]` on the proto types
handles the proto3 JSON mapping. Combined with the `from_parts`
constructors on OtelLog/OtelMetric/OtelSpan, this is the complete
OTLP JSON → internal type path. No additional `otel_json.rs` parse
helpers are needed.

T17 (serde Deserialize directly on Vector types) was correctly reverted
because it duplicated what prost already provides and silently dropped
the OtelMetric `data` field. The right architecture is:
**JSON bytes → proto serde → proto types → from_parts → OTel types.**

## Historical completion log

1. ~~Delete dead `VrlTarget` legacy variants~~ — **DONE** (`2e1b80e`, −492 lines)
2. ~~Fix OTLP buffer codec to use `into_otel_event_iter`~~ — **DONE** (`2bd9027`)
3. ~~Migrate codecs (Group B)~~ — **DONE**: all 5 decoders (avro, protobuf, syslog, gelf, vrl)
4. ~~Unblock source migrations via `MetadataInsertable`~~ — **DONE** (`af17230`)
5. ~~Migrate sources (Group C)~~ — **8/14 done** (heroku_logs, fluent, journald, splunk_hec, docker_logs, kubernetes_logs × 3 annotators). dnstap remains (= B1).
6. ~~Migrate transforms (Group E)~~ — **DONE**: reduce + metric_to_log
7. ~~Performance: per-insert round-trips~~ — **DONE** via `OtelLog::modify_as_value`, applied to splunk_hec `build_log_legacy`

## Current state (2026-04-21)

### ALL legacy types and layout eliminated
- **`LogEvent`** — DELETED (`80ff2fb`, 1217 lines)
- **`TraceEvent`** — DELETED (`1236e8e`, 191 lines)
- **`Metric` struct** — DELETED (`9bd0d06`, 1040 lines across 21 files)
- **Proto backward compat** — DELETED (`9363568`, old decoders/encoders)
- **All `Event::from(LogEvent/Metric/TraceEvent)` bridges** — DELETED
- **All `from_legacy_metric`/`from_log_event`/`from_trace_event`** — DELETED
- **`to_value_legacy_layout`** — DELETED (all callers → `to_value_canonical`)
- **`apply_value_legacy_layout`** — renamed to `apply_value_map`
- **`hoist_resource_fields`/`hoist_scope_fields`** — DELETED
- **`message_path()`/`host_path()`** — DELETED
- **`log_event!` macro** — renamed to `otel_event!`

### What remains: user-facing VRL alias removal (Phase B)
The only remaining legacy pattern is that VRL field aliases (`.message`,
`.host`, `.source_type`, `.timestamp`) are still accepted by the
fast-path get/insert/remove code. This is a **user-facing breaking
change** gated on the product decision to ship `vector vrl-migrate`.
The migration tool is implemented (B4 DONE). When the decision is made,
removing the aliases is ~2h of mechanical work.

### Performance findings

**Per-insert round-trip cost (discovered 2026-04-14, FIXED by T16)**

Before T16, `OtelLog::insert(event_path!(...), value)` was
**O(size of event)** — a full `to_value_canonical()` → mutate →
`apply_value_map()` round-trip per call. T16 replaced this with
direct proto fast-path accessors for single-segment and multi-segment
paths. Only array-index paths still fall through to the round-trip.

Historical impact before the fix:

| Site | Inserts / event | Throughput impact |
|------|------------------|-------------------|
| `lib/codecs/src/decoding/format/syslog.rs` | 8–12 | 3.5% event loss at 10k msg/s (FIXED) |
| `src/sources/splunk_hec/mod.rs` `build_log_legacy` | ~N record fields | **FIXED** via `modify_as_value` — N → 1 round-trip |
| `src/sources/kafka.rs` | 1 | Negligible |

**k8s annotators re-audit (2026-04-15)**: the production k8s annotators
(`pod_metadata_annotator`, `namespace_metadata_annotator`,
`node_metadata_annotator`) use `OtelLog::set_resource_attribute` /
`set_attribute`, which are **O(1) amortized direct proto mutations**
— no round-trip. The earlier "up to 48 inserts" count was inflated by
test code. Only the test blocks use `insert(event_path!(...))` for
expected-value construction. No migration needed.

**Mitigations**

1. **"Build-once" pattern** for initial construction: build a full
   `ObjectMap` in one pass, then `OtelLog::from_value_map(...)` once.
   Used by syslog + gelf decoders.
2. **`OtelLog::modify_as_value(|v| { ... })`** for bulk mutation of
   existing events. Amortizes the round-trip across N mutations in
   the closure. Regression test: `modify_as_value_equivalent_to_multiple_inserts`.
3. **Prefer direct proto APIs** (`set_attribute`, `set_resource_attribute`,
   `record_mut().time_unix_nano`, etc.) when the semantic mutation
   targets a specific proto field. These are O(1) and bypass the
   legacy layout entirely.

## Verification (updated 2026-04-21)

After post-migration cleanup:
- Full test suite: **1791 pass, 0 failures**
- `to_value_legacy_layout` deleted — all callers use `to_value_canonical`
- `apply_value_legacy_layout` renamed to `apply_value_map`
- `log_event!` macro renamed to `otel_event!` (392 sites)
- DRY: `restore_resource`/`restore_scope` shared helpers extracted
- Resource/scope round-trip tests added (scope, observed_time_unix_nano)

## Related docs

- `BRIDGE_REMOVAL_SESSION.md` — per-session log of bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — path model for OTel-native VRL targets
- `VRL_MIGRATION_TOOL.md` — Phase A spec
- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
