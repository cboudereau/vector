# Sink Removal Strategy: Remove First vs Keep Throughout

This document analyses the two strategies for handling the DataDog and Vector sinks during the
OTel core migration, and gives a concrete recommendation based on the actual coupling measured
in the codebase.

---

## 1. What the Sinks Actually Couple To

Before comparing strategies, we need to know what types the sinks hold onto in the core.

### 1.1 Vector sink

The vector sink (`src/sinks/vector/`, ~530 lines total) couples exclusively to:

- `EventWrapper` (the native protobuf oneof — `event.proto`)
- `proto::EventArray` (the native protobuf batch type)
- `proto_vector::PushEventsRequest` (the gRPC request type)
- Standard infrastructure types (`BatcherSettings`, `TowerRequestConfig`, `Service` trait)

It has **zero direct references** to `MetricValue`, `MetricSketch`, `AgentDDSketch`, or any
DataDog-specific metadata. Its only coupling to the core event model is through `EventWrapper`,
which is the proto serialization of `Event`. When `Event` becomes an OTel type, `EventWrapper`
either disappears or becomes `OtlpBufferBatch` — and the vector sink disappears with it.

### 1.2 DataDog sinks

The DataDog sinks have two distinct coupling layers:

**Layer A — per-event routing metadata** (present in every DD sink):

```
datadog_api_key  ←  EventMetadata::datadog_api_key()
```

Used in `logs/sink.rs`, `metrics/sink.rs`, `events/request_builder.rs`, `traces/sink.rs`.
This is a field on `EventMetadata` in `lib/vector-core/src/event/metadata.rs` (611 lines).

**Layer B — DataDog-specific metric types** (metrics sink only):

```
AgentDDSketch     ←  lib/vector-core/src/metrics/ddsketch.rs  (1,637 lines)
MetricSketch      ←  lib/vector-core/src/event/metric/value.rs
MetricValue::Sketch { sketch: MetricSketch::AgentDDSketch }
DatadogMetricOriginMetadata  ←  lib/vector-core/src/event/metadata.rs
```

Used in `metrics/encoder.rs` (1,792 lines), `metrics/normalizer.rs` (327 lines),
`traces/apm_stats/bucket.rs` (191 lines), `traces/apm_stats/mod.rs` (123 lines).

**Layer B also bleeds into non-DD sinks:**

Prometheus, InfluxDB, and GreptimeDB sinks all handle `MetricValue::Sketch` with a
`MetricSketch::AgentDDSketch` match arm, because `Sketch` is a variant of `MetricValue` itself —
it is part of the core type. Removing `AgentDDSketch` from `MetricValue` forces changes in
**every** sink that does a complete match on `MetricValue`.

### 1.3 The core entanglement map

```
lib/vector-core/src/event/metric/value.rs
  └── MetricValue::Sketch { sketch: MetricSketch }     ← variant in the core enum
        └── MetricSketch::AgentDDSketch(AgentDDSketch) ← DD type embedded in core

lib/vector-core/src/metrics/ddsketch.rs (1,637 lines)  ← DD algorithm in core

lib/vector-core/src/event/metadata.rs (611 lines)
  ├── datadog_api_key: Option<Arc<str>>                ← DD routing in core metadata
  └── DatadogMetricOriginMetadata                      ← DD origin in core metadata

lib/vector-core/src/event/proto.rs
  └── AgentDdSketch in event.proto serialization       ← DD type in native wire format
```

Consumers of Layer B (the DD-specific metric types):

| File | Role | Can remove with sinks? |
|---|---|---|
| `sinks/datadog/metrics/encoder.rs` (1,792) | DD metrics encoding | Yes — removed with sink |
| `sinks/datadog/metrics/normalizer.rs` (327) | DD metrics normalization | Yes |
| `sinks/datadog/traces/apm_stats/` (314) | DD APM stats | Yes |
| `sinks/prometheus/collector.rs` | `Sketch` match arm | No — must handle the variant removal |
| `sinks/influxdb/metrics.rs` | `Sketch` match arm | No |
| `sinks/greptimedb/metrics/request_builder.rs` | `Sketch` match arm | No |
| `transforms/log_to_metric.rs` | (none — no Sketch arm) | N/A |
| `sources/datadog_agent/metrics.rs` | Creates `AgentDDSketch` | No — source kept; needs OTel output |

