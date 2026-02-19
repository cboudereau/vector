# Gap Analysis: Docs vs Codebase — What Blocks Coding

Generated from source code inspection. Every claim here is code-verified.

> All action items from this document have been integrated into `CONSOLIDATED_MIGRATION_PLAN.md`
> and `DISK_BUFFER_MIGRATION.md`. This file is kept as the audit trail.

---

## TL;DR — Blockers Before Any PR Can Be Merged

| # | Blocker | Severity | Blocks |
|---|---|---|---|
| B1 | `ser.rs` encode/decode DO NOT branch on `BUFFER_FORMAT` yet | **HARD** | Step 0a completion |
| B2 | No `OtlpBufferBatch` proto exists anywhere | **HARD** | Step 0a completion |
| B3 | `buffer_format` not wired into `GlobalOptions` config | **HARD** | Step 0a completion |
| B4 | OTel metric serializer errors on `Event::Metric` | **HARD** | Step 2 (Step 1 blocked) |
| B5 | `OtlpDeserializer::output_type()` returns `Log | Trace`, not `Metric` | **HARD** | Step 2 |
| B6 | Span `InstrumentationScope` silently dropped in `spans.rs` | **MEDIUM** | Step 0b (data loss today) |
| B7 | `AgentDDSketch::transform_to_sketch` exists but no `to_exp_histogram` | **MEDIUM** | Step 1 (Sketch arms in non-DD sinks must convert) |

---

## Section 1 — Step 0a: What Is Actually Done vs What the Plan Claims

### What is done (verified in `lib/vector-core/src/event/ser.rs`)

- `BufferFormat` enum with `#[default] Vector`, `Otlp`, `Migrate` ✓
- `BUFFER_FORMAT: AtomicCell<BufferFormat>` static ✓
- `OtlpEncoding = 0b10` flag in `EventEncodableMetadataFlags` ✓
- `get_metadata()` branches on `BUFFER_FORMAT` ✓
- `can_decode()` branches on `BUFFER_FORMAT` ✓
- 6 unit tests covering all three modes ✓

### What is NOT done (plan says "remaining work")

**B1 — `encode()` and `decode()` do not branch on `BUFFER_FORMAT`:**

Current `encode()` (line 137–144):
```rust
fn encode<B>(self, buffer: &mut B) -> Result<(), Self::EncodeError>
where B: BufMut {
    proto::EventArray::from(self)
        .encode(buffer)  // ← always Vector proto, never OTel
        .map_err(|_| EncodeError::BufferTooSmall)
}
```

Current `decode()` (line 146–161): always decodes as `proto::EventArray` or `proto::EventWrapper`.
Never checks `BUFFER_FORMAT`, never attempts `OtlpBufferBatch` decode.

This means the toggle currently has NO behavioural effect on actual I/O. `get_metadata()` and
`can_decode()` are implemented and tested, but they gate decoding of records whose encoding
path doesn't exist yet. The system is partially wired — the metadata/flag layer is done, the
actual encode/decode is not.

**B2 — `OtlpBufferBatch` proto does not exist:**

`lib/vector-core/proto/` contains only `event.proto` (241 lines). There is no
`otlp_buffer.proto` or `OtlpBufferBatch` type anywhere in the codebase.
The plan describes it but it has not been created.

**B3 — `buffer_format` not wired into `GlobalOptions`:**

`lib/vector-core/src/config/global_options.rs` (`GlobalOptions` struct, fields at lines
62–175) has: `data_dir`, `wildcard_matching`, `log_schema`, `telemetry`, `timezone`, `proxy`,
`acknowledgements`, `expire_metrics*`, `buffer_utilization_ewma_alpha`, `latency_ewma_alpha`,
`metrics_storage_refresh_period`.

**No `buffer_format` field.** The `BUFFER_FORMAT` static can only be set programmatically
today. There is no config file path from `vector.yaml` → `BUFFER_FORMAT.store(...)`.

### Step 0a completion checklist (what remains to code)

1. `lib/vector-core/proto/otlp_buffer.proto` — define `OtlpBufferBatch` as a wrapper around
   the three OTel export request types.
2. Generated Rust from that proto (add to `build.rs`).
3. `encode()` update: check `BUFFER_FORMAT`, dispatch to `OtlpBufferBatch::from(self).encode()`
   when `Otlp` or `Migrate`.
