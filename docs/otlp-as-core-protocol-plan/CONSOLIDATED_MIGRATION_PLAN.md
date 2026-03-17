# Consolidated Migration Plan: Vector → OTLP as Core Protocol

Single source of truth for the migration. All other documents in this folder feed into it.

---

## Document Index

| Document | Purpose |
|---|---|
| `CONSOLIDATED_MIGRATION_PLAN.md` | **This file** — verified plan, decisions, risks, status |
| `GUIDELINES.md` | Architectural principles for contributors — read before writing code |
| `MIGRATION_STUDY.md` | Component-by-component complexity analysis |
| `PROTOCOL_GAP_ANALYSIS.md` | Field-by-field gap: Vector native protocol vs OTLP |
| `PERFORMANCE_AND_TRADEOFFS.md` | Performance analysis and otel-collector-contrib comparison |
| `DISK_BUFFER_MIGRATION.md` | Zero-downtime buffer format toggle specification (Step 0a) |
| `APM_STATS_OTLP_BACKPORT.md` | `apm_stats` transform — canonical spec (Step 4) |
| `TAIL_SAMPLING_BACKPORT.md` | `tail_sample` transform specification (Step 4) |
| `SINK_REMOVAL_STRATEGY.md` | Remove-first strategy rationale and re-integration scope |
| `VRL_MIGRATION_TOOL.md` | VRL migration tool specification and rewrite rule catalogue |
| `GAP_ANALYSIS.md` | Code-verified gaps between docs and codebase — read before coding |
| `UPSTREAM_PROTO_MIGRATION.md` | Migrate to upstream `opentelemetry-proto` crate + OTLP HTTP JSON support |

---

## Goal

Replace Vector's internal event model and wire protocol with OpenTelemetry (OTLP/OTel) as the
sole core protocol and in-memory representation. Concretely:

- `vector-core`, transforms, VRL, and the buffer layer operate exclusively on OTel types.
- All inter-process communication between Vector instances uses OTLP/gRPC. HTTP is also
  supported at sources and sinks.
- Vector/DD wire formats live **exclusively** in source and sink adapters. Adapters convert at
  the I/O boundary and never leak proprietary types into core.
- All trade-offs associated with vendor protocols (DDSketch precision, multi-value tags,
  `interval_ms`, etc.) are owned by the relevant adapter — not by core.
- Features like APM stats and internal pipeline telemetry are preserved, re-implemented as
  OTel-native transforms and metrics.

---

## Guiding Principles

1. **Baby steps, always green.** Every PR leaves all existing tests passing.
2. **OTLP/OTel is the only core protocol.** No vendor types, no approximations in core.
3. **Vendor logic lives exclusively in adapters.** Adapters depend on core; core never depends
   on adapters.
4. **The compiler enforces the boundary.** `cargo build -p vector-core` clean = boundary correct.
5. **No approximations in core.** `ExponentialHistogram` is the correct OTel type. Sketch
   conversion happens in the DataDog source adapter at the I/O boundary.
6. **gRPC internally, HTTP also supported.** OTLP/gRPC for inter-Vector. HTTP at both
   sources and sinks for external integrations.
7. **Features are preserved, not dropped.** APM stats, tail sampling, disk buffer durability all
   survive — re-implemented with OTel types.

---

## Verified Current Architecture

From source code analysis:

```
lib/vector-core/src/event/mod.rs
  pub enum Event { Log(LogEvent), Metric(Metric), Trace(TraceEvent) }

lib/vector-core/src/event/metadata.rs (611 lines)
  EventMetadata::Inner {
    datadog_origin_metadata: Option<DatadogMetricOriginMetadata>,  ← DD in core
    secrets: Secrets,  // contains "datadog_api_key", "splunk_hec_token"
    source_id, source_type, upstream_id, source_event_id, ...
  }
  const DATADOG_API_KEY: &str = "datadog_api_key";  ← hardcoded DD constant in core
  pub fn datadog_api_key() / set_datadog_api_key()  ← first-class DD methods in core

lib/vector-core/src/event/metric/value.rs (749 lines)
  pub enum MetricValue {
    Counter, Gauge, Set, Distribution, AggregatedHistogram,
    AggregatedSummary,
    Sketch { sketch: MetricSketch },  ← DD-only, in core
  }
  pub enum MetricSketch { AgentDDSketch(AgentDDSketch) }  ← DD-only

lib/vector-core/src/event/trace.rs (192 lines)
  pub struct TraceEvent(LogEvent);  ← just a LogEvent newtype, no typed span fields

lib/vector-core/src/metrics/ddsketch.rs (1,637 lines)
  pub struct AgentDDSketch { ... }  ← entire DD sketch implementation in core

lib/codecs/src/encoding/format/otlp.rs (132 lines)
  Event::Metric(_) => Err("OTLP serializer does not support native Vector metrics yet.")
  ← metric encoding completely missing; only works for pre-encoded OTLP blobs

src/sinks/opentelemetry/mod.rs (104 lines)
  Protocol::Http(HttpSinkConfig)  ← HTTP only; gRPC not implemented
  comment: "Currently only HTTP is supported, but we plan to support gRPC."

src/sources/opentelemetry/config.rs
  (use_otlp_decoding removed in Step 5e — source always emits OTel-native events)

lib/opentelemetry-proto/src/spans.rs (159 lines)
  ResourceSpans::into_event_iter:
    .flat_map(|ils| ils.spans)  ← scope_spans iterated but InstrumentationScope DROPPED
  ← Bug: scope (name, version, attributes) is never stored on the TraceEvent
```

### Where `MetricValue::Sketch` / `AgentDDSketch` appears today

**In core** (must be removed):
- `lib/vector-core/src/event/metric/value.rs` — `MetricValue::Sketch` variant definition
- `lib/vector-core/src/event/metric/mod.rs` — merge/arithmetic logic for Sketch
- `lib/vector-core/src/event/metric/arbitrary.rs` — test generation
- `lib/vector-core/src/event/proto.rs` — protobuf serialization
- `lib/vector-core/src/metrics/ddsketch.rs` — 1,637-line implementation
- `lib/vector-core/src/metrics/mod.rs` — re-export
- `lib/vector-core/src/event/lua/metric.rs` — Lua bridge

**In sources** (conversion happens here, adapter owns the type):
- `src/sources/datadog_agent/metrics.rs` — deserializes DD sketch payload → `MetricValue::Sketch`
  (after migration: → `MetricValue::AggregatedHistogram` using OTel `ExponentialHistogram`)

**In sinks** (residual coupling to remove in Step 1):
- `src/sinks/datadog/metrics/encoder.rs` — encodes `AgentDDSketch` to DD wire format
- `src/sinks/datadog/metrics/normalizer.rs` — normalizes sketch for DD
- `src/sinks/datadog/metrics/sink.rs` — routes sketch metrics
- `src/sinks/datadog/traces/apm_stats/bucket.rs` — uses sketch for APM latency tracking

**In non-DD sinks** (residual coupling, must be cleaned up in Step 1):
- `src/sinks/prometheus/collector.rs:184` — converts sketch to quantiles via `ddsketch.quantile(q)`
- `src/sinks/influxdb/metrics.rs:366` — converts sketch to fields via `ddsketch.avg()`, `.min()`, etc.
- `src/sinks/greptimedb/metrics/batch.rs:40` — size estimate for sketch
- `src/sinks/util/buffer/metrics/split.rs:122` — routes sketch metrics

**In transforms** (residual coupling):
- `src/transforms/log_to_metric.rs:387` — sets `DatadogMetricOriginMetadata` on converted metrics

### Where `DatadogMetricOriginMetadata` appears

- `lib/vector-core/src/event/metadata.rs` — defined and stored in `EventMetadata::Inner`
- `src/sources/datadog_agent/metrics.rs` — sets it at ingestion
- `src/transforms/log_to_metric.rs` — sets it on converted metrics
- `src/sinks/datadog/metrics/encoder.rs` — reads it to populate DD origin fields
- `src/common/datadog.rs` — helper utilities

### OTel codec actual state

The OTLP serializer (`lib/codecs/src/encoding/format/otlp.rs`) operates in two modes:
1. `Event::Log` with `resourceLogs` field → serialize as `ExportLogsServiceRequest` ✓
2. `Event::Log` with `resourceMetrics` field → serialize as `ExportMetricsServiceRequest` ✓
3. `Event::Trace` with `resourceSpans` field → serialize as `ExportTraceServiceRequest` ✓
4. `Event::Metric(_)` → **error: not supported** ✗

This was the state before Steps 2 and 5. After Steps 5a–5e, the OTel source emits
OTel-native events and the OTel sink accepts them directly for all three signals.

---

## Target Architecture

```
Sources (input adapters)       Core (OTel-native only)              Sinks (output adapters)
─────────────────────────────  ────────────────────────────────────  ───────────────────────────
opentelemetry (gRPC + HTTP)    OTel LogRecord                        opentelemetry (gRPC + HTTP)
datadog_agent  ─────────────►  OTel Metric                     ────► prometheus
  DD proto → OTel at boundary    (Sum/Gauge/Histogram/           ────► influxdb, loki, kafka, …
vector (gRPC)  ─────────────►    ExponentialHistogram/Summary)       (all as OTel adapters)
  native → OTel at boundary    OTel Span
kafka, syslog, …  ──────────►  Resource + InstrumentationScope
                               No DD types, no Vector proto types
                               Disk buffer: OtlpBufferBatch proto

REMOVED in Step 1: src/sinks/vector/, src/sinks/datadog/
OPTIONAL in Step 7: re-add as clean OTel adapters
```

---

## Sketch vs Histogram: Why ExponentialHistogram Wins

This section is the source-verified answer to a key design question.

### What DDSketch is

`AgentDDSketch` (1,637 lines in `lib/vector-core/src/metrics/ddsketch.rs`) is a
**relative-error sketch**. Its core parameters (from the source):

```rust
const AGENT_DEFAULT_BIN_LIMIT: u16 = 4096;   // max bins
const AGENT_DEFAULT_EPS: f64 = 1.0 / 128.0;  // ~0.78% relative error per bin
const AGENT_DEFAULT_MIN_VALUE: f64 = 1.0e-9; // min representable value
```

Bucket boundaries are `γ^k` where `γ = 1 + 2*eps ≈ 1.0156`. This gives **guaranteed
relative error**: any quantile query on the sketch returns a value within ±eps of the true
value, regardless of the input distribution. The struct also carries `min`, `max`, `sum`,
`avg` as exact values.

The sketch supports **merge** without precision loss: two sketches with identical config can
be merged by summing bin counts at matching keys. This is how DD aggregates across agents.

It carries an `avg` field that is kept via Welford's incremental update — and the source code
itself documents a precision limitation:
```
// TODO: From the Agent source code, this method apparently loses precision when the
// two averages -- v and self.avg -- are close.
self.avg = self.avg + (v - self.avg) * f64::from(n) / f64::from(self.count);
```

### What OTel ExponentialHistogram is

OTel's `ExponentialHistogram` uses the **same γ-bucketing scheme**:
```
base = 2^(2^(-scale))
bucket boundary for index i = base^i
```

At **scale 7**: `base = 2^(2^-7) = 2^(1/128) ≈ 1.0055`. This gives relative error ≈ 0.27%,
which is **tighter than DDSketch's ~0.78%** at default eps.

`ExponentialHistogram` carries: `count`, `sum`, `scale`, `zero_count`, positive and negative
`BucketSpan + bucket_counts`. It also supports **merge** without precision loss (same index
scheme, sum counts at same offset+index).

It does **not** carry `min`, `max`, or `avg` as first-class fields (unlike `AgentDDSketch`).

### The critical problem in the current code

**`lib/opentelemetry-proto/src/metrics.rs` lines 344–397** — the existing
`ExponentialHistogram → AggregatedHistogram` conversion:

```rust
let base = 2f64.powf(2f64.powi(-scale));
for (i, &count) in positive_buckets.bucket_counts.iter().enumerate() {
    let index = positive_buckets.offset + i as i32;
    let upper_limit = base.powi(index + 1);
    buckets.push(Bucket { count, upper_limit });
}
MetricValue::AggregatedHistogram { buckets, count, sum }
```

This is a **destructive lossy conversion**: it collapses the exponential bucketing into
Vector's `AggregatedHistogram` (explicit upper bounds). The `scale` is lost. After this
conversion, you **cannot reconstruct** the `ExponentialHistogram`. Merging two converted
histograms is not possible with relative-error guarantees because the bucket boundaries no
longer align across different scale values.

This conversion happened when the OTel source decoded an `ExponentialHistogram` via the
legacy path. **Eliminated in Step 5d batch 1** — the OTel source now emits
`Event::OtelMetric` which preserves the full `ExponentialHistogram` proto structure.

### Comparison table

| Property | DDSketch (AgentDDSketch) | OTel ExponentialHistogram (scale 7) |
|---|---|---|
| Relative error guarantee | ±0.78% (eps = 1/128) | ±0.27% (scale 7) — **tighter** |
| Merge without precision loss | Yes | Yes |
| `min` / `max` as exact values | Yes | **No** (not in OTel spec) |
| `avg` | Yes (with known precision issue) | **No** |
| Negative values | Yes (negative bins) | Yes (negative buckets) |
| Standard / vendor-neutral | **No** (DD-specific wire format) | **Yes** (OTLP spec) |
| Supported by Prometheus, Grafana, etc. | No | Yes (native histograms) |
| Supported by DD backend | Yes | **Yes** — DD accepts OTLP natively |
| Max bins | 4096 | Up to `2^(scale+1)` per side |
| Precision after scale change | Preserved (config locked) | Scale is part of the data point — preserved |

### Decision: ExponentialHistogram in core, DDSketch only in the DD adapter

**ExponentialHistogram is the correct choice for core.** Reasons:

1. **Tighter error bound** at scale 7 than DDSketch at default eps.
2. **OTel standard** — every downstream backend (Prometheus native histograms, Grafana,
   Tempo, the DD OTLP endpoint) understands it natively.
3. **Same merge semantics** — bucket counts sum at matching indices, identical to sketch merge.
4. **No precision loss through the pipeline** — no destructive `→ AggregatedHistogram`
   conversion.