---

## 2. Strategy A: Remove Sinks First

Remove all DataDog and Vector sinks at the start of the migration, before touching the core
event model.

### What gets deleted immediately

```
src/sinks/vector/           (~530 lines)
src/sinks/datadog/          (~6,040 lines including tests)
```

### What does NOT get deleted (still blocked)

Deleting the sinks does not unblock the core type cleanup:

- `MetricValue::Sketch` variant remains — it is defined in `vector-core`, not in the sinks.
- `AgentDDSketch` (1,637 lines) remains in `vector-core` because the DataDog **source** still
  produces it and the Prometheus/InfluxDB/GreptimeDB sinks still consume it.
- `DatadogMetricOriginMetadata` and `datadog_api_key` remain in `EventMetadata` because the
  DataDog **source** still writes them.
- `event.proto` still contains `AgentDdSketch` because the native proto serialization must
  continue to round-trip sketch data for the source → buffer → any-sink path.

So after sink removal, the compiler still fails to build unless Prometheus, InfluxDB, and
GreptimeDB sinks are also updated to remove their `MetricSketch::AgentDDSketch` match arms — or
the variant is kept temporarily and left as a dead match arm.

### Advantages

- Eliminates the largest block of proprietary code early (~6k lines).
- Developers working on the core model are not confused by DataDog sink code that references the
  old types.
- CI compile times shrink.
- Cleaner git history: one large deletion commit, then incremental core refactors.
- No need to maintain backward compatibility of the sinks during core churn.

### Disadvantages

- **Immediate user-facing breakage.** Anyone routing to a DataDog or Vector sink has no fallback
  from day one. This makes the migration a flag-day deployment for all users, not a gradual
  rollout.
- The core types (`MetricValue::Sketch`, `AgentDDSketch`, `datadog_api_key` on `EventMetadata`)
  still cannot be removed immediately. Sink deletion is necessary but not sufficient to clean the
  core — it only removes the largest consumers.
- Prometheus, InfluxDB, and GreptimeDB sinks require immediate updates to handle the removal of
  `MetricSketch::AgentDDSketch` from `MetricValue`, because Rust's exhaustive match forces every
  match arm to be updated at once.
- The DataDog source still creates `AgentDDSketch` metrics. After sink removal, those metrics
  flow to the OTel sink and need conversion — which the OTel metrics encoder currently does not
  fully support (`MetricValue::Sketch` → `ExponentialHistogram` is unimplemented).
  This means DataDog sketch metrics are silently dropped until Phase 5.

---

## 3. Strategy B: Keep Sinks Throughout, Adapt Incrementally

Keep all existing sinks operational throughout the migration, adapting them to the OTel core
as the core types change.

### What this means in practice

Each phase of the core migration adds an OTel-native path alongside the existing Vector-native
path, and the sinks are updated to consume the new path at the same time. Sinks are deleted only
after the core migration is complete and the OTel type is the sole representation.

### Advantages

- **No user-facing breakage at any intermediate phase.** The DataDog and Vector sinks continue
  to work throughout. Users can migrate on their own schedule.
- Enables incremental validation: each phase's sink output can be compared against pre-migration
  output to detect regressions.
- The Prometheus, InfluxDB, and GreptimeDB `MetricSketch::AgentDDSketch` match arms do not need
  to change until the sketch variant is actually removed — which happens in Phase 5 or later.
- No silent data loss: sketch metrics from the DataDog source continue to reach DD-compatible
  sinks.

### Disadvantages

- During the transition (Phases 1–4), the sinks must maintain compatibility with **two**
  representations of the same data: the old Vector-native types and the new OTel types. This
  creates dual-path logic in the sink layer.
