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

- **A — VRL migration tool rules**: Rewrite rules to transform user VRL
  programs. Blocked by aliases still being live. See `VRL_MIGRATION_TOOL.md`.
- **B — Remove VRL aliases**: Once Phase A ships, remove `.message`,
  `.timestamp`, etc. Expect ~15-25 test breakages.
- **C — OtelLog/OtelSpan/OtelMetric OTel-to-Legacy bridge**: **DONE**
- **D — OtelMetric Legacy-to-OTel bridge** (`from_legacy_metric`):
  Requires migrating ~5 production sites to native construction. Low
  priority — `from_legacy_metric` is useful for test ergonomics.
- **E — Remove legacy types**: Requires:
  - `VrlTarget::Log` → `VrlTarget::OtelLog` (deferred, complex VRL work)
  - Disk-buffer proto (`lib/vector-core/src/event/proto.rs`) to stop
    producing intermediate LogEvent/TraceEvent (currently does
    `LogEvent::from(proto_log) → OtelLog::from_value_map`)
  - 5 production `from_log_event` callers → `from_value_map`
  - `TraceEvent` is a newtype over `LogEvent`; drops for free with
    LogEvent
  - Metric cannot be removed until `MetricSet/BatchedMetrics/MetricRef`
    go native OtelMetric

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

## Next concrete targets (ordered)

1. **VRL migration tool MVP** (Phase A) — at least `LOG-01`/`MET-06`/`MET-07`
   rules compile and rewrite real programs. **This is the single blocking
   item for further bridge removal.**
2. **`Metric::from_parts`/`into_parts` normalizer path** — `MetricSet`,
   `BatchedMetrics`, `MetricRef` still use legacy `Metric` as the
   normalization key. Migrating to a native OtelMetric `MetricSet` would
   remove the biggest remaining legacy footprint in the hot path.
3. **`VrlTarget::Log(LogEvent)` → `VrlTarget::OtelLog`** — largest
   single change, gated on Phase A + stable OtelLog VRL semantics.
4. **Audit remaining `LogEvent` use sites in sources/codecs**
   — ~14 files import `LogEvent`. Each is a candidate to decode straight
   into `OtelLog` via `from_value_map`.

## Verification

- `cargo test -p vector --lib` — 1789/1789 pass after write-back fix
- `cargo test -p vector-core --lib event::otel_event` — 34/34 pass,
  0 ignored (all previously-ignored gap tests resolved)
- `cargo test -p vector --lib sinks::` — 551/551 pass
- `cargo check -p vector` — compiles clean

## Related docs

- `BRIDGE_REMOVAL_SESSION.md` — per-session log of bridge elimination
- `VRL_OTEL_NATIVE_TARGETS.md` — path model for OTel-native VRL targets
- `VRL_MIGRATION_TOOL.md` — Phase A spec
- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0–7)