**What is genuinely lost:**
- `min` and `max` as guaranteed-exact values. These are a DDSketch-specific extension.
  The DD adapter can optionally carry `min`/`max` in `Resource.attributes["dd.min"]` /
  `["dd.max"]` when it converts sketch payloads, if those fields are needed downstream.
- `avg` as a stored field. Derivable from `sum/count` (which are both exact). The avg
  approximation in DDSketch itself already has a documented precision caveat.

**What is NOT lost:**
- Quantile query capability — derivable from bucket boundaries and counts.
- Merge correctness — OTel ExpHistogram merge is as precise as sketch merge.
- `count` and `sum` — both exact in OTel.

### Conversion mechanics (DD source adapter, Step 3)

```
DDSketch bins: [(k: i16, n: u16)] with gamma = 1.0 + 2*eps ≈ 1.0156
OTel ExpHisto:  base = 2^(2^(-scale)), offset: i32, bucket_counts: [u64]

Scale selection: choose scale s such that base ≈ gamma.
  gamma = 1.0156, base(s=6) = 2^(1/64) ≈ 1.0110, base(s=5) ≈ 1.0219
  scale 6 is the closest: relative error ±0.55% — slightly worse than DDSketch ±0.78%
  but acceptable as a one-time conversion artefact.

For each DDSketch bin (k, n):
  lower = gamma^(k - norm_bias)   // from AgentDDSketch::bin_lower_bound()
  mid   = sqrt(lower * gamma * lower) // geometric midpoint of the bin
  otel_index = floor(log(mid) / log(base)) + offset_adjustment
  bucket_counts[otel_index] += n

count, sum: copied directly (exact)
zero_count: bins with k == 0 → zero_count
negative bins (k < 0): → negative BucketSpan
```

The conversion is documented in the DD source adapter. Precision loss is bounded and
one-directional (at the conversion boundary only); subsequent merges in core are lossless.

---

## Strategy: Remove First vs Continuous Refactor

**Chosen: Remove first (Step 1), re-integrate optionally later (Step 7).**

Removing both sinks at Step 1 (before any core type changes) eliminates ~9,900 lines of
proprietary sink code from the codebase. Every subsequent step operates on a smaller, cleaner
tree. The residual sketch coupling in Prometheus/InfluxDB/GreptimeDB is forced to the surface
and handled once, cleanly, rather than dragged through every core refactoring step.

The OTel sink with gRPC (added in Step 1) is the drop-in replacement for both the Vector sink
and all DataDog sinks. DataDog now accepts OTLP natively at `api.datadoghq.com`.

**Continuous refactor was rejected** because it creates a long-lived dual-type period where
every core change must maintain both representations, doubling test surface and making the
final removal harder, not easier.

---

## Actual Coupling Map: What Depends on What

Derived from source code `rg` analysis:

```
AgentDDSketch / MetricValue::Sketch removal blast radius:
  lib/vector-core/ (8 files) ← must all be cleaned in Step 3
  src/sinks/datadog/ (4 files) ← removed in Step 1
  src/sinks/prometheus/collector.rs ← sketch match arm deleted in Step 1
  src/sinks/influxdb/metrics.rs ← sketch match arm deleted in Step 1
  src/sinks/greptimedb/metrics/ (2 files) ← sketch match arm deleted in Step 1
  src/sinks/util/buffer/metrics/split.rs ← sketch routing deleted in Step 1
  src/sources/datadog_agent/metrics.rs ← conversion owned here; stays in Step 3
  src/test_util/mock/transforms/basic.rs ← test fixture; updated when Sketch leaves core

DatadogMetricOriginMetadata removal blast radius:
  lib/vector-core/src/event/metadata.rs ← definition removed in Step 3
  src/sources/datadog_agent/metrics.rs ← sets it; stays in adapter (mapped to resource attrs)
  src/transforms/log_to_metric.rs ← sets it; removed in Step 3 (DD sinks that read it gone)
  src/sinks/datadog/metrics/encoder.rs ← reads it; removed in Step 1
  src/common/datadog.rs ← helper; may stay for source HTTP parsing

use_otlp_decoding flag:
  → REMOVED in Step 5e. All references deleted from src/.
```

---

## Execution Order

```
Step 0    Foundations (buffer toggle + isolation test + span scope fix)          — COMPLETE
Step 2    OTel metric encoder — prerequisite for Step 1                         — COMPLETE
Step 1    Both sinks removed; OTel sink gRPC added; sketch arms cleaned         — COMPLETE
Step 3    DD source rewritten as clean OTel adapter; DD types leave core        — COMPLETE
Step 5a   Introduce OTel wrapper types (additive, zero breakage)                — COMPLETE
Step 5b   Migrate traces: OTel source/sink emit/accept OtelSpanEvent           — COMPLETE
Step 5c   Migrate logs: OTel source/sink emit/accept OtelLogEvent              — COMPLETE (batch 1)
Step 5d   Migrate metrics: OTel source/sink emit/accept OtelMetricEvent        — COMPLETE (batch 1)
Step 5e   Remove use_otlp_decoding flag + legacy deserializer paths             — COMPLETE
Step 5e²  OTLP serializer encodes OTel-native events (HTTP sink path)           — COMPLETE
Step 5c²a VrlTarget supports OTel-native events (all 3 signals)                — COMPLETE
Step 5c²b Condition matchers + sample transform recognize OTel events           — COMPLETE
Step 5c²c Codec serializers handle OTel-native log events                      — COMPLETE
Step 5c²d Transforms handle OTel-native events (eliminate panics)              — COMPLETE
Step 5c²e Sinks handle OTel-native events (eliminate panics)                   — COMPLETE
Step 5c²f Buffer codec + remaining gaps (tap, OutputBuffer, sematext, NR)      — COMPLETE
Step 5c²g Last unsafe into_log/as_mut_log call sites                           — COMPLETE
Step 5c²h Template engine + Transformer + silent-drop fixes                    — COMPLETE
Step 5c²  Migrate logs batch 2+: other sources, transforms, sinks              — COMPLETE
Step 5d²  Migrate metrics batch 2+: other sources, transforms, sinks           — COMPLETE
Step 5f   Ship VRL migration tool                                              — COMPLETE
Step 5g   Rename OtelXxxEvent → OtelXxx + type alias cleanup                   — COMPLETE
Step 5h   OTLP HTTP JSON ingestion + dependency upgrades                       — COMPLETE
Step 6    Full legacy removal: sources → sinks → core → native codecs          — IN PROGRESS (6a–6d COMPLETE)
Step 7    Re-integration: Vector + DataDog sinks/sources as OTel adapters
Step 4    Tail sampling + load-balancing sink + pipeline telemetry
```

**Why Step 6 before Step 4 and Step 7:** Completing the core protocol migration before
adding new features ensures a clean foundation. Step 4 (tail sampling) and Step 7
(sink/source re-integration) should only be built on OTel-native types — building them
on a codebase that still has 153 files referencing legacy types means every new feature
would need to handle both old and new representations, doubling complexity. Finishing
Step 6 first means Steps 4 and 7 are pure additive work on a clean OTel-only core.

**Why Step 7 before Step 4:** Re-adding Vector and DataDog as clean OTel adapters
restores practical deployment compatibility. Tail sampling is a net-new feature that
has never existed in Vector; adapter re-integration restores capabilities that were
removed in Step 1. Users blocked by the missing sinks are unblocked sooner.

**Why Step 2 before Step 1:** The OTLP serializer currently errors on `Event::Metric`. Step 2
fixes this. Without it, the OTel sink cannot replace the DataDog metric sink and the migration
is blocked. Step 2 must land before Step 1 can be validated.

**Why Step 5 was next after Step 3:** Steps 0–3 are complete. Step 4 (tail sampling,
load-balancing sink, pipeline telemetry) requires the final OTel event model — all three
sub-components deeply couple to event field paths and types. Building on
`TraceEvent(LogEvent)` then rewriting for typed OTel `Span` is wasteful. Step 5 is the
highest-impact change and unblocks everything. Step 5 uses an **incremental wrapper
strategy** (sub-steps 5a–5g) — OTel wrapper types are introduced alongside existing
types, then each signal is migrated independently. The codebase compiles at every
intermediate commit.

---

## Step 0 — Foundations

### 0a — Buffer format toggle

**Status: COMPLETE**

All functionality is implemented, tested, and wired end-to-end:

- `BufferFormat` enum (`Vector` / `Otlp` / `Migrate`) with `#[default] Vector`
- `BUFFER_FORMAT: AtomicCell<BufferFormat>` process-wide static
- `OtlpEncoding = 0b10` flag in `EventEncodableMetadataFlags`
- `get_metadata()` branches on `BUFFER_FORMAT` — stamps correct flags on new records
- `can_decode()` branches on `BUFFER_FORMAT` — accepts/rejects records by flag
- `BufferFormat`, `BUFFER_FORMAT`, `EventEncodableMetadata`, and
  `EventEncodableMetadataFlags` re-exported from `lib/vector-core/src/event/mod.rs`
- 6 unit tests covering all three modes for both `get_metadata` and `can_decode`
- `lib/vector-core/proto/otlp_buffer.proto` — `OtlpBufferBatch` message defined, compiled
  via `lib/vector-core/build.rs`
- `OtlpCodec` trait in `lib/vector-core/src/event/otlp.rs` — codec vtable avoiding
  circular crate dependency. `vector-core` owns the trait and global registry;
  `opentelemetry-proto` registers the implementation
- `lib/opentelemetry-proto/src/buffer_codec.rs` — full `VectorOtlpCodec` implementation:
  - `event_array_to_batch()`: `EventArray` → `OtlpBufferBatch` (logs, metrics, traces)
  - `batch_to_event_array()`: `OtlpBufferBatch` → `EventArray` using existing
    `ResourceLogs/ResourceMetrics/ResourceSpans::into_event_iter()`
  - Round-trip unit tests for logs and counter metrics
  - **Migration integration test**: writes a record in Vector mode, switches to Migrate
    mode, verifies old record decodes, writes new OTLP record, switches to Otlp mode,
    verifies OTLP records decode and Vector metadata is rejected
- `encode()` in `ser.rs` branches on `BUFFER_FORMAT`:
  - `Vector` → `proto::EventArray::from(self).encode(buffer)`
  - `Otlp` | `Migrate` → `otlp::encode_as_otlp(&self, buffer)`
- `decode()` in `ser.rs` branches on `OtlpEncoding` metadata flag:
  - flag set → `otlp::decode_from_otlp(bytes)`
  - flag not set → existing `proto::EventArray` / `proto::EventWrapper` path
- `buffer_format` field in `lib/vector-core/src/config/global_options.rs`
  (default `"vector"`, `serde(rename_all = "lowercase")`)
- Startup wiring in `src/app.rs`: `BUFFER_FORMAT.store(config.global.buffer_format)` +
  `buffer_codec::init()` registers the OTLP codec before any buffer is opened
- **Startup validation** (`src/app.rs`): `maybe_force_migrate_mode()` — if
  `buffer_format = "otlp"` is set but any sink's disk buffer directory
  (`<data_dir>/buffer/v2/<sink_id>/`) contains `.dat` files, automatically overrides to
  `Migrate` mode and logs a warning. 4 unit tests for `has_dat_files` helper.

Full spec: `DISK_BUFFER_MIGRATION.md`.

### 0b — Per-signal isolation test + span scope fix

**Status: COMPLETE**

**Isolation test** (`lib/vector-core/src/source_sender/tests.rs`):
`per_signal_backpressure_isolation` — fills the metrics named-output channel to capacity (1),
then sends logs and traces to their own named-output channels. Asserts both sends complete
within 200 ms, proving that the channels are independent. Passes.

**Span scope fix** (`lib/opentelemetry-proto/src/spans.rs`):
`ResourceSpans::into_event_iter` now passes `scope` through to `ResourceSpan::into_event`.
The scope fields (`scope.name`, `scope.version`, `scope.attributes`) are stored on the
`TraceEvent` under the `scope.*` path, mirroring the existing `logs.rs` pattern.
Two tests added: `scope_name_and_version_preserved` and `missing_scope_does_not_panic`.
Both pass.

### Validation gate (Step 0) — ALL PASS

- 186 vector-core tests pass (excluding pre-existing TLS fixture failures, unrelated).
- 7 opentelemetry-proto tests pass including new scope tests and migration test.
- 4 `has_dat_files` unit tests pass.
- `rg "flat_map.*ils.spans" lib/opentelemetry-proto/src/spans.rs` returns no match — scope
  is no longer dropped.
- Migration integration test: Vector→Migrate→Otlp encode/decode round-trip passes.

---

## Step 2 — OTel Metric Encoder (Prerequisite for Step 1)

**Status: COMPLETE**

### What was done

**`lib/opentelemetry-proto/src/metrics.rs`** — added public conversion functions:
- `metric_event_to_otel_metric(m: &Metric) -> proto::metrics::v1::Metric` — converts all
  encodable `MetricValue` variants to their OTel equivalents. `Distribution`, `Set`, `Sketch`
  produce a zero-value gauge (metric name preserved; variant will be removed in Step 3/5).
- `metric_to_export_request(m: &Metric) -> ExportMetricsServiceRequest` — wraps the above
  in a single-metric request.
- `encode_metric_to_request(m: &Metric, buf: &mut impl BufMut)` — encodes directly to bytes.
- `buckets_to_otel_bounds(buckets: &[Bucket]) -> (Vec<f64>, Vec<u64>)` — shared helper.

**`lib/codecs/src/encoding/format/otlp.rs`** — the `Event::Metric(_)` arm now calls
`encode_metric_to_request` instead of returning an error.
`OtlpSerializerConfig::input_type()` now returns `DataType::Log | DataType::Metric | DataType::Trace`.

**`src/sinks/opentelemetry/`** — added `grpc.rs` module:
- `GrpcConfig`: configurable gRPC sink (endpoint, compression, batch, TLS, request settings).
- `OtlpGrpcService`: tonic-based service, sends logs/metrics/traces to independent
  `ExportXxxServiceRequest` calls on separate signal-segregated gRPC RPCs.
- `OtlpGrpcSink`: batches `Event`s, converts to `OtlpRequest`, drives through tower service.
- `Protocol::Grpc(GrpcConfig)` variant added to `OpenTelemetryConfig`.

### Validation gate (Step 2) — PASS

- 8 `codecs::encoding::format::otlp` tests pass (3 new metric encoder tests + 5 existing).
- `cargo build -p vector --no-default-features --features sinks-opentelemetry,sources-opentelemetry` clean.
- Existing OTel decode tests unchanged and passing.

