# Plan: Remove Legacy Backward Compat + Update VRL Migration Tool

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
access goes through `to_value_legacy_layout` (read) / proto accessors.

**Phase E gap (write-back symmetry) — FIXED 2026-04-13**

`apply_value_legacy_layout` now extracts native proto fields symmetrically
with `to_value_legacy_layout`:

- **OtelLog**: `severity_text`, `severity_number`, `trace_id` (hex →
  bytes), `span_id` (hex → bytes), plus existing `body`/`message`,
  `timestamp`, `source_type`/`host` → resource
- **OtelSpan**: `name`, `trace_id`, `span_id`, `parent_span_id`,
  `start_time`, `end_time`, `kind`, `status` (with nested `code`/`message`)

Malformed hex IDs fall back to attribute storage rather than dropping
data. `from_log_event` and `from_trace_event` now delegate to
`apply_value_legacy_layout`, so both round-trip paths share one
field-routing implementation.

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
| `OtelMetric::from_legacy_metric(Metric)` | ~72 | ~67 in test modules; `Event::from(Metric)` impl; `prometheus remote_write` (TODO, blocked on BatchedMetrics migration) |

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

### Legacy types — still defined, usage shrinking

| Type | Defined | Imports (code+tests) | Status |
|------|---------|----------------------|--------|
| `LogEvent` | `lib/vector-core/src/event/log_event.rs` | 19 files | Used for I/O boundary + VRL target mutation |
| `Metric` | `lib/vector-core/src/event/metric/mod.rs:58` | many | `Metric::from_parts`/`into_parts` is the proto ↔ normalizer bridge |
| `TraceEvent` | `lib/vector-core/src/event/trace.rs:19` (newtype over LogEvent) | 2 files | Nearly-dead, only kept for disk buffer compat |
| `prometheus_parser::Metric` | `lib/prometheus-parser/src/line.rs:99` | Prometheus parser only | Unrelated to legacy `vector_core::Metric` |

`LogEvent` is the widest-remaining legacy type. Removal requires:
1. Replacing `VrlTarget::Log(LogEvent)` with direct `OtelLog` mutation
   (VRL_OTEL_NATIVE_TARGETS.md Phase 2 — deferred, large VRL change)
2. Removing the `From<LogEvent>` / `From<TraceEvent>` / `From<Metric>`
   bridge impls — blocked on all sources/transforms/codecs that still
   emit legacy types being migrated to direct OTel construction
3. Deleting `lua/log.rs`, `lua/event.rs` Lua adapters (only non-proto
   places that still construct `LogEvent` directly)

### VRL aliases / paths

| Alias | Target | Still live |
|-------|--------|-----------|
| `.message` | `.body` | Yes — deprecation notice only |
| `.timestamp` | `.time_unix_nano` | Yes — schema-meaning mapping |
| `.tags."key"` | `.attributes."key"` | Yes (OtelLog); OtelMetric `.tags` also live |
| `.host` | `.resource.attributes."host.name"` | Yes — hoisted by `to_value_legacy_layout` |
| `.source_type` | `.resource.attributes."source_type"` | Yes — hoisted |
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

- **A — VRL migration tool rules**: **DONE** (B4) — `src/vrl_migrate/`
  ships 22 rules across 3 passes, CLI wired into `vector vrl-migrate`,
  29/29 tests green. See `VRL_MIGRATION_TOOL.md`.
- **B — Remove VRL aliases**: Pending product decision. With Phase A
  shipped, `.message`/`.timestamp`/`.tags`/etc. aliases on OtelLog VRL
  paths can be removed. Expect ~15-25 test breakages — users would
  pre-run `vector vrl-migrate` on their configs.
- **C — OtelLog/OtelSpan/OtelMetric OTel-to-Legacy bridge**: **DONE**
- **D — OtelMetric Legacy-to-OTel bridge** (`from_legacy_metric`):
  Folded into Phase F.
- **E — Remove legacy types from production**: **DONE** — only one
  true production caller of `LogEvent` remained
  (`http_client/client.rs:464`), the rest were tests. All blockers B1–B6
  resolved.
- **F — Delete legacy types entirely** (LogEvent, TraceEvent, Metric):
  see dedicated section below.

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

**F.1 — One real production caller (15 min)**
- Migrate `http_client/client.rs:464` to construct `OtelLog` directly
  for the empty VRL target.

**F.2 — `Discriminant::from_otel_log` (~2 hours)**
- Add `Discriminant::from_otel_log(&OtelLog, &[impl AsRef<str>])`.
- Migrate `transforms/aggregate`, `transforms/dedupe`,
  `transforms/reduce` callers to use it where they hold OtelLog.
- Keep `from_log_event` for tests that still use `LogEvent`.

**F.3 — Extend OtelMetric constructors (~half day)**
- Add `OtelMetric::new_histogram`, `new_summary`, `new_distribution`,
  `new_set`. Cover the 5 `MetricValue` variants currently only
  reachable via `from_legacy_metric`.
- Add tests verifying parity with `from_legacy_metric` for each.

**F.4 — Delete `Event::from(TraceEvent)` (~30 min)**
- Smallest of the three bridges. 1 test caller.
- Migrate the 2 `TraceEvent::from` test sites to `OtelSpan::from_value_map`.
- Delete `TraceEvent::from_*` impls + `Event::from(TraceEvent)`.
- Delete `lib/vector-core/src/event/trace.rs` (192 lines).

**F.5 — Delete `Event::from(Metric)` (~half day)**
- 18 test callers. Each one does `Event::from(Metric::new(...))`.
- Replace with `Event::Metric(OtelMetric::new_<variant>(...))`.
- Delete `Event::from(Metric)` impl.
- Delete `OtelMetric::from_legacy_metric`.
- Delete `Metric::new` and its 390 test convenience callers (mechanical
  but high volume — split across multiple sessions or apply via macro
  refactor).