4. `decode()` update: when metadata has `OtlpEncoding`, decode via `OtlpBufferBatch::decode()`.
5. `OtlpBufferBatch → EventArray` conversion (uses existing `lib/opentelemetry-proto/src/`).
6. `EventArray → OtlpBufferBatch` conversion (new — inverse direction).
7. `buffer_format` field in `GlobalOptions` (default `"vector"`) + startup wiring:
   `BUFFER_FORMAT.store(config.global.buffer_format.into())`.
8. Integration test.

---

## Section 2 — OTel Decoder/Encoder Type Gap (B4, B5)

### B4 — `OtlpSerializer` errors on `Event::Metric`

`lib/codecs/src/encoding/format/otlp.rs` line 127:
```rust
Event::Metric(_) => {
    Err("OTLP serializer does not support native Vector metrics yet.".into())
}
```

This is correctly identified in the plan as the Step 2 blocker. No new finding here, but
the impact is wider than the plan states:

- The OTel sink cannot forward any pipeline that ingests metrics from the DD source
  (which produces `Event::Metric` events via `ResourceMetrics::into_event_iter()`).
- The OTel sink cannot replace the DD metrics sink until Step 2 is done.
- This also means the buffer `encode_as_otlp` path (Step 0a item 3 above) cannot use the
  OTel serializer for metrics — it needs its own direct `Event::Metric → MetricsDataPoint`
  conversion, independently of the OTLP serializer.

### B5 — `OtlpDeserializer::output_type()` declares `Log | Trace`, not `Metric`

`lib/codecs/src/decoding/format/otlp.rs` line 70–72:
```rust
pub fn output_type(&self) -> DataType {
    DataType::Log | DataType::Trace
}
```

This is consistent with the current design where `use_otlp_decoding = true` stores OTLP
metrics as `Log` events (raw blobs). But it becomes incorrect in the target state where metrics
must be `Event::Metric`.

The OTel source `outputs()` method (config.rs line 355–358) already branches:
```rust
let metrics_output = if self.use_otlp_decoding {
    SourceOutput::new_maybe_logs(DataType::Log, Definition::any()).with_port(METRICS)
} else {
    SourceOutput::new_metrics().with_port(METRICS)
};
```

This means: when `use_otlp_decoding = false` (the default), metrics go to the `metrics` port
as proper `Event::Metric` events via `ResourceMetrics::into_event_iter()`. The OTel
source is actually already correct for the default path.

**The gap is in the decoder, not the source.** The `OtlpDeserializer` is only used when
`use_otlp_decoding = true`. The plan is correct that the decoder path (`use_otlp_decoding`)
is a dead-end to be removed in Step 5d, not fixed.

**Clarification for the plan:** The `OtlpDeserializer::output_type()` missing `Metric` is not
a bug to fix — it accurately describes the blob-passthrough mode. The note in the plan
about Step 2 is about the encoder, not the decoder.

---

## Section 3 — `spans.rs` Scope Drop (B6)

The plan correctly identifies this in Step 0b. Code-verified at
`lib/opentelemetry-proto/src/spans.rs` lines 32–34:

```rust
self.scope_spans
    .into_iter()
    .flat_map(|ils| ils.spans)  // ← `ils.scope` (InstrumentationScope) discarded
    .map(move |span| { ... })
```

`ils` is `ScopeSpans { scope: Option<InstrumentationScope>, spans: Vec<Span>, ... }`.
The `scope` field carries: `name: String`, `version: String`, `attributes: Vec<KeyValue>`,
`dropped_attributes_count: u32`.

The `logs.rs` equivalent correctly passes scope through. The fix is ~15 lines.

**This is live data loss today** — every trace span processed by the OTel source loses its
instrumentation library name/version/attributes. Since the plan calls this Step 0b, it
should be the first PR after Step 0a closes.

---

## Section 4 — Sketch Conversion Gap in Non-DD Sinks (B7)

The plan says: for Step 1, the Prometheus/InfluxDB/GreptimeDB `MetricValue::Sketch` match
arms must be deleted (not approximated). That is the right call but has a dependency:

`AgentDDSketch` has `transform_to_sketch` (histogram → sketch, forward direction) and
`quantile()` and arithmetic methods, but **no `to_aggregated_histogram()` method**. The
non-DD sinks today call `ddsketch.quantile(*q)` to get approximate quantile values from the
sketch. That is the only conversion path available.