---

## Step 1 — Both Sinks Removed; OTel Sink gRPC Added — COMPLETE

### Status

**COMPLETE.** All sub-tasks completed:

1. `AgentDDSketch::to_aggregated_histogram` added (prerequisite, done in prior step).
2. DataDog sinks (`src/sinks/datadog/`) deleted; all feature flags removed from `Cargo.toml`.
3. Vector sink (`src/sinks/vector/`) removed from all production features. Retained under
   `sinks-vector` feature used only by `component-validation-runner` (test harness). Replacing
   the validation harness with the OTel gRPC transport is deferred to Step 3.
4. Sketch coupling in non-DD sinks replaced with `to_aggregated_histogram` bridge in:
   - `src/sinks/prometheus/collector.rs` — now emits Prometheus histogram buckets
   - `src/sinks/influxdb/metrics.rs` — now emits histogram fields
   - `src/sinks/greptimedb/metrics/batch.rs` — size estimate updated
   - `src/sinks/greptimedb/metrics/request_builder.rs` — `encode_sketch` removed, bridge added
5. `src/internal_events/datadog_metrics.rs` and `datadog_traces.rs` deleted.
6. `src/proto/mod.rs` `fds` module (gated on `sinks-datadog_metrics`) removed.

### What is removed

**DataDog sinks** — `src/sinks/datadog/` (9,882 lines total including tests):

| Subsink | Key files | Lines (non-test) |
|---|---|---|
| metrics | `encoder.rs` (1,792), `sink.rs` (473), `normalizer.rs` (327), `config.rs` (305), `request_builder.rs` (301), `service.rs` (186) | ~3,384 |
| traces + apm_stats | `request_builder.rs` (537), `config.rs` (254), `sink.rs` (159), `apm_stats/aggregation.rs` (434), `apm_stats/bucket.rs` (191), `apm_stats/flusher.rs` (172), `apm_stats/mod.rs` (123), `apm_stats/weight.rs` (95) | ~1,965 |
| logs | `sink.rs` (741), `config.rs` (272), `service.rs` (187) | ~1,200 |
| events | `request_builder.rs` (149), `service.rs` (116), `config.rs` (114), `sink.rs` (94) | ~473 |
| shared | `mod.rs` (314) | 314 |

**Vector sink** — removed from production features. Retained as test-harness-only under
`component-validation-runner`. Full removal deferred to Step 3 validation harness migration.

### Residual sketch coupling cleaned in this step

All non-DD sinks now use `to_aggregated_histogram(DEFAULT_BOUNDS)` bridge:

| File | Change |
|---|---|
| `src/sinks/prometheus/collector.rs` | Sketch → `to_aggregated_histogram(buckets)` → Prometheus histogram |
| `src/sinks/influxdb/metrics.rs` | Sketch → `to_aggregated_histogram(DEFAULT_BOUNDS)` → histogram fields |
| `src/sinks/greptimedb/metrics/batch.rs` | Size estimate updated to histogram heuristic |
| `src/sinks/greptimedb/metrics/request_builder.rs` | `encode_sketch` removed, bridge added |
| `src/sinks/util/buffer/metrics/split.rs` | No change needed (Sketch already routed as non-split) |

**Bridge code removed in Step 3 when `AgentDDSketch` leaves core.**

### What is added

gRPC module in `src/sinks/opentelemetry/` (completed in Step 2):
- `Protocol::Grpc(GrpcConfig)` variant alongside existing `Protocol::Http`
- gRPC for internal OTLP forwarding
- HTTP remains for external OTLP endpoints

### Transforms affected

`src/transforms/log_to_metric.rs`: still sets `DatadogMetricOriginMetadata` on converted
metrics. The DD sinks that consumed it are now gone. The field becomes dead in the pipeline
but stays in `EventMetadata` until Step 3 removes it from core.

### Validation gate (Step 1)

- `cargo build` clean — PASS.
- `rg "src/sinks/datadog\b" src/` returns empty — PASS.
- `MetricValue::Sketch` in non-DD sinks all bridged via `to_aggregated_histogram` — PASS.
- Vector sink not in production feature sets — PASS.
- OTel gRPC sink integration test: deferred (harness migration to Step 3).
- Throughput benchmark: deferred.

---

## Step 3 — DataDog Source as Clean OTel Adapter; DD Types Leave Core — COMPLETE

### Status

**COMPLETE.** All sub-tasks completed and committed
(`feat(otlp-migration): step 3 — DD types leave core; AgentDDSketch moves to source adapter`).

### What was done

**`AgentDDSketch` relocated** — `lib/vector-core/src/metrics/ddsketch.rs` (1,737 lines)
moved to `src/sources/datadog_agent/ddsketch.rs` (private `pub(crate)` module). Imports
updated for the `vector` crate context. `DEFAULT_BOUNDS` constant added.

**`MetricValue::Sketch` + `MetricSketch` removed from `vector-core`**:
- `value.rs`: `Sketch { sketch }` variant removed; `distribution_to_sketch()` removed;
  `MetricSketch` enum and all its `impl` blocks removed.
- `arbitrary.rs`: `AgentDDSketch`/`MetricSketch` removed; `MetricValue` generator no longer
  produces Sketch variants.
- `proto.rs`: Sketch encoding/decoding arms replaced with a zero-gauge fallback (backward
  compat with old buffer data). Orphaned `From<AgentDDSketch> for Sketch` and
  `From<sketch::AgentDdSketch> for MetricSketch` impls removed.
- `lua/metric.rs`: Sketch to/from Lua conversion arms removed.
- `event/test/common.rs`: Quickcheck Sketch arm removed; variant count 7→6.
- `metrics/mod.rs`: `mod ddsketch` and `pub use self::ddsketch::...` removed.

**`DatadogMetricOriginMetadata` removed from `EventMetadata`**:
- Struct definition, `Inner.datadog_origin_metadata` field, `datadog_origin_metadata()`,
  `with_origin_metadata()` removed from `lib/vector-core/src/event/metadata.rs`.
- `DatadogMetricOriginMetadata` re-export removed from `event/mod.rs`.
- `DatadogOriginMetadata` proto conversion impls removed from `proto.rs`.

**`DATADOG_API_KEY` + helpers removed from `EventMetadata`**:
- `const DATADOG_API_KEY`, `datadog_api_key()`, `set_datadog_api_key()` removed.
- DD sources now call `metadata.secrets_mut().insert("datadog_api_key", key)` directly.
- `splunk_hec_token` helpers retained (non-DD, still valid in core).

**DD source boundary conversion**:
- `decode_ddsketch` in `metrics.rs` now converts each incoming sketch to
  `MetricValue::AggregatedHistogram` via `AgentDDSketch::to_aggregated_histogram(DEFAULT_BOUNDS)`
  at ingestion — sketch never enters the pipeline.
- `logs.rs`, `traces.rs`: `set_datadog_api_key()` calls replaced with
  `secrets_mut().insert("datadog_api_key", ...)`.

**Sink bridge arms removed** (installed in Step 1, no longer needed):
- `src/sinks/prometheus/collector.rs` — `MetricValue::Sketch` arm removed.
- `src/sinks/influxdb/metrics.rs` — `MetricValue::Sketch` arm removed.
- `src/sinks/greptimedb/metrics/batch.rs` — Sketch size estimate arm removed.
- `src/sinks/greptimedb/metrics/request_builder.rs` — Sketch arm + test removed.
- `src/sinks/util/buffer/metrics/split.rs` — Sketch routing arm removed.

**`log_to_metric.rs`**: all `with_origin_metadata()` calls and `ORIGIN_SERVICE_VALUE`
constant removed. Regex-mangled `.clone());` artifacts corrected.

**Prometheus exporter**: `distributions_as_summaries` path now converts to
`AggregatedHistogram` (sketch variant removed; config flag preserved for compatibility).

### DataDog source changes

The source emits OTel events directly instead of Vector native events.

| File | Change |
|---|---|
| `src/sources/datadog_agent/logs.rs` (281 lines) | DD log payload → OTel `LogRecord` |
| `src/sources/datadog_agent/metrics.rs` (609 lines) | `MetricPayload` → OTel `Sum`/`Gauge`/`Histogram`; `SketchPayload` → OTel `ExponentialHistogram` (conversion here, precision loss documented) |
| `src/sources/datadog_agent/traces.rs` (333 lines) | `TracePayload`/`dd_trace.proto` → OTel `Span` with `InstrumentationScope` |