- Delete `lib/vector-core/src/event/metric/mod.rs` `Metric` struct
  + impls. Keep the primitive sub-types.

**F.6 — Delete `Event::from(LogEvent)` (~1-2 days)**
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

### Group H — Bridges (permanent)

| Function | Role |
|----------|------|
| `Event::from(LogEvent / Metric / TraceEvent)` | Fundamental coercion — stays until all sources emit OTel directly |
| `OtelLog::from_log_event` | Thin wrapper over `from_value_map`; useful for test ergonomics |
| `OtelSpan::from_trace_event` | Same as above |
| `OtelMetric::from_legacy_metric` | Same as above |

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
| B3 | ~~`BatchedMetrics` + `MetricSet`~~ | `src/sinks/prometheus/remote_write/sink.rs`, `src/sinks/util/buffer/metrics/normalize.rs` | **DONE — declared permanent** (`58f917c`): the `_otel` methods are now documented as the permanent compatibility layer between OTel-native event arrays and the legacy `MetricSet`. Audit found the prometheus wire format is closely tied to `MetricValue`'s variant structure; a proto-native `MetricSet` would not simplify code. | ~~MEDIUM~~ **DONE** | — |
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

**B3 — `BatchedMetrics` + `MetricSet`** (MEDIUM)
- Three viable options:
  1. Make `BatchedMetrics` generic over `M: Into<Metric>` — narrow,
     low-risk. ~4 hours.
  2. Full native-OtelMetric `MetricSet` — large refactor, needs
     `METRICSINK_PIPELINE_REFACTOR.md` design doc first. ~1 week.
  3. Accept `_otel` wrappers as permanent and stop calling them "shims".
     Zero code change, honest documentation.

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

## Historical completion log

1. ~~Delete dead `VrlTarget` legacy variants~~ — **DONE** (`2e1b80e`, −492 lines)
2. ~~Fix OTLP buffer codec to use `into_otel_event_iter`~~ — **DONE** (`2bd9027`)
3. ~~Migrate codecs (Group B)~~ — **DONE**: all 5 decoders (avro, protobuf, syslog, gelf, vrl)
4. ~~Unblock source migrations via `MetadataInsertable`~~ — **DONE** (`af17230`)
5. ~~Migrate sources (Group C)~~ — **8/14 done** (heroku_logs, fluent, journald, splunk_hec, docker_logs, kubernetes_logs × 3 annotators). dnstap remains (= B1).
6. ~~Migrate transforms (Group E)~~ — **DONE**: reduce + metric_to_log
7. ~~Performance: per-insert round-trips~~ — **DONE** via `OtelLog::modify_as_value`, applied to splunk_hec `build_log_legacy`

## Current state (2026-04-14)

### `from_log_event` callers
- **1 production**: `Event::from(LogEvent)` bridge impl — permanent
- **~18 test sites** — test convenience, acceptable

### `from_legacy_metric` callers
- **2 production**: `Event::from(Metric)` bridge + prometheus remote_write TODO
- **~67 test sites** — test convenience

### `from_trace_event` callers
- **1 production**: `Event::from(TraceEvent)` bridge impl — permanent
- **0 test sites** outside the bridge

### Dead code deleted this session
- `VrlTarget::LogEvent/Trace/Metric` variants (−492 lines)
- `TargetIter<LogEvent>/<TraceEvent>`, `create_log_event`, `set_metric_tag_values`
- `precompute_metric_value`, `target_get_metric`, `target_get_mut_metric`
- `VALID_METRIC_PATHS_SET/GET`, `MAX_METRIC_PATH_DEPTH`
- `lua/log.rs` (−80 lines)
- `LogNamespace::new_log_from_data` (0 callers)
- `traces_to_export` + `trace_event_to_resource_spans` in buffer_codec
- k8s docker parser `parse_json(&mut LogEvent)` + `normalize_event` (−125 lines)

### New OtelLog helpers added this session
- `OtelLog::merge()` — field-level byte concatenation (unblocks LogEventMergeState)
- `OtelLog::maybe_insert()` — convenience mirroring LogEvent::maybe_insert
- `OtelLog::from_value_map()` — preferred entry point for constructing from Value tree
- `MetadataInsertable` trait — makes `LogNamespace::insert_source_metadata`
  and `insert_vector_metadata` generic over LogEvent and OtelLog

### Performance findings

**Per-insert round-trip cost (discovered 2026-04-14 via syslog test failure)**

`OtelLog::insert(event_path!(...), value)` is **O(size of event)**:
1. Calls `to_value_legacy_layout()` — clones the entire event into a
   flat `Value` tree (with field routing).
2. Calls `Value::insert()` on the flat tree.
3. Calls `apply_value_legacy_layout()` — reparses the flat tree back
   into proto `LogRecord` + `Resource` + `Scope` + `EventMetadata`.

Each insert does at least 1 full round-trip. `LogEvent::insert` is
O(depth of path). In hot paths this matters:

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

## Verification (updated 2026-04-14)

- `cargo test -p vector --lib` — 1789/1789 pass
- `cargo test -p vector-core --lib event::otel_event` — 35/35 pass, 0 ignored
- `cargo test -p codecs --lib` — 171/171 pass
- `cargo test -p vector-opentelemetry-proto --lib` — 22/22 pass
- `cargo test -p vector --lib sinks::` — 551/551 pass
- `cargo check -p vector` — compiles clean

## Related docs

- `BRIDGE_REMOVAL_SESSION.md` — per-session log of bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — path model for OTel-native VRL targets
- `VRL_MIGRATION_TOOL.md` — Phase A spec
- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