`SINK_REMOVAL_STRATEGY.md` line 235 says these match arms should "convert to
`AggregatedHistogram` using the existing `AgentDDSketch::to_histogram()` method." That method
does not exist by that name. The closest is `insert_interpolate_buckets` which goes the
other direction (histogram → sketch).

**What actually exists:**
- `quantile(q: f64) -> Option<f64>` — single quantile query
- `insert_interpolate_buckets()` — histogram → sketch (wrong direction)
- `transform_to_sketch()` — metric → sketch (wrong direction)
- No `to_aggregated_histogram()` or `to_buckets()` or similar

**Impact:** When the DD sinks are deleted in Step 1, the Prometheus/InfluxDB/GreptimeDB
match arms cannot "convert to AggregatedHistogram" as the plan states. They have two options:

1. **Implement a new method** `AgentDDSketch::to_aggregated_histogram(bounds: &[f64]) ->
   MetricValue` that samples the sketch at the given explicit bounds and returns bucket counts.
   This is the correct approach and belongs in `AgentDDSketch` since it still lives in core
   at that point.

2. **Delete the arms entirely** and let those sinks skip sketch metrics. This is simpler but
   means DataDog agent sketch metrics are silently dropped by Prometheus/InfluxDB/GreptimeDB.

Option 1 is better: it produces an approximation, documents it explicitly, and preserves data
flow until Step 3 removes `AgentDDSketch` from core entirely. The method should be added as
part of Step 1, not deferred.

---

## Section 5 — `APM_STATS_OTLP_BACKPORT.md` vs Plan Inconsistency

`APM_STATS_OTLP_BACKPORT.md` is a detailed, standalone spec. The `CONSOLIDATED_MIGRATION_PLAN.md`
Step 4 section references `TAIL_SAMPLING_BACKPORT.md` for the `apm_stats` transform spec.
But `TAIL_SAMPLING_BACKPORT.md` only has a brief section (§5) on APM stats. The detailed spec
is in `APM_STATS_OTLP_BACKPORT.md`.

**The consolidated plan should reference `APM_STATS_OTLP_BACKPORT.md` directly.** Both the
metric names and module structure differ slightly:

| Item | `TAIL_SAMPLING_BACKPORT.md` §5 | `APM_STATS_OTLP_BACKPORT.md` §2.3 |
|---|---|---|
| Duration metric name | `trace.spans.duration` | `spans.duration.ok` + `spans.duration.error` |
| Error metric name | `trace.spans.errors` | `spans.errors` |
| Total metric name | `trace.spans.total` | `spans.hits` + `spans.top_level_hits` |
| Completed trace metric | `trace.traces.completed` | Not present (different scope) |
| Module path | Not specified | `src/transforms/apm_stats/` (5 files) |

The `APM_STATS_OTLP_BACKPORT.md` version is more detailed and correct. Use it as the
canonical spec for Step 4 APM stats.

---

## Section 6 — Minor Inconsistencies in the Plan

### 6a — Line counts

`SINK_REMOVAL_STRATEGY.md` §1.1 states Vector sink is "~530 lines". Actual count:
`src/sinks/vector/` = 791 lines (4 files). The discrepancy is because the strategy doc counts
only production code (~530) vs total including tests. Not a blocking issue but worth noting.

`SINK_REMOVAL_STRATEGY.md` §2 states DataDog sinks are "~6,040 lines including tests". Actual
count: 9,882 lines. The 3,842-line gap is the test files (`integration_tests.rs`, `tests.rs`).
The strategy doc appears to count only non-test files. Not a blocking issue.

### 6b — Step 2 output_type must be updated

`OtlpSerializerConfig::input_type()` currently returns `DataType::Log | DataType::Trace`.
After Step 2 it must return `DataType::Log | DataType::Metric | DataType::Trace`. The plan
mentions this. Confirmed it is not done yet.

### 6c — `DISK_BUFFER_MIGRATION.md` references `AtomicCell` but Cargo.toml must be checked

`crossbeam-utils` is used for `AtomicCell`. Confirmed in `lib/vector-core/Cargo.toml` (from
prior session). This is fine.

### 6d — `SINK_REMOVAL_STRATEGY.md` §5 "forward code to invert"

This section says `ExponentialHistogram → AggregatedHistogram` conversion was lossy and the
reverse should emit OTel `Histogram` with explicit bounds. But after Step 3, `AggregatedHistogram`
no longer exists in core — the model is pure OTel. The "invert" path in Step 7 should go
directly from OTel `Metric` → OTel wire proto, not through `AggregatedHistogram`.
This is consistent with the overall plan but the wording in the strategy doc is slightly ahead
of itself. No code impact.