- The DataDog sink's coupling to `AgentDDSketch`, `DatadogMetricOriginMetadata`, and
  `datadog_api_key` creates a dependency web that constrains the order in which core types can
  be refactored.
- `EventWrapper` and `proto::EventArray` cannot be deleted until the vector sink is deleted,
  which means the native proto types linger in the codebase.
- Harder to reason about: at any given phase, it's unclear whether the "real" path is the new
  OTel path or the legacy path.

---

## 4. Decision Matrix

| Question | Remove First (chosen) | Keep Throughout | Hybrid |
|---|---|---|---|
| Core type cleanup unblocked immediately? | **Yes** — no sink code constraining refactor order | No | Partially |
| Developer clarity during core refactor? | **High** — no legacy sink code in the way | Low | Medium |
| CI compile times during migration? | **Faster** — ~6.5k lines gone from day one | Unchanged | Marginally better |
| Git history clarity? | **Clean** — one deletion commit, then incremental core work | Messy interleaving | Balanced |
| `AgentDDSketch` core removal blocked by sinks? | No — source must adapt regardless | No | No |
| Hidden coupling surfaced early? | **Yes** — compiler forces all `MetricValue::Sketch` match arms immediately | No — deferred | Partially |
| DD/Vector sink re-integration studied separately? | **Yes** — clean OTel core as foundation first | Not applicable | No |
| Risk of dual-path confusion during transition? | None — only one path exists | High | Medium |

---

## 5. Adopted Recommendation: Remove Both Sinks First, Re-integrate Later

Both the Vector sink and the DataDog sinks are removed at the start of the migration. This is
**Strategy A** applied to both sinks simultaneously.

### Rationale

The previous hybrid approach (feature-flag DataDog sinks, remove Vector sink first) was designed
to avoid breaking users during migration. The revised intent is different:

> **Remove first to study the clean state. Re-integrate Vector and DataDog sinks properly on
> top of the OTel core once the core is stable — rather than carrying legacy coupling throughout
> the entire migration.**

This is the right call for two reasons:

1. **The sinks are the largest source of coupling noise.** While the DataDog sinks cannot
   remove `AgentDDSketch` from `vector-core` on their own (the source still produces it),
   their presence creates a web of DataDog-specific types that makes it harder to see what the
   clean OTel core actually needs. Removing them first makes the target state visible.

2. **Re-integration is easier from a clean foundation.** Once the core runs OTel types natively,
   a Vector sink adapter and a DataDog sink adapter are well-defined, narrow translation
   problems: OTel-native event → proprietary wire format. Without the legacy coupling, the
   re-integrated sinks will be significantly smaller and better-tested than the originals.

### What this means in practice

**Step 1 of the consolidated plan**: delete `src/sinks/vector/` and `src/sinks/datadog/` in
the same commit. The sources (`src/sources/vector/`, `src/sources/datadog_agent/`) are kept.

**Immediate consequences to address in the same step:**
- Prometheus, InfluxDB, and GreptimeDB sinks: remove the `MetricSketch::AgentDDSketch` match
  arms. Since `MetricValue::Sketch` is still in the core type at this point, these become
  unreachable match arms that must be handled (either panicking or converting to an approximate
  value). The correct handling is converting to `AggregatedHistogram` using the existing
  `AgentDDSketch::to_histogram()` method — the same conversion that the OTel metric encoder
  will implement in Step 2.
- The DataDog source still produces `MetricValue::Sketch` events. Those events now flow only
  to the OTel sink or to Prometheus/InfluxDB/GreptimeDB sinks via the approximate conversion.
  This is intentional: it surfaces exactly which downstream sinks and users depend on precise
  sketch semantics, before the core types are changed.

**What is NOT immediately possible** (still requires later steps):
- `AgentDDSketch` cannot be removed from `vector-core` yet — the DataDog source still creates
  it and the converted match arms in non-DD sinks still use it.
- `datadog_api_key` and `DatadogMetricOriginMetadata` cannot be removed from `EventMetadata`
  yet — the DataDog source still writes them.