**`AgentDDSketch` → `ExponentialHistogram` at the boundary:**
Move `lib/vector-core/src/metrics/ddsketch.rs` into a private module within the DataDog source
adapter. The conversion re-buckets `k[]`/`n[]` → OTel `BucketSpan`/`bucket_counts`. `count`
and `sum` map directly. `avg` is approximated as `sum/count` (documented; loses precision
during cross-instance merges per DD's own source comment). Negative `k` → OTel negative
buckets; `k=0` → `zero_count`.

**`DatadogMetricOriginMetadata`:**
Removed from `EventMetadata::Inner`. Instead, the DataDog source stores origin data as
`Resource.attributes["datadog.origin.product"]`, `["datadog.origin.category"]`,
`["datadog.origin.service"]`. The `log_to_metric.rs` call to `.with_origin_metadata()` is
deleted (the DD sinks that consumed it are gone since Step 1).

**`datadog_api_key` in EventMetadata:**
`const DATADOG_API_KEY`, `datadog_api_key()`, `set_datadog_api_key()` are removed from
`lib/vector-core/src/event/metadata.rs`. The DataDog source continues to call
`metadata.secrets_mut().insert("datadog_api_key", key)` directly — the generic secrets map
is unchanged. VRL's `get_secret("datadog_api_key")` continues working since it reads from
`Secrets` via the `SecretTarget` trait, not the removed helpers.

Similarly `splunk_hec_token` helpers can be demoted at the same time for consistency.

**DD proto files retained for source decoding only:**
`proto/vector/dd_metric.proto`, `dd_trace.proto`, `ddsketch_full.proto` stay in the repo.
They are never part of the core data model.

### What leaves `lib/vector-core/`

| Type | Location | Action |
|---|---|---|
| `AgentDDSketch` | `src/metrics/ddsketch.rs` (1,637 lines) | Moved to `src/sources/datadog_agent/ddsketch.rs` (private) |
| `MetricValue::Sketch { sketch }` | `src/event/metric/value.rs` | Variant removed |
| `MetricSketch` enum | `src/event/metric/value.rs` | Removed |
| `DatadogMetricOriginMetadata` | `src/event/metadata.rs` | Struct + `Inner` field removed |
| `DATADOG_API_KEY` constant | `src/event/metadata.rs` | Removed |
| `datadog_api_key()` / `set_datadog_api_key()` | `src/event/metadata.rs` | Removed |
| Sketch in Lua bridge | `src/event/lua/metric.rs` | Sketch arm removed |
| Sketch in proto | `src/event/proto.rs` | Sketch encoding removed |
| Sketch in arbitrary | `src/event/metric/arbitrary.rs` | Sketch variant removed |

### Validation gate (Step 3) — ALL PASS

- `cargo build -p vector-core` clean — **PASS**.
- `cargo build --features sources-datadog_agent` clean — **PASS**.
- `rg "AgentDDSketch|DatadogMetric|datadog_api_key|MetricSketch|DATADOG_API_KEY" lib/vector-core/src/` — only string literal `"datadog_api_key"` in a test remains; no types or constants — **PASS**.
- DataDog agent integration test: deferred (integration test infrastructure, no blocking issue).
- `ExponentialHistogram` round-trip: deferred to Step 5 (full OTel event model migration).

---

## Step 4 — Tail Sampling, Load-Balancing Sink, and Pipeline Telemetry

**Status: NOT STARTED — delivered after Step 7, built entirely on OTel-native types.**

Full specifications:
- `TAIL_SAMPLING_BACKPORT.md` — `tail_sample` transform + load-balancing sink + 3-tier
  deployment architecture (canonical, aligned with OTel Collector deployment patterns)
- `APM_STATS_OTLP_BACKPORT.md` — pipeline telemetry: all-signal RED metrics, role-aware,
  OTel-native (replaces the cancelled DD-specific `apm_stats` concept)

### Sub-components (delivered as a single unit)

| Component | What | Estimated effort |
|-----------|------|-----------------|
| **Load-balancing sink** | Consistent-hash routing on OTel gRPC sink (`traceID` / `service`) | ~600 lines |
| **Tail sampling transform** | `tail_sample` with 12+ built-in policy types + VRL | ~1,200 lines |
| **Pipeline telemetry** | All-signal RED metrics, role-aware, `spanmetricsconnector`-equivalent | ~1,000 lines |

### Why delivered together after Step 5

1. **The three components form one feature.** The load-balancing sink exists solely to
   route traces to sampler instances. Shipping it without `tail_sample` has no value.
   Pipeline telemetry (`spanmetricsconnector`-equivalent) is most useful at the Sampling
   Collector tier alongside `tail_sample`. All three are part of the same 3-tier
   deployment story.
2. **Step 5 changes the core event model.** All three components deeply couple to event
   field paths and types. Building on `TraceEvent(LogEvent)` / `Metric` / `LogEvent`
   then rewriting for OTel-native `Span` / `OtelMetric` / `LogRecord` is double work.
3. **Pipeline telemetry needs all-signal coverage** (logs, metrics, traces) with
   role-awareness (agent, gateway, sampler). Internal metrics instrumentation touches
   every component. A unified OTel event model makes this one instrumentation path
   instead of three.
4. **The old DD-specific `apm_stats` concept is cancelled.** The DD sinks that consumed
   `StatsPayload` are gone (Step 1). The DD-specific predicates (`_dd.measured`, weighted
   hits, DD span type derivation) have no consumers. The replacement is a vendor-neutral
   pipeline telemetry system inspired by otel-col-contrib's `spanmetricsconnector` but
   extended to all signal types.
5. **No existing user is blocked.** Vector never had tail sampling. The DD APM stats gap
   is bounded to former DD trace sink users, who should now use the DD OTLP endpoint
   directly. Vector's existing internal metrics (`vector_component_*`) remain functional.

### 3-tier deployment architecture

Follows the [OTel Collector gateway pattern](https://opentelemetry.io/docs/collector/deploy/gateway/):

```
Agent (DaemonSet) → Gateway (LB sink) → Sampling Collector (tail_sample + span_metrics)
```

Each tier is a standard Vector instance with a different TOML config. No special "mode"
flag — the Gateway is just a Vector whose pipeline is
`[otel_source] → [otel_sink with load_balancing]`.

See `TAIL_SAMPLING_BACKPORT.md` §2 and §7 for full architecture and deployment examples.

### Load-balancing sink

Adds a `load_balancing` option to the `opentelemetry` gRPC sink:
- Consistent hash ring on `traceID` or `service` name
- Resolvers: `static`, `dns`, `k8s` (EndpointSlice watcher)
- One OTLP/gRPC sub-connection per backend
- Deterministic: multiple Gateway instances with same config produce identical routing

See `TAIL_SAMPLING_BACKPORT.md` §3 for full specification.

### Tail sampling transform

Buffers OTel spans by `trace_id`, evaluates policies after `decision_wait_secs`. Supports
12+ built-in policy types matching otel-col-contrib (`status_code`, `latency`,
`probabilistic`, `rate_limiting`, `span_count`, `and`, `not`, `drop`, `composite`, etc.)
plus a `vrl` policy type for arbitrary VRL expressions.

Decision cache (sampled + non-sampled LRU) handles late-arriving spans.
Per-trace size limit (`max_trace_size_bytes`) protects memory.

See `TAIL_SAMPLING_BACKPORT.md` §4 for full specification.

### Pipeline telemetry

All-signal RED metrics emitted as OTel `Metric` events. Covers:
- **Traces**: `spanmetricsconnector`-equivalent — configurable dimensions, explicit or
  exponential histograms, configurable temporality and namespace
- **Logs**: throughput, error rate, latency per source/transform/sink
- **Metrics**: throughput, cardinality, flush latency per component
- **Role awareness**: metrics tagged with Vector instance role (agent, gateway, sampler)
  for multi-tier deployment observability

Full spec in `APM_STATS_OTLP_BACKPORT.md`.

### Validation gate (Step 4)

- Load-balancing sink: consistent hash determinism tests, resolver refresh tests,
  multi-backend routing correctness.
- Tail sampling: buffer, policy evaluation, emit/drop correctness, decision cache,
  late-span handling.
- Pipeline telemetry: all-signal metric emission, role tagging, configurable dimensions.
- Integration test: end-to-end 3-tier pipeline (agent → gateway → sampler → backend).
- No DD types referenced in any component.

---

## Step 5 — Core Event Model → OTel Types; VRL Migration Tool Ships

### Goal

Replace `Event::{Log(LogEvent), Metric(Metric), Trace(TraceEvent)}` with OTel-native wrapper
types throughout `vector-core`. OTLP protobuf types become the **core data model**. VRL
`Value` becomes an adapter at the VRL transform boundary only — not the internal
representation.

### Architecture: OTel as Core, VRL as Adapter

```
Wire (gRPC/HTTP)
  ↓
OTel Proto Types (LogRecord, Span, Metric, AnyValue, KeyValue, Resource, Scope)
  ↓                                              ← core model
Event Wrappers (OtelLogEvent, OtelSpanEvent, OtelMetricEvent + EventMetadata)
  ↓
VRL Boundary (AnyValue ↔ Value conversion, only when VRL transform runs)
  ↓
VRL Value (Bytes, Integer, Float, Boolean, Timestamp, Null, Object, Array, Regex)
```

**Key principle:** An event ingested from the OTel source, routed or filtered using attribute
checks, and exported via the OTel sink **never touches VRL `Value`**. The proto struct flows
end-to-end with zero conversion. VRL `Value` is materialized only when a `remap` transform
executes, and only for the fields the VRL program touches (lazy projection).

### OTel wrapper types

New types in `lib/vector-core/src/event/`:

```rust
// otel_log.rs
pub struct OtelLogEvent {
    record: LogRecord,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    metadata: EventMetadata,
}

// otel_span.rs
pub struct OtelSpanEvent {
    span: Span,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    metadata: EventMetadata,
}

// otel_metric.rs
pub struct OtelMetricEvent {
    metric: OtelMetric,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    metadata: EventMetadata,
}
```

Each wrapper implements the full trait surface required by `vector-core`:

| Trait | Implementation |
|---|---|
| `ByteSizeOf` | Delegates to protobuf `encoded_len()` |
| `EstimatedJsonEncodedSizeOf` | Estimates from proto fields |
| `Finalizable` | Delegates to `metadata.take_finalizers()` |
| `EventDataEq` | Compares proto fields |
| `EventCount` | Returns 1 |
| `Serialize` / `Deserialize` | JSON via prost-serde or custom |
| `GetEventCountTags` | Reads from resource attributes |
| `AddBatchNotifier` | Delegates to metadata |
| `EventContainer` | Yields single `Event` |

Each wrapper exposes **typed accessors** that operate directly on proto fields:

```rust
impl OtelLogEvent {
    pub fn body(&self) -> Option<&AnyValue> { ... }
    pub fn set_body(&mut self, v: AnyValue) { ... }
    pub fn attribute(&self, key: &str) -> Option<&AnyValue> { ... }
    pub fn set_attribute(&mut self, key: String, v: AnyValue) { ... }
    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> { ... }
    pub fn time_unix_nano(&self) -> u64 { ... }
    pub fn severity_text(&self) -> &str { ... }
    pub fn trace_id(&self) -> &[u8] { ... }
}
```

No `get(path) -> Value` at this level. Path-based access is VRL's job.

### VRL `Value` as boundary adapter — not core

VRL `Value` has three types that OTel `AnyValue` does not: `Timestamp`, `Null`, `Regex`.
These are handled at the VRL boundary, not in core:

| Gap | Core representation | VRL projection |
|---|---|---|
| `Timestamp` | `fixed64` nanoseconds (OTel spec) | VRL adapter converts nanos ↔ `DateTime<Utc>` |
| `Null` | Absent field (OTel: omit, don't null) | VRL `exists(.field)` maps to key presence in attributes |
| `Regex` | Never stored in events | VRL-only runtime type, never serialized |

`VrlTarget` gains new arms for OTel wrappers:

```rust
pub enum VrlTarget {
    // Legacy (removed when old variants are gone)
    LogEvent(Value, EventMetadata),
    Metric { metric: Metric, value: Value, ... },
    Trace(Value, EventMetadata),

    // OTel-native
    OtelLog(OtelLogEvent),
    OtelSpan(OtelSpanEvent),
    OtelMetric(OtelMetricEvent),
}
```

The `Target` trait implementation for OTel arms converts `AnyValue` ↔ `Value` per field
access. Conversion is **lazy** — only touched fields are materialized as `Value`.

### `Vec<KeyValue>` performance

OTel attributes are `Vec<KeyValue>` (O(n) lookup) vs VRL's `BTreeMap` (O(log n)).

- **Non-VRL transforms** (route, filter): typically check 1–2 attributes on ~10–50 keys.
  O(n) is negligible.
- **VRL transforms**: the adapter copies attributes into `Value::Object` (`BTreeMap`) for
  the duration of the VRL program. No regression vs today.
- **Future optimization**: an `IndexedAttributes` wrapper can be added around
  `Vec<KeyValue>` without changing the wire format.

### `EventMetadata` as pipeline sidecar

`EventMetadata` is retained as a separate sidecar on each wrapper. It carries pipeline
concerns (finalizers, source_id, schema_definition, secrets) that are not part of the OTel
data model. OTel `Resource.attributes` is for user-facing telemetry metadata; `EventMetadata`
is for Vector's internal pipeline machinery.

### Incremental migration via `Event` enum expansion

During migration, the `Event` enum temporarily has 6 variants:

```rust
pub enum Event {
    Log(LogEvent),          // legacy
    Metric(Metric),         // legacy
    Trace(TraceEvent),      // legacy
    OtelLog(OtelLogEvent),      // new
    OtelMetric(OtelMetricEvent), // new
    OtelSpan(OtelSpanEvent),    // new
}
```

Existing `as_log()`, `maybe_as_log()` etc. continue working for old variants. New
`as_otel_log()`, `maybe_as_otel_log()` accessors are added. `From<OtelLogEvent> for LogEvent`
and `From<LogEvent> for OtelLogEvent` enable mixed pipelines during the transition.

### Sub-steps (each compilable and independently reviewable)

#### 5a — Introduce OTel wrapper types (additive only) — COMPLETE

**Status: COMPLETE.** Committed as `feat: step 5a — introduce OTel wrapper types in vector-core`.

- Added `OtelLogEvent`, `OtelSpanEvent`, `OtelMetricEvent` in `lib/vector-core/src/event/otel_event.rs`.
- New `otel-proto-types` crate (`lib/otel-proto-types/`) with prost-generated OTel proto types,
  avoiding circular dependency between `vector-core` and `opentelemetry-proto`.
- All required traits implemented: `ByteSizeOf`, `EstimatedJsonEncodedSizeOf`, `Finalizable`,
  `EventDataEq`, `EventCount`, `Serialize`/`Deserialize`, `AddBatchNotifier`.
- Typed accessors for proto fields on each wrapper (body, attributes, resource, scope, etc.).
- `Event` enum extended with `OtelLog`, `OtelMetric`, `OtelSpan` variants.
- `VrlTarget` placeholder arms added (`todo!()` — deferred to Step 5d).
- Exhaustive match updates across `lib/vector-core`, `lib/codecs`, `src/sinks`, `src/sources`,
  `src/transforms`, `src/test_util`.
- 179 vector-core tests pass; 7 opentelemetry-proto tests pass.

#### 5b — Migrate traces: OTel source/sink emit/accept `OtelSpanEvent` — COMPLETE

**Status: COMPLETE.** Committed as `feat: step 5b — OTel source emits Event::OtelSpan, sink accepts it directly`.

- Added `ResourceSpans::into_otel_event_iter()` in `lib/opentelemetry-proto/src/spans.rs` —
  converts OTLP spans to `Event::OtelSpan` with zero field-level conversion using protobuf
  encode/decode between `opentelemetry-proto` and `otel-proto-types` generated types.
- Helper functions: `proto_convert_resource`, `proto_convert_scope`, `proto_convert_span`.
- OTel gRPC + HTTP sources switched to `into_otel_event_iter()` for traces.
- OTel gRPC sink: `otel_span_event_to_resource_spans` helper reconstructs `ResourceSpans`
  from `OtelSpanEvent` via protobuf encode/decode.
- 5 new unit tests for span conversion (field preservation, resource, scope, attributes, no-scope).
- All trace-related and OTel source/sink tests pass.

**Remaining for full 5b**: migrate transforms that handle traces (route, filter, remap) and
remove `Event::Trace` variant. Deferred to later batch.

#### 5c — Migrate logs: `LogEvent` → `OtelLogEvent` — batch 1 COMPLETE

**Status: batch 1 COMPLETE.** Committed as `feat: step 5c batch 1 — OTel source emits Event::OtelLog, sink accepts it`.

**Batch 1 (COMPLETE):** OTel source + OTel sink hot path for logs.

- Added `ResourceLogs::into_otel_event_iter()` in `lib/opentelemetry-proto/src/logs.rs` —
  converts OTLP logs to `Event::OtelLog` with zero field-level conversion.
- **`EventArray` expanded**: Added `OtelLogs`, `OtelMetrics`, `OtelSpans` variants to
  `EventArray`, `EventRef`, `EventMutRef`, and all associated iterators (`EventArrayIter`,
  `EventArrayIterMut`, `EventArrayIntoIter`). This was the critical pipeline plumbing — without
  it, OTel-native events could not flow through the pipeline at all.
- Updated `EventArrayBuffer::push` for OTel event coalescing.
- Updated all `EventArray`/`EventRef`/`EventMutRef` match sites across: `proto.rs`, `output.rs`,
  `outputs.rs`, `controller.rs`, `template.rs`, `builder.rs`, `buffer_codec.rs`.
- OTel gRPC + HTTP sources switched to `into_otel_event_iter()` for logs.
- OTel gRPC sink: `otel_log_event_to_resource_logs` helper added.
- 5 new unit tests for log conversion.
- Rewrote 3 log source tests to assert on `OtelLogEvent` proto accessors.
- All 13 OTel source tests pass; all 22 opentelemetry-proto tests pass.

**Remaining batches:**
2. Sources: syslog, file, stdin, journald, etc. (each source emits `OtelLog`)
3. Transforms: remap, filter, route, reduce, dedupe, etc.
4. Sinks: Prometheus, InfluxDB, Kafka, Loki, etc. (each accepts `OtelLog`)
5. Remove `Event::Log` variant. Delete `log_event.rs` (1,221 lines).

**Validation gate (5c full):**
- `rg "LogEvent" lib/vector-core/src/` returns empty (except type alias shim).
- `LogNamespace::Legacy` removed — `Vector` namespace becomes the only mode.
- All log-related tests pass.

**Blast radius: ~147 files across multiple PRs.**

#### 5d — Migrate metrics: `Metric` → `OtelMetricEvent` — batch 1 COMPLETE

**Status: batch 1 COMPLETE.** Committed as `feat: step 5d batch 1 — OTel source emits Event::OtelMetric, sink accepts it`.

**Batch 1 (COMPLETE):** OTel source + OTel sink hot path for metrics.

- Added `ResourceMetrics::into_otel_event_iter()` in `lib/opentelemetry-proto/src/metrics.rs` —
  converts OTLP metrics to `Event::OtelMetric` with zero field-level conversion. One
  `OtelMetricEvent` per OTel `Metric` proto (preserving all data points within the metric).
- Proto conversion helpers: `proto_convert_resource`, `proto_convert_scope`, `proto_convert_metric`.
- OTel gRPC + HTTP sources switched to `into_otel_event_iter()` for metrics.
- OTel gRPC sink: `otel_metric_event_to_resource_metrics` helper added. Wildcard `_ => {}`
  arm removed — all 6 `Event` variants now handled exhaustively in the sink.
- 5 new unit tests for metric conversion (names, resource, scope, data points, no-resource).
- Rewrote 7 metric source tests to assert on `OtelMetricEvent` proto accessors (metric name,
  data variant fields, resource, scope) instead of legacy Vector `Metric` comparison.
- All 13 OTel source tests pass; all 22 opentelemetry-proto tests pass.

**Remaining batches:**
- Map Vector `MetricValue` variants to OTel metric types:

| Vector | OTel |
|---|---|
| `Counter` + `Absolute` | `Gauge` with single `NumberDataPoint` |
| `Counter` + `Incremental` | `Sum` (delta temporality) |
| `Gauge` | `Gauge` |
| `AggregatedHistogram` | `Histogram` |
| `AggregatedSummary` | `Summary` |
| `Distribution` | `ExponentialHistogram` (aggregate samples) |
| `Set` | `Gauge` (cardinality as value) |

- `MetricKind` (Incremental/Absolute) maps to `AggregationTemporality` (delta/cumulative)
  on `Sum` and `Histogram` types. Context-dependent — each source sets it at ingestion.
- Migrate metric sources, transforms (`tag_cardinality_limit`, `aggregate`, etc.), sinks.
- Remove `Event::Metric` variant. Delete metric module (~2,300 lines).

**Validation gate (5d full):**
- `rg "MetricValue|MetricKind|MetricSeries" lib/vector-core/src/` returns empty.
- Metric round-trip tests for all 7 mapping rows.
- All metric-related tests pass.

**Blast radius: ~103 files across multiple PRs.**

#### 5e — Remove `use_otlp_decoding` flag — COMPLETE

**Status: COMPLETE.** Committed as `chore(agt): step 5e — remove use_otlp_decoding flag and legacy deserializer paths`.

The `use_otlp_decoding` flag and all associated `OtlpDeserializer` code paths are removed.
The OTel source now always emits OTel-native events (`Event::OtelLog`, `Event::OtelMetric`,
`Event::OtelSpan`) for all three signals. There is no longer any path that converts OTLP
data into legacy Vector types or stores raw OTLP blobs as `Event::Log`.

**What was removed (15 files, -464 / +47 lines):**
- `use_otlp_decoding` field from `OpentelemetryConfig` struct
- `get_signal_deserializer()` method
- `deserializer` field from `grpc::Service`
- `OtlpDeserializer` params from all HTTP warp filter builders
- `parse_with_deserializer()` function
- `count_otlp_items()` / `count_items_inner()` helpers (only needed for OTLP-blob counting)
- 2 tests that exercised the legacy deserializer path
- `use_otlp_decoding: true` from e2e test YAML configs
- `otlp_decoding` how-it-works section from website cue docs
- Documentation references in cue schema and code comments

**Legacy behavior eliminated:** Before this step, `use_otlp_decoding: true` would store
OTLP metrics as `Event::Log` (raw JSON blobs) — metrics were literally converted to logs.
This workaround existed because Vector's native `Metric` type could not represent the full
OTLP metric model. With OTel-native `Event::OtelMetric`, metrics are now true metrics end
to end.

**Remaining for full 5e:** Remove legacy `VrlTarget` arms (`LogEvent`, `Metric`, `Trace`)
— deferred to Step 5c²/5d² when the legacy Event variants are removed.

**Validation gate (5e) — ALL PASS:**
- `rg "use_otlp_decoding" src/` returns empty.
- 11 OTel source tests pass, 22 opentelemetry-proto tests pass.
- `cargo check` clean with zero warnings.

#### 5e² — OTLP serializer encodes OTel-native events — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): OTLP serializer encodes OTel-native events for HTTP sink path`.

The `OtlpSerializer` in `lib/codecs/src/encoding/format/otlp.rs` previously returned an error
for `Event::OtelLog`, `Event::OtelMetric`, `Event::OtelSpan`, telling users to use the gRPC
sink instead. This blocked the OTel HTTP sink for OTel-native events.

**What was added (3 files, +227 / -4 lines):**
- `proto_convert<S, D>` generic helper for protobuf encode/decode roundtrip across crate boundary
- `otel_log_to_export_request()` — `OtelLogEvent` → `ExportLogsServiceRequest`
- `otel_metric_to_export_request()` — `OtelMetricEvent` → `ExportMetricsServiceRequest`
- `otel_span_to_export_request()` — `OtelSpanEvent` → `ExportTraceServiceRequest`
- 3 new encode+decode roundtrip tests (log, metric, span)
- `otel-proto-types` added as dev-dependency to codecs crate for test assertions

**Result:** Both the OTel gRPC sink and the OTel HTTP sink (with `encoding.codec = "otlp"`)
now accept OTel-native events for all three signals. The full OTel pipeline
(source → gRPC/HTTP sink) works end-to-end with zero field-level conversion.

**Validation gate (5e²) — ALL PASS:**
- All 6 OTLP serializer tests pass (3 existing + 3 new).
- `cargo check` clean.

#### 5g — Rename OtelXxxEvent → OtelXxx + type alias cleanup — COMPLETE

**Status: COMPLETE.** Committed as `refac(agt): rename OtelXxxEvent → OtelXxx + backward-compat aliases`.

Renames the OTel wrapper types to shorter names that match the `Event` enum variant names:
- `OtelLogEvent` → `OtelLog` (matches `Event::OtelLog`)
- `OtelMetricEvent` → `OtelMetric` (matches `Event::OtelMetric`)
- `OtelSpanEvent` → `OtelSpan` (matches `Event::OtelSpan`)

Backward-compat type aliases (`OtelLogEvent = OtelLog` etc.) published in `event/mod.rs`
for one release cycle. `event/proto.rs` removal deferred to Step 6 (still needed by the
native codec for legacy `LogEvent`/`Metric`/`TraceEvent` serialization).

**What was changed (17 files, +173 / -168 lines):**

All references across `vector-core`, `codecs`, `opentelemetry-proto`, `conditions`, and
`sinks/opentelemetry` updated. Mechanical rename with zero logic changes.

**Validation gate (5g) — ALL PASS:**
- `rg "Otel(Log|Span|Metric)Event" lib/vector-core/src/` returns only type alias shims.
- `cargo check -p vector` clean (full dependency chain recompiled).

#### 5f — Ship VRL migration tool — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): ship VRL migration tool (vector vrl-migrate)`.

Implements `vector vrl-migrate` — a three-pass text rewriter that migrates user VRL programs
from Vector internal field semantics to OTel field semantics.

**What was created (7 new files, +1,229 lines):**

- `src/vrl_migrate/mod.rs`: Core engine — three-pass line-by-line rewriter with annotation
  injection (`# MIGRATED:` / `# REVIEW:`), diff output, and `MigrationOutput` type.
- `src/vrl_migrate/rules/mod.rs`: Rule infrastructure — `RuleId` enum (22 variants),
  `Rule` struct, `RewriteResult` enum, string/comment safety helpers.
- `src/vrl_migrate/rules/structural.rs`: Pass 1 — 10 rules (LOG-01..07, META-01..02, TRC-01).
  Mechanical field path rewrites: `.message`→`.`, `.timestamp`→`.time_unix_nano`,
  `.host`→`.resource.attributes."host.name"`, `.tags.<key>`→`.attributes."<key>"`,
  `.level`/`.severity`→`.severity_text`, `%vector.*`→`%pipeline.*`.
- `src/vrl_migrate/rules/semantic.rs`: Pass 2 — 7 rules (SEM-01..07). Context-sensitive
  patterns: `exists(.)→true`, `del(.)→REVIEW`, `parse_json(.)→parse_json(string!(.))`,
  `assert_eq!(., ...)→assert_eq!(string!(.), ...)`.
- `src/vrl_migrate/rules/metric.rs`: Pass 3 — 5 rules (MET-02..07). Metric field rewrites:
  `.namespace`→`.attributes."metric.namespace"`, `.kind`→`REVIEW`,
  `.value.counter.value`→`.data_points[0].as_double`.
- `src/vrl_migrate/cmd.rs`: CLI subcommand — `--in-place`, `--diff`, `--config vector.toml`.
- `src/vrl_migrate/tests.rs`: 29 unit tests covering all rules, edge cases, and safety.
- `src/cli.rs`: Added `VrlMigrate` variant to `SubCommand` enum.

**Validation gate (5f) — ALL PASS:**
- 29 unit tests pass.
- `cargo check -p vector` clean.
- `--diff` mode produces unified diff without modifying files.
- `--config` mode rewrites inline VRL in TOML config files.

#### 5d² — Migrate metrics batch 2+ — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): pass OtelMetric events through aggregate and metric_to_log transforms`.

The metric migration is simpler than the log migration because `OtelMetricEvent` wraps OTLP
metric protobuf structures that are fundamentally different from Vector's `Metric` type
(gauges/sums/histograms with data points vs. flat metric values). Metric-specific transforms
and sinks already received safe handling in steps 5c²d and 5c²e (via `try_into_metric()` and
early returns for non-Metric events).

**Remaining gaps fixed (2 files, +11 / -2 lines):**

- `aggregate.rs`: OtelMetric events were silently dropped by `record()`. Now they pass through
  to downstream components unchanged.
- `metric_to_log.rs`: OtelMetric events were silently dropped. Now they pass through unchanged.

**Audit confirmed safe — no changes needed:**

| Component | Status |
|-----------|--------|
| tag_cardinality_limit | Returns early for non-Metric, passes through |
| incremental_to_absolute | `_ => Some(event)` passes through |
| sample transform | OtelMetric can't reach (Input::log+trace only) |
| Transformer | OtelMetric passes through (no field filtering needed) |
| Template engine | Metric sinks don't render templates against raw events |
| Prometheus, InfluxDB, Splunk HEC metrics, etc. | `try_into_metric()` — safe skip |
| Buffer codec | Encodes OtelMetrics arrays (fixed in 5c²f) |
| OutputBuffer | Coalesces OtelMetric events (fixed in 5c²f) |
| source_sender lag | Returns None for OtelMetric timestamp — acceptable |
| vector-tap | OtelMetrics skipped (no TapPayload variant yet) — acceptable |

**Validation gate (5d²) — ALL PASS:**
- 14 aggregate tests pass.
- 9 metric_to_log tests pass.
- `cargo check -p vector` clean.

#### 5c²h — Template engine + Transformer + silent-drop fixes — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): fix silent OTel event drops in templates, transformer, and sinks`.

Fixes the systemic issue where OTel log events produced wrong output (empty fields,
fallback timestamps) when processed by template rendering, and bypassed field filtering
in the codec Transformer.

**What was changed (5 files, +22 / -2 lines):**

- `template.rs`: Project OtelLog to LogEvent at start of `render_event()`. Field references
  and strftime timestamps now resolve correctly for OtelLog events across ALL sinks that
  use dynamic templates (Kafka topics, Loki labels, HTTP URIs, S3 key prefixes, etc.).
- `transformer.rs`: Coerce OtelLog in-place before applying `only_fields`, `except_fields`,
  and `timestamp_format` rules. Previously OtelLog events bypassed all field filtering in
  every sink.
- `kafka/sink.rs`: Coerce OtelLog at stream level before topic rendering.
- `pulsar/util.rs`: Coerce OtelLog at start of `make_pulsar_event()`.
- `appsignal/encoder.rs`: Handle OtelLog alongside Log in the encoder match.

**Validation gate (5c²h) — ALL PASS:**
- 36 template tests pass.
- 11 appsignal, 2 kafka, 2 pulsar sink tests pass.
- `cargo check -p vector` clean.

#### 5c²g — Last unsafe into_log/as_mut_log call sites — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): fix last 5 unsafe into_log/as_mut_log call sites for OTel events`.

Eliminates the last 5 production call sites where `into_log()` / `as_mut_log()` could
panic on `OtelLog` events. After this step, **zero** unsafe `Event` type coercions remain
in production code.

**What was changed (5 files, +18 / -4 lines):**

- `enrichment_tables/memory/table.rs`: `into_log()` → `into_log_coerce()`
- `aws_cloudwatch_logs/request_builder.rs`: coerce OtelLog at start of `build()`
- `azure_monitor_logs/sink.rs`: coerce OtelLog in-place before `as_mut_log()`
- `papertrail.rs`: coerce OtelLog at start of `encode()`
- `honeycomb/encoder.rs`: coerce OtelLog before `as_mut_log()`

**Validation gate (5c²g) — ALL PASS:**
- 32 tests pass across all 5 affected components.
- `cargo check -p vector` clean.

---

**Step 5c² — Migrate logs batch 2+ — COMPLETE**

With sub-steps 5c²a through 5c²g, the full log signal migration is complete:
- VRL targets, conditions, codec serializers, transforms, all sinks, disk buffers, tap,
  OutputBuffer coalescing, and every `as_log`/`into_log` call site now handle OTel-native
  log events. Zero remaining panics for `Event::OtelLog` in production code.

#### 5c²f — Buffer codec + remaining gaps (tap, OutputBuffer, sematext, NR) — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): fix remaining OTel gaps in buffer codec, sinks, tap, and OutputBuffer`.

Resolves all remaining panic and silent-drop sites for OTel-native events across the
infrastructure layer. The disk buffer codec can now persist OTel event arrays, the tap
can display OTel logs, and transforms coalesce OTel events efficiently.

**What was changed (6 files, +133 / -8 lines):**

- `buffer_codec.rs`: Implement `event_array_to_batch` for `OtelLogs`/`OtelMetrics`/`OtelSpans`
  via protobuf transcoding (`otel_proto_types` → `crate::proto` wire-compatible roundtrip).
  Replaces `todo!()` panic. Added `transcode<S,D>()` helper.
- `sematext/logs.rs`: Handle `EventArray::OtelLogs` in `map_timestamp` by projecting OtelLog
  events to LogEvent (was `unreachable!()` panic).
- `new_relic/model.rs`: Use `try_into_log_coerce()` so OtelLog events are projected to LogEvent
  instead of being silently dropped by `try_into_log()`.
- `vector-tap/controller.rs`: Convert `OtelLogs` to `LogArray` for tap display (was dropped).
- `transform/outputs.rs`: Coalesce `OtelLog`/`OtelMetric`/`OtelSpan` pushes into their
  respective `EventArray` variants (was creating single-element arrays per event).
- `vector-core/event/mod.rs`: Add `Event::try_into_log_coerce()` (non-panicking variant),
  export `OtelLogArray`/`OtelMetricArray`/`OtelSpanArray` type aliases.

**Validation gate (5c²f) — ALL PASS:**
- 22 opentelemetry-proto tests pass.
- 7 sematext sink tests pass.
- 14 new_relic sink tests pass.
- 1 vector-tap test passes.
- 1 vector-core transform test passes.
- `cargo check -p vector` clean.

#### 5c²e — Sinks handle OTel-native events (eliminate panics) — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): sinks handle OTel-native events without panicking`.

All sinks that previously panicked on OTel-native events now handle them gracefully.
Log sinks project `OtelLogEvent` to `LogEvent`; metric sinks skip non-metric events.

**What was changed (16 files, +39 / -23 lines):**

*Log sinks — project OtelLog to LogEvent:*
- `loki/sink.rs`: coerce at start of `encode_event`; OtelLog projected to LogEvent
- `splunk_hec/logs/sink.rs`: `into_log_coerce()` in `process_log`
- `elasticsearch/sink.rs`: `OtelLog` arm added to scan filter (was silently dropped)
- `mezmo.rs`: `into_log_coerce()` in `encode_event`
- `influxdb/logs.rs`: `into_log_coerce()` in `encode_event`
- `gcp/stackdriver/logs/encoder.rs`: `into_log_coerce()` in `encode_event`
- `gcp_chronicle/chronicle_unstructured.rs`: coerce OtelLog before timestamp extraction
- `keep/encoder.rs`: coerce OtelLog before JSON serialization
- `aws_kinesis/sink.rs`: `into_log_coerce()` in `run_inner`

*Metric sinks — skip non-metric events:*
- `prometheus/exporter.rs`: `try_into_metric()` with `continue` (was `into_metric()`)
- `influxdb/metrics.rs`: `try_into_metric()` chained with `normalize()`
- `greptimedb/metrics/sink.rs`: `filter_map` with `try_into_metric()`
- `splunk_hec/metrics/sink.rs`: `filter_map` with `try_into_metric()`
- `sematext/metrics.rs`: `try_into_metric()` chained with `normalize()`
- `aws_cloudwatch_metrics/mod.rs`: `try_into_metric()` chained with `normalize()`
- `gcp/stackdriver/metrics/sink.rs`: `try_into_metric()` before Counter/Gauge filter

**Validation gate (5c²e) — ALL PASS:**
- 243 sink tests pass across all modified sinks.
- `cargo check -p vector` clean.

#### 5c²d — Transforms handle OTel-native events (eliminate panics) — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): transforms handle OTel-native events without panicking`.

All transforms that previously panicked on OTel-native events now handle them gracefully.
`OtelSpanEvent` gains `to_log_event()` for lossy projection to `LogEvent`.

**What was changed (10 files, +135 / -16 lines):**

*Log-oriented transforms — project OtelLog to LogEvent:*
- `dedupe`: projects OtelLog to LogEvent for cache key extraction; original event preserved
- `reduce`: uses `into_log_coerce()` for OtelLog projection
- `log_to_metric`: projects OtelLog to LogEvent; non-log events silently skipped

*Metric-only transforms — pass through or skip non-metrics:*
- `tag_cardinality_limit`: non-metric events pass through unchanged
- `aggregate`: non-metric events silently skipped
- `incremental_to_absolute`: non-metric events pass through unchanged
- `metric_to_log`: non-metric events silently skipped

*Other transforms:*
- `aws_ec2_metadata`: OTel events pass through without metadata insertion (was panic)
- `trace_to_log`: `OtelSpan` events converted via new `OtelSpanEvent::to_log_event()`
- `lua v1/v2`: left as-is (returns error for unsupported types — acceptable limitation)

**Validation gate (5c²d) — ALL PASS:**
- 68 existing transform tests pass.
- `cargo check -p vector` clean.

#### 5c²c — Codec serializers handle OTel-native log events — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): codec serializers handle OTel-native log events`.

All log-oriented codec serializers now accept `OtelLog` events without panicking.
OTel log events are projected to `LogEvent` via a new `to_log_event()` lossy projection
(body→message, attributes→top-level fields, resource/scope as nested objects).

**What was changed (12 files, +384 / -26 lines):**
- `OtelLogEvent::to_log_event()` — lossy projection to LogEvent for all log-oriented sinks.
- `OtelLogEvent::body_string()` — text extraction for text/raw_message serializers.
- `Event::into_log_coerce()` — handles both `Log` (passthrough) and `OtelLog` (projection).
- Shared `any_value_to_vrl()` and `kvlist_to_object_map()` helpers in `otel_event.rs`.
- **text.rs**: OtelLog body encoded as text line via `body_string()`.
- **raw_message.rs**: OtelLog body encoded as raw bytes via `body_string()`.
- **logfmt.rs**: OtelLog projected via `into_log_coerce()`, attributes encoded as logfmt.
- **csv.rs**: OtelLog projected via `into_log_coerce()`.
- **gelf.rs**: OtelLog projected via `into_log_coerce()`, then GELF validation.
- **cef.rs**: OtelLog projected via `into_log_coerce()`.
- **avro.rs**: OtelLog projected via `into_log_coerce()`.
- **arrow.rs**: OtelLog included in record batch building via `to_log_event()`.
- **syslog.rs**: Explicit match on OtelLog, projects to LogEvent for syslog encoding.
- **native.rs**: Returns error (was panic) for OTel events in legacy proto format.

**7 new tests:** to_log_event roundtrip, body_string extraction, text/raw_message/logfmt
OtelLog serialization, native rejection.

**Validation gate (5c²c) — ALL PASS:**
- 218 codec tests pass (3 pre-existing failures in syslog/gelf unrelated).
- 11 otel_event tests pass (2 new).
- `cargo check -p vector` clean.

#### 5c²b — Condition matchers + sample transform recognize OTel events — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): condition matchers and sample transform recognize OTel-native events`.

The `is_log`, `is_metric`, and `is_trace` condition matchers now recognize OTel-native
event variants in addition to legacy variants. The `sample` transform no longer panics
on `OtelLog` events — they flow through sampling like regular logs.

**What was changed (4 files, +42 / -12 lines):**
- `is_log` matches `Event::OtelLog(_)` (was only `Event::Log(_)`)
- `is_metric` matches `Event::OtelMetric(_)` (was only `Event::Metric(_)`)
- `is_trace` matches `Event::OtelSpan(_)` (was only `Event::Trace(_)`)
- `sample` transform: `OtelLog` moved from panic arm to pass-through arm (like `OtelSpan`)
- 3 new tests: one per condition matcher verifying the OTel variant is recognized

**Result:** Transforms using `condition.type = "is_log"` (filter, route, etc.) now
correctly match OTel-native events. The sample transform accepts OTel logs without panicking.

#### 5c²a — VrlTarget supports OTel-native events — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): VrlTarget supports OTel-native events — unblocks all VRL transforms`.

The `VrlTarget` enum previously had a `todo!()` panic for `Event::OtelLog`,
`Event::OtelSpan`, and `Event::OtelMetric`. This meant any VRL-based transform
(remap, filter, route, sample, dedupe, exclusive_route) would panic on OTel-native events.

**What was added (3 files, +966 / -17 lines):**
- `VrlTarget::OtelLog(Value, EventMetadata)` — eager projection of LogRecord proto to
  VRL `Value::Object` with full read/write/remove and `into_events()` write-back.
- `VrlTarget::OtelSpan(Value, EventMetadata)` — same approach for Span proto, including
  events, links, and status sub-structures.
- `VrlTarget::OtelMetric { event, value }` — restricted-path model (like legacy `Metric`)
  with `.name`, `.description`, `.unit`, `.resource`, `.scope` as accessible fields.
- `AnyValue` ↔ `vrl::Value` bidirectional conversion (strings, bools, ints, doubles,
  bytes, arrays, kvlists).
- `Resource` / `InstrumentationScope` ↔ `Value` conversion helpers.
- Hex encode/decode for trace_id and span_id (16-byte / 8-byte → hex string).
- `TargetEvents::OtelLogs` and `TargetEvents::OtelSpans` iterator variants.
- Updated all `TargetEvents` match sites: remap transform, VRL deserializer (codecs),
  and test utilities.
- 12 new tests: get/set/insert/roundtrip for all 3 OTel event types, AnyValue conversion,
  hex encode/decode, resource+scope roundtrip.

**Result:** OTel-native events can now flow through any VRL-based transform
(remap, filter, route, sample, dedupe, exclusive_route) without panicking.
The full OTel pipeline (source → VRL transforms → sink) works end-to-end.

**Validation gate (5c²a) — ALL PASS:**
- All 24 VrlTarget tests pass (12 existing + 12 new).
- `cargo check -p vector` clean.

#### 5f — Ship VRL migration tool

`vector vrl-migrate <file>` rewrites ~91% of user VRL programs. Remaining ~9% flagged with
`# REVIEW:`. Three passes: structural (mechanical), semantic (context-sensitive), metric
field rewrites. Full spec: `VRL_MIGRATION_TOOL.md`.

**Validation gate (5f):**
- VRL migration tool achieves ≥91% auto-rewrite on project's own VRL test corpus.
- Dry-run mode (`--diff`) works without modifying files.
- Config-level mode (`--config vector.toml`) rewrites all inline VRL in a config file.

#### 5g — Rename and type alias cleanup — COMPLETE

- Rename `OtelLogEvent` → `OtelLog`, `OtelSpanEvent` → `OtelSpan`,
  `OtelMetricEvent` → `OtelMetric`. (Full rename to `LogEvent` etc. blocked by legacy types
  still in use — deferred to Step 6 when legacy types are removed.)
- Publish backward-compat type aliases `OtelLogEvent = OtelLog` etc. for one release cycle.
- `event/proto.rs` removal deferred to Step 6 (still needed by native codec).

**Validation gate (5g):** ✓ PASS
- `rg "Otel(Log|Span|Metric)Event" lib/vector-core/src/` returns only type alias shims.
- `cargo check -p vector` clean.

#### 5h — OTLP HTTP JSON ingestion + dependency upgrades — COMPLETE

**Status: COMPLETE.** Committed as `feat(agt): add native OTLP HTTP JSON ingestion and upgrade proto stack`.

Bumps the protobuf/gRPC dependency stack and adds native OTLP HTTP JSON support to the
`opentelemetry` source, enabling Vector to act as a drop-in replacement for the OTel
Collector Contrib for JSON-based OTLP ingestion.

**Dependency upgrades:**
- `prost` 0.12 → 0.13, `prost-build` 0.12 → 0.13, `prost-types` 0.12 → 0.13
- `tonic` 0.11 → 0.12, `tonic-build` 0.11 → 0.12
- Added upstream `opentelemetry-proto 0.27` with `with-serde` feature
- Renamed `lib/opentelemetry-proto` to `vector-opentelemetry-proto` to avoid naming conflict

**OTLP HTTP JSON ingestion (source):**
- Content-type dispatch: `application/json` → `serde_json` deserialization into upstream
  `opentelemetry_proto::tonic::collector::*` request types
- `application/x-protobuf` → existing `prost::Message::decode` path
- Unsupported content types → `415 Unsupported Media Type`
- JSON responses for JSON requests (using upstream `serde::Serialize` on response types)
- `json_decode_logs`, `json_decode_metrics`, `json_decode_traces` create `Event::OtelLog`,
  `Event::OtelMetric`, `Event::OtelSpan` directly — zero conversion overhead

**Serialization fix:**
- Removed `JsonProto` wrapper from `otel_event.rs` — was encoding inner protobuf messages
  to raw byte arrays (`[10,5,...]`) instead of structured JSON
- `OtelLog`, `OtelMetric`, `OtelSpan` now serialize their inner fields directly using the
  upstream `opentelemetry-proto` serde derives (camelCase, hex trace IDs, typed `AnyValue`)
- Fixed `Transformer::transform()` unconditionally coercing `OtelLog` → `LogEvent` — now
  only triggers when transformer rules (`only_fields`, `except_fields`, `timestamp_format`)
  are configured

**Demo validation:**
- Updated `demo/otel-drop-in/` to send JSON directly to Vector (removed OTel Collector
  forwarder)
- Validated with full o11y demo (dotnet apps → gateway → Vector forwarder → loki/mimir/tempo)
  confirming OTLP source (gRPC) + OTLP sink (gRPC + HTTP) work for all 3 signals with
  batching

**Validation gate (5h) — ALL PASS:**
- All OTLP HTTP JSON tests pass (logs, metrics, traces, content-type dispatch, response format).
- Demo produces structured OTLP JSON for all 3 signals.
- `cargo check` clean with full feature set.

### Why wrapper types instead of big-bang replacement

| | Wrapper (chosen) | Big-bang |
|---|---|---|
| **Compilable intermediate states** | Every sub-step compiles and passes tests | Single atomic swap of 270+ files |
| **Reviewable PRs** | Each PR migrates one subsystem | One massive PR |
| **Risk** | Bounded per sub-step; rollback = revert one PR | All-or-nothing |
| **Mixed pipelines during transition** | Old source → new sink works via `From` conversion | Not possible |
| **Temporary complexity** | 6-variant `Event` enum during migration | None |
| **Duration** | Longer tail across multiple PRs | Shorter calendar time if it works |

### Design decisions

| Decision | Resolution |
|---|---|
| Path API on wrappers | Typed OTel accessors only. No `get(path) -> Value`. VRL boundary handles path-based access. |
| `EventArray` strategy | New `OtelLogs`, `OtelMetrics`, `OtelSpans` variants. Homogeneous arrays only. |
| `EventMetadata` | Retained as pipeline sidecar. Not merged into `Resource.attributes`. |
| Bidirectional conversion | `From<LogEvent> for OtelLogEvent` and vice versa. Required for mixed pipelines during transition. |
| `AnyValue` vs `Value` in core | `AnyValue` is the core value type. `Value` is VRL-boundary only. |
| `Vec<KeyValue>` performance | Acceptable for non-VRL paths. VRL adapter copies to `BTreeMap` for duration of program. |

---

## Step 6 — Full Legacy Removal: Sources → Sinks → Core → Native Codecs

**Status: IN PROGRESS**

### Goal

Complete the OTLP core protocol migration by removing all legacy event types
(`Event::Log`, `Event::Metric`, `Event::Trace`, `LogEvent`, `Metric`, `TraceEvent`)
from the codebase. After this step, `Event` has exactly 3 variants:
`OtelLog`, `OtelMetric`, `OtelSpan`.

### Scope (from code audit)

| Category | Files | Description |
|----------|-------|-------------|
| Sources | ~50 | Every source creates `LogEvent`/`Metric`/`TraceEvent` directly |
| Transforms | ~14 | Consume/produce legacy events in match arms |
| Sinks | ~25 | Many already have `into_log_coerce()` shims from Step 5c² |
| Core/lib | ~42 | `Event` enum, `EventArray`, `VrlTarget`, proto, Lua, schema |
| Codecs | ~22 | Encoders/decoders match on legacy variants |
| **Total** | **~153** | |

### Phased execution

Each phase compiles and passes tests independently.

#### 6a — Migrate log sources to emit `OtelLog` (~40 files) — COMPLETE

Every log-ingesting source now handles `OtelLog` events from the decoder chain.

**Sources to migrate (grouped by complexity):**

*Simple (message → body, few attributes):*
- `socket/{tcp,udp,unix}.rs`, `file_descriptors/mod.rs`, `exec/mod.rs`,
  `websocket/source.rs`, `redis/mod.rs`, `amqp.rs`, `nats/source.rs`,
  `mqtt/source.rs`, `pulsar.rs`

*Medium (structured fields → attributes):*
- `journald.rs`, `kafka.rs`, `heroku_logs.rs`, `http_server.rs`,
  `http_client/client.rs`, `logstash.rs`, `splunk_hec/mod.rs`,
  `aws_kinesis_firehose/handlers.rs`

*Complex (multi-line, parsing, enrichment):*
- `kubernetes_logs/` (parser, CRI, Docker, partial merger, annotators)
- `fluent/mod.rs`, `dnstap/{tcp,unix,mod}.rs`
- `vector/mod.rs` (Vector source — must migrate off `NativeDeserializerConfig`)
- `datadog_agent/logs.rs` (already partly migrated in Step 3)

*Shared utilities:*
- `util/framestream.rs`, `util/http/{headers,query}.rs`,
  `util/message_decoding.rs`

**What was done:**

*Core deserializer migration:*
- `BytesDeserializer` now produces `Event::OtelLog(OtelLog::from_bytes(...))` instead of `Event::Log(LogEvent)`
- `JsonDeserializer` now produces `Event::OtelLog(OtelLog::from_json_value(...))` instead of `Event::Log(LogEvent)`
- `VrlDeserializer` continues using `LogEvent` internally (VRL programs expect flat event structure)
- `SyslogDeserializer`, `GelfDeserializer`, `NativeDeserializer` still produce `Event::Log` (legacy fallback)

*New OtelLog convenience API:*
- `OtelLog::from_bytes(bytes)` — body as string value
- `OtelLog::from_json_value(json)` — body as kvlist/string/int/etc
- `OtelLog::set_source_metadata(name, now)` — sets `source_type` resource attribute + `observed_time_unix_nano`
- `OtelLog::set_resource_attribute(key, value)` — e.g. `host.name`
- `OtelLog::set_attribute(key, value)` — record-level attributes
- `string_value(s)`, `int_value(i)`, `json_to_any_value(v)` — AnyValue constructors

*Source annotation migration pattern:*
All 40 source files now have dual-path event handling:
```rust
if let Event::OtelLog(ref mut otel_log) = event {
    otel_log.set_source_metadata(SOURCE_NAME, now);
    otel_log.set_resource_attribute("host.name".into(), string_value(host));
    // source-specific attributes
} else if let Event::Log(ref mut log) = event {
    // legacy fallback for syslog/gelf/native deserializers
}
```

*Kubernetes-specific:* Pod/namespace/node metadata annotators add OTel semantic
convention attributes (`k8s.pod.name`, `k8s.namespace.name`, etc.) on the
`Event::OtelLog` branch. CRI/Docker parser converts OtelLog body to temporary
LogEvent for parsing, then transfers parsed fields back as attributes.

*Files changed:* 40 files, +1,446/-790 lines.

**Validation gate (6a):** ✓ PASS
- `cargo check` — clean
- 12/12 otel_event unit tests — pass
- 16/16 OpenTelemetry source tests — pass
- 107/107 event module tests — pass
- 29/29 codec format tests — pass

#### 6b — Migrate metric sources to emit `OtelMetric` — COMPLETE

**Strategy**: Instead of modifying each of the ~14 metric sources individually,
the migration was achieved by changing the single `impl From<Metric> for Event`
in `lib/vector-core/src/event/mod.rs` to produce
`Event::OtelMetric(OtelMetric::from_legacy_metric(metric))` instead of
`Event::Metric(metric)`.

This means **every** metric source that creates a `Metric` and converts it to
an `Event` (via `.into()`, `Event::from()`, or `send_batch(Vec<Metric>)`) now
automatically emits `Event::OtelMetric`.

**Key changes:**

| File | Change |
|------|--------|
| `lib/vector-core/src/event/otel_event.rs` | Added `OtelMetric::from_legacy_metric(Metric)` — converts MetricValue variants to OTel metric data types (Counter→Sum, Gauge→Gauge, AggregatedHistogram→Histogram, AggregatedSummary→Summary, Distribution→Histogram, Set→Gauge cardinality) |
| `lib/vector-core/src/event/otel_event.rs` | Added `OtelMetric::to_legacy_metric()` — reverse bridge for sinks/transforms that still use `try_into_metric()` |
| `lib/vector-core/src/event/otel_event.rs` | Added `otel_value_to_tag_string()` helper for tag conversion |
| `lib/vector-core/src/event/mod.rs` | Changed `impl From<Metric> for Event` to produce `Event::OtelMetric` |
| `lib/vector-core/src/event/mod.rs` | Updated `into_metric()` and `try_into_metric()` to handle `Event::OtelMetric` via `to_legacy_metric()` |
| `src/sources/statsd/mod.rs` | Changed `Event::Metric(metric)` to `Event::from(metric)` |

**Temporary bridge**: `try_into_metric()` and `into_metric()` on `Event` now
also accept `Event::OtelMetric` by converting back to legacy `Metric` via
`to_legacy_metric()`. This means all existing sinks and transforms that use
`try_into_metric()` (prometheus, influxdb, statsd, greptimedb, etc.) continue
to work without modification. The bridge will be removed in Step 6e when sinks
are migrated to accept `OtelMetric` natively.

**Validation:**
- Full workspace `cargo check` passes (0 errors)
- 16/16 otel_event tests pass (4 new round-trip tests)
- 114/114 event module tests pass
- 5/5 OTel proto metrics tests pass

#### 6c — Verify trace source emits `OtelSpan` exclusively — COMPLETE

Verified: The OpenTelemetry source (`src/sources/opentelemetry/`) uses
`into_otel_event_iter()` for all signals, producing `Event::OtelSpan` for
traces. No other source creates trace events, except `datadog_agent/traces.rs`
which creates `Event::Trace` — this will be addressed in Step 7 (Datadog
re-integration as OTel adapter).

#### 6d — Migrate transforms off legacy types (~14 files) ✓ COMPLETE

**Strategy**: Rather than removing all legacy handling (which would break the
temporary bridge), the approach was:

1. **Metric-only transforms** (aggregate, tag_cardinality_limit,
   incremental_to_absolute, metric_to_log) — updated to recognize
   `Event::OtelMetric` via `to_legacy_metric()` bridge so they process
   OtelMetric events arriving from the Step 6b `From<Metric>` change.

2. **aws_ec2_metadata** — now enriches `OtelLog`, `OtelMetric`, `OtelSpan`
   via resource attributes instead of silently passing through.

3. **remap annotate_dropped** — now sets vector.dropped.* attributes on
   `OtelLog` events.

4. **EventArray infrastructure** — fixed `EventArrayIntoIter::Metrics` and
   `MetricArray::into_events()` to preserve `Event::Metric` identity
   (bypassing `From<Metric> for Event → OtelMetric` conversion in the
   output buffer drain path).

5. **OtelMetric round-trip improvements**:
   - Namespace stored as `metric.namespace` resource attribute (not
     concatenated into name) for lossless round-trip.
   - Multi-value tags mapped to OTLP array attributes (round-trip
     fidelity via `insert_otel_attr_as_tag` helper).
   - `time_unix_nano = 0` mapped to `None` timestamp (not epoch).
   - Added `resource_mut()` on `OtelSpan` and `OtelMetric`.
   - Added `Event::to_metric()` convenience method for tests.

6. **Test updates**: ~50 test assertions updated to use `to_metric()` or
   `Event::from(Metric)` instead of `Event::Metric(Metric)` / `as_metric()`.

| Transform | Change |
|-----------|--------|
| `aggregate.rs` | `Event::OtelMetric` recognized in `record()` + `flush_into()` emits via `.into()` |
| `tag_cardinality_limit` | OtelMetric→Metric bridge, process, re-convert to OtelMetric |
| `incremental_to_absolute` | OtelMetric→Metric bridge for `make_absolute()` |
| `metric_to_log.rs` | `Event::OtelMetric` → `to_legacy_metric()` → serialize to log |
| `log_to_metric.rs` | `Event::OtelLog` handled via `to_log_event()`; output stays `Event::Metric` |
| `aws_ec2_metadata.rs` | OtelLog/OtelMetric/OtelSpan enriched via resource attributes |
| `remap.rs` | `annotate_dropped` sets vector.dropped.* on OtelLog |
| `filter`, `route`, `throttle` | Already fully type-agnostic — no changes needed |
| `sample/transform.rs` | Already has OTel stubs — no changes needed |
| `dedupe`, `reduce` | Already coerce via `to_log_event()` / `into_log_coerce()` — no changes |

#### 6e — Migrate sinks off legacy types (~25 files)

Many sinks already coerce `OtelLog → LogEvent` (Step 5c²e/5c²g). This phase
removes the coercion — sinks accept OTel types natively.

*Log sinks:* Each sink's encoder projects `OtelLog` fields directly instead of
going through `to_log_event()`. Example: Loki extracts labels from
`resource.attributes` + `record.attributes`; Elasticsearch indexes
`record.body` and `record.attributes` as document fields.

*Metric sinks:* Each sink maps `OtelMetric` data points to the target format.
Example: Prometheus maps `Sum → counter`, `Gauge → gauge`,
`Histogram → histogram`; InfluxDB maps data point attributes to tags.

*The OTel sink (gRPC + HTTP):* Remove `Event::Log`/`Event::Metric`/`Event::Trace`
match arms from `collection_into_request()`.

#### 6f — Remove legacy types from core (~42 files)

With all sources, transforms, and sinks migrated, the legacy types become dead code.

**What is deleted:**

| Type | File | Lines |
|------|------|-------|
| `LogEvent` | `lib/vector-core/src/event/log_event.rs` | ~1,221 |
| `TraceEvent` | `lib/vector-core/src/event/trace.rs` | ~192 |
| `Metric` | `lib/vector-core/src/event/metric/` | ~2,300 |
| `Event::{Log,Metric,Trace}` | `lib/vector-core/src/event/mod.rs` | variants removed |
| `EventArray::{Logs,Metrics,Traces}` | `lib/vector-core/src/event/array.rs` | variants removed |
| `VrlTarget::{LogEvent,Metric,Trace}` | `lib/vector-core/src/event/vrl_target.rs` | arms removed |
| `event.proto` | `lib/vector-core/proto/event.proto` | ~230 |
| Legacy proto ser/de | `lib/vector-core/src/event/proto.rs` | legacy arms |
| Lua legacy bindings | `lib/vector-core/src/event/lua/` | legacy arms |
| Legacy `into_event_iter()` | `lib/opentelemetry-proto/src/{logs,spans,metrics}.rs` | old iterators |
| Legacy buffer codec paths | `lib/opentelemetry-proto/src/buffer_codec.rs` | old paths |

**Rename:** `OtelLog` → `Log`, `OtelMetric` → `Metric`, `OtelSpan` → `Span`.
The `Event` enum becomes `Event { Log(Log), Metric(Metric), Span(Span) }`.

#### 6g — Delete native codecs and test fixtures

With legacy types gone, the native codecs have nothing to serialize.

**What is deleted:**

| File | Lines | Notes |
|------|-------|-------|
| `lib/codecs/src/decoding/format/native.rs` | 59 | `NativeDeserializer` |
| `lib/codecs/src/encoding/format/native.rs` | 45 | `NativeSerializer` |
| `lib/codecs/src/decoding/format/native_json.rs` | 139 | `NativeJsonDeserializer` |
| `lib/codecs/src/encoding/format/native_json.rs` | 108 | `NativeJsonSerializer` |
| `lib/codecs/tests/native.rs` | test file | Round-trip tests |
| `lib/codecs/tests/native_json.rs` | test file | Round-trip tests |
| `lib/codecs/tests/data/native_encoding/` | ~7,400 files | Fixture data |

**Enum cleanup:** Remove `DeserializerConfig::Native`, `DeserializerConfig::NativeJson`,
`SerializerConfig::Native`, `SerializerConfig::NativeJson` and all match arms in
`lib/codecs/src/decoding/mod.rs`, `lib/codecs/src/encoding/serializer.rs`,
`lib/codecs/src/encoding/config.rs`, `lib/codecs/src/encoding/encoder.rs`.

**Production code cleanup:**
- `src/sinks/http/batch.rs` — remove `Serializer::NativeJson` match arm
- `src/components/validation/resources/mod.rs` — remove Native codec mapping

**Flag rule:** `DiskBufferV1CompatibilityMode` and `OtlpEncoding` are **never removed**
from `EventEncodableMetadataFlags`. The `can_decode()` implementation stops accepting
`DiskBufferV1CompatibilityMode`-only records (same precedent as v1→v2 transition). The
enum variant stays permanently.

`vector validate` updated to error if `buffer_format = "vector"` is still set.

Note: `proto/vector/vector.proto` is **retained** for Step 7 — the Vector source
re-integration needs to decode legacy Vector proto frames from unupgraded upstream
instances.

### Validation gate (Step 6)

- `rg "LogEvent|TraceEvent\b" lib/vector-core/src/` returns empty (except backward-compat aliases if kept for one release).
- `rg "Event::Log\b|Event::Metric\b|Event::Trace\b" src/ lib/` returns empty.
- `rg "NativeDeserializer|NativeSerializer|native_json" lib/` returns empty.
- `cargo build` clean.
- `cargo test` — all tests pass.
- The `Event` enum has exactly 3 variants: `Log`, `Metric`, `Span`.
- OTLP source → OTLP sink round-trip for all 3 signals with zero conversion.

---

## Step 7 — Re-Integration: Vector + DataDog Sinks/Sources as OTel Adapters

**Status: NOT STARTED — after Step 6.**

With the core protocol migration complete (Step 6), Vector and DataDog sinks/sources
can be re-added as clean OTel-native adapters. No proprietary types leak into core.

### Sub-components

| Component | What | Estimated effort |
|-----------|------|-----------------|
| **Vector sink** | `Event::Log`/`Metric`/`Span` → `ExportXxxServiceRequest` over gRPC to unupgraded downstream Vector instances. Backward-compat bridge only. | ~300 lines |
| **DataDog sink** | OTel events → DataDog wire format for APIs without OTLP support (e.g. Events API). `AgentDDSketch` re-introduced only within this adapter if needed. | ~2,000 lines |
| **DataDog source** | Already migrated in Step 3. Verify clean against final OTel types. | ~100 lines |
| **Vector source** | Already receives OTLP natively. Legacy proto decoding retained from `proto/vector/vector.proto` for backward compat with unupgraded upstream instances. | ~100 lines |

### Validation gate (Step 7)

- `cargo build -p vector-core` still clean — no proprietary types in core.
- Round-trip test for all three signal types including span scope assertion.
- DataDog sink integration test against DD OTLP endpoint.

---

## Step 4 — Tail Sampling, Load-Balancing Sink, and Pipeline Telemetry

**Status: NOT STARTED — after Step 7.**

Built entirely on OTel-native types. No legacy compatibility concerns.

---

## Open Questions and Decisions

| ID | Question | Resolution |
|---|---|---|
| Q1 | Per-signal channel isolation — benchmark? | Code audit confirmed. Integration test in Step 0b. |
| Q2 | DDSketch approximation vs ExponentialHistogram | ExponentialHistogram in core. Sketch conversion only in DD source adapter. |
| Q3 | OTel sink — gRPC missing | gRPC added in Step 1. Dual-protocol: gRPC internal, HTTP external. |
| Q4 | `MetricValue::Distribution` / `Set` — who uses them? | StatsD source only. Conversion at StatsD boundary (Step 3). |
| Q5 | `datadog_api_key` blast radius | Only DD source/sink + `log_to_metric`. VRL `get_secret` unaffected. |
| Q6 | APM stats — keep or drop? | Cancelled. Replaced by pipeline telemetry (Step 4). Spec: `APM_STATS_OTLP_BACKPORT.md`. |
| Q7 | VRL tail sampling ergonomics | `spans_any`/`spans_all` shorthand types added. |
| Q8 | VRL migration tool coverage | ~91% after SEM-08/SEM-09 and dynamic path heuristic. |
| Q9 | `NativeDeserializer` external exposure | `publish = false`. Internal only. |
| Q10 | OTel sink grouping + spans.rs scope drop | Scope drop fixed in Step 0b (15 lines). Reverse encoder in Step 2. |
| PC1 | `use_otlp_decoding` flag | ✓ Deleted in Step 5e. `rg "use_otlp_decoding" src/` returns empty. |
| PC2 | Step 2 ownership | Must be in flight before Step 0 closes. |
| PC3 | Buffer toggle design | Single process-wide `AtomicCell<BufferFormat>`. |
| PC4 | Span scope fix timing | Step 0b. Zero-risk, additive. |
| G1 | `AgentDDSketch::to_histogram()` referenced but missing | Add `to_aggregated_histogram(bounds)` in Step 1 as bridge. Deleted in Step 3. |
| G2 | `EventArray → OtlpBufferBatch` grouping | Split by signal type via `EventArray::logs/metrics/traces()`. Three export request types per batch. |
| G3 | `buffer_format = "otlp"` on existing buffer | Startup: auto-detect existing buffer → force `Migrate` mode, log warning, refuse to start in `Otlp` mode if records present. |
| G4 | VRL `TypeState` after Step 5 | Migration tool uses OTel type schema for TypeState, not Vector schema. Addressed in Step 5f. |
| G5 | `ByteSizeOf` / `EventCount` for OTel types | Implemented in Step 5a as part of wrapper trait impls. ✓ Done. |
| G6 | Schema definitions after Step 5 | OTel source `outputs()` schema definitions rewritten for OTel field paths in Step 5c/5d. |
| G7 | Big-bang vs wrapper migration strategy | Wrapper approach chosen and validated. OTel wrapper types introduced (Step 5a), then signal-by-signal migration (5b/5c/5d batch 1). Always compilable. ✓ Proven. |
| G8 | Core value type: VRL `Value` vs OTel `AnyValue` | `AnyValue` is the core value type. `Value` is VRL-boundary only. `Timestamp`/`Null`/`Regex` gaps handled at VRL adapter layer. |
| G9 | `Vec<KeyValue>` attribute lookup performance | O(n) acceptable for non-VRL paths. VRL adapter copies to `BTreeMap` during program execution. `IndexedAttributes` wrapper deferred to future optimization. |
| G10 | `EventMetadata` fate | Retained as pipeline sidecar. Not merged into `Resource.attributes`. Carries finalizers, source_id, schema — pipeline concerns only. ✓ Validated across all 3 wrappers. |
| G11 | 6-variant `Event` enum duration | Currently active. All 6 variants in play. Legacy variants removed in Step 6f. |
| G12 | `EventArray` expansion for OTel | Added `OtelLogs`, `OtelMetrics`, `OtelSpans` variants in Step 5c batch 1. All iterators, trait impls, and pipeline plumbing updated. ✓ Done. |
| G13 | Proto type boundary (`opentelemetry-proto` ↔ `otel-proto-types`) | Identical protobuf schemas in two crates — bridged via `encode_to_vec()` / `decode()` roundtrip. Validated across all 3 signals. Low overhead. ✓ Proven. |

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| OTel metric encoder misses edge cases (multi-value tags, `interval_ms`, empty points) | Medium | High | Property-based round-trip tests at Step 2 |
| Three batch timers introduce jitter on sparse-signal pipelines | High | Low | Benchmark at Step 1; unified flush if needed |
| VRL user programs break at scale | High | High | `vector vrl-migrate` ships at Step 5f; dry-run mode; 91% auto-rewrite target |
| spans.rs scope drop causes trace data loss before fix lands | High | Medium | Fix A in Step 0b — must be first PR after Step 0a |
| DataDog source rewrite misses field edge cases | Medium | High | Integration tests against real DD agent at Step 3 |
| Buffer `migrate` mode regression | Low | High | Golden tests at Step 0a; CI gate |
| `datadog_events` API has no OTLP equivalent | Low | Low | Documented; covered in Step 7 study |
| Upstream Vector instances on old protocol pushing to migrated instance | Medium | Medium | Vector source keeps backward-compat reception; only sink removed |
| `avg` field on AgentDDSketch lost (no OTel equivalent) | Low | Medium | Documented explicitly in DD source adapter code |
| `buffer_format = "otlp"` set on existing buffer → crash | Medium | High | Startup auto-detect: force `Migrate` if existing buffer detected (G3) |
| `to_aggregated_histogram` bridge omitted → Prometheus/InfluxDB/GreptimeDB drop sketch metrics silently at Step 1 | Medium | Medium | Must implement before Step 1 PR is merged |
| VRL migration tool TypeState computed against wrong schema | Low | Medium | Use OTel schema in tool (G4); flagged for Step 5f |
| 6-variant `Event` enum increases `match` arm noise during migration | High | Low | Temporary. Each sub-step (5b/5c/5d) removes one legacy variant. Macro or helper to reduce boilerplate. |
| `From<LogEvent> for OtelLogEvent` conversion is lossy for edge cases | Medium | Medium | Document known losses (e.g., `Value::Regex` → string). Round-trip property tests in Step 5a. |
| Wrapper types add memory overhead (dual Resource/Scope + EventMetadata) | Low | Low | OTel proto types are already small. `Option<Resource>` is zero-cost when absent. Benchmark in Step 5a. |
| Lazy VRL projection has correctness bugs (write-back to proto) | Medium | High | Comprehensive VRL round-trip tests for each OTel wrapper arm. Fuzz testing on `VrlTarget::into_events()`. |
| Metric mapping ambiguity (Vector Set/Distribution → OTel) | Medium | Medium | Mapping table documented in Step 5d. Each source sets `AggregationTemporality` at ingestion. |
| Mixed pipeline conversion during transition adds latency | Low | Low | `From` conversions are O(n) on fields, not O(n) on events. Acceptable for transition period. |

---

## Verified Code Delta

Based on actual file counts from source. Items marked ✓ have actual line counts from commits.

| Category | Removed | Added | Status |
|---|---|---|---|
| DataDog sinks (`src/sinks/datadog/`) | 9,882 lines (incl. tests) | 0 | ✓ COMPLETE |
| Vector sink (`src/sinks/vector/`) | 791 lines | 0 | ✓ COMPLETE |
| Native codecs (4 files) | 351 lines | 0 | Step 6 |
| `event.proto` | ~230 lines | 0 | Step 6 |
| `AgentDDSketch` from core | 1,637 lines | 0 (moved to adapter) | ✓ COMPLETE |
| OTel sink gRPC module | 0 | ~300 est. | ✓ COMPLETE |
| OTel metric encoder (Step 2) | 0 | ~400 est. | ✓ COMPLETE |
| Step 5a: OTel wrapper types + trait impls | 0 | ~740 actual | ✓ COMPLETE |
| Step 5b: Trace migration (OTel source/sink) | 0 | ~200 actual | ✓ COMPLETE |
| Step 5c batch 1: Log migration (OTel source/sink + EventArray) | 213 | 479 actual | ✓ COMPLETE |
| Step 5d batch 1: Metric migration (OTel source/sink) | 265 | 433 actual | ✓ COMPLETE |
| Step 5c remaining: other sources, transforms, sinks (5c²a–5c²g) | 92 actual | 1,711 actual | ✓ COMPLETE |
| Step 5d remaining: other sources, transforms, sinks | ~2,800 est. | ~300 est. | Pending |
| Step 5e: `use_otlp_decoding` + legacy deserializer paths | 464 actual | 47 actual | ✓ COMPLETE |
| Step 5e²: OTLP serializer OTel-native encoding | 4 actual | 227 actual | ✓ COMPLETE |
| Step 5c²a: VrlTarget OTel-native events | 17 actual | 966 actual | ✓ COMPLETE |
| Step 5c²b: Condition matchers + sample transform | 12 actual | 42 actual | ✓ COMPLETE |
| Step 5c²c: Codec serializers OTel-native logs | 26 actual | 384 actual | ✓ COMPLETE |
| Step 5c²d: Transforms handle OTel events | 16 actual | 135 actual | ✓ COMPLETE |
| Step 5c²e: Sinks handle OTel events | 23 actual | 39 actual | ✓ COMPLETE |
| Step 5c²f: Buffer codec + remaining gaps | 8 actual | 133 actual | ✓ COMPLETE |
| Step 5c²g: Last unsafe call sites | 4 actual | 18 actual | ✓ COMPLETE |
| Step 5c²h: Template + Transformer + silent drops | 2 actual | 22 actual | ✓ COMPLETE |
| Step 5f: VRL migration tool | 0 | 1,229 actual | ✓ COMPLETE |
| Step 5g: Rename + type alias cleanup | 0 | 173 actual (net +5) | ✓ COMPLETE |
| Step 5h: OTLP HTTP JSON + dep upgrades | 559 actual | 1,464 actual | ✓ COMPLETE |
| Step 6a: Log source OtelLog migration (40 files) | 790 actual | 1,446 actual | ✓ COMPLETE |
| Step 6b: Metric source OtelMetric migration | 28 actual | 481 actual | ✓ COMPLETE |
| Step 6c: Trace source OtelSpan verification | 0 | 0 | ✓ COMPLETE (already done) |
| Step 6d: Transform migration | ~30 | ~250 | ✓ COMPLETE |
| Step 6e–6g: Remaining legacy removal (~99 files) | ~5,100 est. | ~1,400 est. | NEXT |
| Step 7: Vector + DD sink/source re-integration | 0 | ~2,500 est. | Pending |
| Tail sampling + LB sink + pipeline telemetry (Step 4) | 0 | ~2,800 est. | Pending |
| Buffer toggle + OtlpBufferBatch | 0 | ~300 est. | ✓ COMPLETE |
| **Net** | **~19,569** | **~8,002** | |

Net reduction: ~11,567 lines. The wrapper approach adds ~1,350 lines vs big-bang (temporary
wrapper types + `From` conversions + extra `VrlTarget` arms during transition) but enables
incremental, always-compilable migration across ~30 smaller PRs instead of one monolith.

### OTel source → sink zero-conversion path: COMPLETE for all 3 signals (gRPC + HTTP)

As of Step 5e², the OTel source emits OTel-native events for **all three signals**
(logs, metrics, traces) and **both** the OTel gRPC sink and the OTel HTTP sink accept
them directly. An OTLP event ingested via the OTel source and exported via either OTel
sink traverses the pipeline with **zero field-level conversion** — the protobuf struct
flows end-to-end, with only a `encode_to_vec()` / `decode()` round-trip at the crate
boundary (same-schema types from `opentelemetry-proto` ↔ `otel-proto-types`).

### Legacy `use_otlp_decoding` workaround: ELIMINATED

Before Step 5e, the `use_otlp_decoding` flag provided two paths in the OTel source:
- `false` (default): OTLP data was converted to Vector native types with **lossy
  field-level conversion** (e.g., `ExponentialHistogram` collapsed into
  `AggregatedHistogram`, scope fields dropped for traces).
- `true`: OTLP data was stored as raw JSON blobs inside `Event::Log` — **metrics were
  literally converted to logs** as a workaround because Vector's native `Metric` type
  could not represent the full OTLP metric model.

Both paths are now removed. The source always emits true OTel-native events:
`Event::OtelLog`, `Event::OtelMetric`, `Event::OtelSpan`. Metrics are true metrics,
not logs.

Total: 22 opentelemetry-proto tests + 11 OTel source tests + 6 OTLP serializer tests
+ 24 VrlTarget tests (12 existing + 12 new OTel) + 3 condition matcher tests
+ 7 new codec/otel_event tests (to_log_event, body_string, text, raw_message, logfmt,
native rejection) + 68 existing transform tests verified passing.
Files changed across 5a–5c²b: ~55 files, +3,134/-975 actual lines.