---

## Section 7 — What the Plan Does NOT Cover (New Gaps)

### G1 — No plan for `transform_to_sketch` callers after Step 1

`AgentDDSketch::transform_to_sketch` is called 9 times in `src/sinks/datadog/metrics/normalizer.rs`.
That file is deleted in Step 1. But `transform_to_sketch` is a `pub` method in
`lib/vector-core/src/metrics/ddsketch.rs` — it stays until Step 3. After Step 1, it becomes
dead code. Not a blocker, just untidy. Should be noted so it gets cleaned in Step 3.

### G2 — No plan for `EventArray → OtlpBufferBatch` conversion ordering

`EventArray` contains `Vec<LogEvent | Metric | TraceEvent>`. The `OtlpBufferBatch` proto
(once created) needs to split these by type into three separate export request types.
The conversion must group correctly — one `ExportLogsServiceRequest` for all log events,
one `ExportMetricsServiceRequest` for all metric events, etc.

The existing `EventArray::logs()`, `EventArray::metrics()`, `EventArray::traces()` iterators
make this straightforward, but the plan does not specify the exact proto message layout. This
needs to be specified before implementing Step 0a encode/decode.

### G3 — No plan for `buffer_format` config validation

When `buffer_format = "otlp"` is set but the existing disk buffer contains Vector-encoded
records, the process will crash on startup (records fail `can_decode`). The correct behaviour
is: start in `migrate` mode when an existing buffer is detected, switch to `otlp` only when
the buffer is fully drained. This is implied by the `migrate` mode concept but the config
validation / startup logic is not specified anywhere.

### G4 — VRL `TypeState` and OTel types

`VRL_MIGRATION_TOOL.md` §3 Pass 2 says "TypeState is used to confirm that `.` is
`Bytes`-typed." But `TypeState` is computed against the Vector internal type schema today.
After Step 5, the type schema changes fundamentally (`.` is no longer `Bytes` for logs — the
log body becomes `AnyValue`). The VRL migration tool must use the **OTel type schema** for
TypeState computation, not the Vector schema. The plan does not address this.

### G5 — `EventCount` and `ByteSizeOf` for OTel types

Every type in `EventArray` must implement `EventCount` and `ByteSizeOf` for backpressure and
buffer accounting. The current `TraceEvent` delegates to `LogEvent`. After Step 5, typed OTel
`Span` will need its own `ByteSizeOf` implementation. The plan does not mention this. Not a
blocker for early steps, must be addressed in Step 5.

### G6 — Schema definitions and VRL compiler integration

The OTel source's `outputs()` method builds a `schema::Definition` (lines 269–364 in
config.rs) with Vector-specific field paths (`RESOURCE_KEY`, `ATTRIBUTES_KEY`, etc.). After
Step 5, the schema definition must reflect OTel native field paths. The VRL compiler uses
these definitions for type checking. The plan mentions VRL migration but not schema definition
migration. This is required for the VRL compiler to type-check OTel-native programs correctly
after Step 5.

---

## Summary: Ordered Action Items Before First PR

| Priority | Action | File(s) | Step |
|---|---|---|---|
| **P0** | Specify `OtlpBufferBatch` proto layout (G2 above — agree on message structure) | New `otlp_buffer.proto` | 0a |
| **P0** | Specify `buffer_format` startup/validation logic (G3 — migrate-on-existing-buffer) | `GlobalOptions` + startup | 0a |
| **P1** | Implement `OtlpBufferBatch` proto + `encode`/`decode` branching in `ser.rs` | `ser.rs`, new proto | 0a |
| **P1** | Add `buffer_format` to `GlobalOptions` + startup wiring | `global_options.rs`, startup | 0a |
| **P1** | Fix `spans.rs` scope drop (15 lines, zero risk) | `spans.rs` | 0b |
| **P2** | Add `AgentDDSketch::to_aggregated_histogram(bounds)` method | `ddsketch.rs` | 1 |
| **P2** | Implement OTel metric encoder for all `MetricValue` variants | `otlp.rs` (encoding) | 2 |
| **P3** | Update `OtlpSerializerConfig::input_type()` to include `Metric` | `otlp.rs` (encoding) | 2 |
| **P3** | Align `CONSOLIDATED_MIGRATION_PLAN.md` Step 4 to reference `APM_STATS_OTLP_BACKPORT.md` | Docs | — |