- These are cleaned up in Step 3 (DataDog source adaptation), once the source is rewritten to
  emit OTel types directly.

### What is deferred: sink re-integration

After Step 5 (core OTel types stable), sink re-integration is studied and implemented in
Step 7. The key insight is that **the existing `lib/opentelemetry-proto/src/` code is the
complete specification** — it already contains every field mapping decision in the forward
direction (OTel proto → Vector internal types). The re-integrated sinks are the exact inverse.

**The forward code to invert (all already in the codebase):**
- `logs.rs` — `ResourceLogs → LogEvent`: maps body, severity, attributes, resource, scope,
  trace_id/span_id (hex-encoded), timestamps, flags.
- `metrics.rs` — `ResourceMetrics → MetricEvent`: maps Sum (monotonic/non-monotonic → Counter/
  Gauge), Gauge, Histogram, ExponentialHistogram (→ AggregatedHistogram, lossy), Summary;
  flattens resource/scope/data-point attributes into `MetricTags` with `resource.`/`scope.`
  prefix convention.
- `spans.rs` — `ResourceSpans → TraceEvent`: maps trace_id/span_id/parent_span_id (bytes →
  hex), timestamps (nanos → `Value::Timestamp`), kind, status, attributes, events, links.
- `common.rs` — `AnyValue → Value`: all 7 `PBValue` variants to `Value` variants; `kv_list`
  → `Value::Object`; bytes → hex string.

**Re-integration scope** (from reading the forward code):
- Add `From<Value> for AnyValue` (inverse of `common.rs`) — shared by all three signals.
- Add `From<LogEvent> for ExportLogsServiceRequest` (inverse of `logs.rs`).
- Add `From<MetricEvent> for ExportMetricsServiceRequest` (inverse of `metrics.rs`).
- Add `From<TraceEvent> for ExportTraceServiceRequest` (inverse of `spans.rs`).
- Estimated scope: ~443 lines, comparable to the forward implementation.
- No DataDog types, no `AgentDDSketch`, no `EventWrapper`. Clean, narrow, symmetric.

**Key gaps already visible in the forward code** (must handle in reverse):
- `ExponentialHistogram` was decoded to `AggregatedHistogram` (lossy). Reverse emits OTel
  `Histogram` with explicit bounds — lossless for what was preserved.
- `scope_logs`/`scope_spans` hierarchy was flattened. Reverse must reconstruct by grouping
  events with identical `scope.*` values into the same `ScopeLogs`/`ScopeSpans`.
- `ResourceLogs` resource grouping was flattened. Reverse groups by identical `resources`
  map value. Adding a `pipeline.resource_id` attribute in Step 3a makes this deterministic.
- `Value::Timestamp` has no `AnyValue` equivalent. Reverse encodes as nanosecond `IntValue`.

---

## 6. Step Order (Consolidated Plan Reference)

The full execution order with validation gates, decision points, and rollback procedures is
maintained in **`CONSOLIDATED_MIGRATION_PLAN.md`**. The mapping from this document's analysis
to that plan is:

| This document's recommendation | Consolidated plan step |
|---|---|
| Remove both Vector and DataDog sinks | **Step 1** — Both sinks removed; OTel sink gRPC added |
| Handle `MetricValue::Sketch` match arms in non-DD sinks | **Step 1** — convert to `AggregatedHistogram` as bridge |
| OTel metric encoder completion | **Step 2** — unblocks accurate sketch→histogram conversion |
| DataDog source adaptation (remove DD types from core) | **Step 3** — `datadog_api_key`, `DatadogMetricOriginMetadata`, `AgentDDSketch` leave core |
| APM stats + tail sampling OTel transforms | **Step 4** — vendor-neutral replacements, available before sink re-integration |
| Core event model → OTel types | **Step 5** — the clean foundation |
| Native codec/proto removal | **Step 6** |
| Optional Vector sink re-integration | **Post-Step 5** — studied and designed on clean OTel core |
| Optional DataDog sink re-integration | **Post-Step 5** — studied and designed on clean OTel core |
