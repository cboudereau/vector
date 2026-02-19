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
  pub use_otlp_decoding: bool  ← flag: false = OTel→Vector types (lossy), true = raw blobs

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

This means the current OTel sink **only works when `use_otlp_decoding = true`** on the source
(raw OTLP blobs stored as logs). It cannot handle native Vector `Metric` events at all.

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

This conversion happens today every time the OTel source decodes an `ExponentialHistogram`
with `use_otlp_decoding = false` (the default). It is one of the primary things the migration
must eliminate.

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
  src/sources/opentelemetry/config.rs ← definition
  src/sources/opentelemetry/mod.rs ← routing
  src/sources/opentelemetry/grpc.rs ← conditional branch
  src/sources/opentelemetry/http.rs ← conditional branch
  → frozen at Step 0; all branches deleted in Step 5d
```

---

## Execution Order

```
Step 0   Foundations (buffer toggle + isolation test + span scope fix)
Step 2   OTel metric encoder — prerequisite for Step 1
Step 1   Both sinks removed; OTel sink gRPC added; sketch arms cleaned from non-DD sinks
Step 3   DataDog source rewritten as clean OTel adapter; DD types leave core
Step 4   APM stats + tail sampling as OTel-native transforms
Step 5   Core event model → OTel types; VRL migration tool ships; use_otlp_decoding removed
Step 6   Native codecs and Vector proto removal
Step 7   Optional: Vector and DataDog sink re-integration as OTel-native adapters
```

**Why Step 2 before Step 1:** The OTLP serializer currently errors on `Event::Metric`. Step 2
fixes this. Without it, the OTel sink cannot replace the DataDog metric sink and the migration
is blocked. Step 2 must land before Step 1 can be validated.

---

## Step 0 — Foundations

### 0a — Buffer format toggle

**Status: PARTIAL — metadata layer done, I/O layer not started**

File: `lib/vector-core/src/event/ser.rs`

**Done (committed):**
- `BufferFormat` enum (`Vector` / `Otlp` / `Migrate`) with `#[default] Vector`
- `BUFFER_FORMAT: AtomicCell<BufferFormat>` process-wide static
- `OtlpEncoding = 0b10` flag in `EventEncodableMetadataFlags`
- `get_metadata()` branches on `BUFFER_FORMAT` — stamps correct flags on new records
- `can_decode()` branches on `BUFFER_FORMAT` — accepts/rejects records by flag
- 6 unit tests covering all three modes for both `get_metadata` and `can_decode`

**Not done — `encode()` and `decode()` still ignore `BUFFER_FORMAT`:**

Current `encode()` always uses `proto::EventArray` regardless of the toggle:
```rust
fn encode<B>(self, buffer: &mut B) -> Result<(), Self::EncodeError> {
    proto::EventArray::from(self)   // ← always Vector proto
        .encode(buffer)
        .map_err(|_| EncodeError::BufferTooSmall)
}
```

Current `decode()` never checks for `OtlpEncoding`, always decodes as `proto::EventArray`
or falls back to `proto::EventWrapper`.

The toggle currently has **no effect on actual I/O** — only the metadata/flag stamping is
wired. The data path is the remaining work.

**Remaining tasks:**

1. `lib/vector-core/proto/otlp_buffer.proto` — define `OtlpBufferBatch`:
   ```protobuf
   message OtlpBufferBatch {
     opentelemetry.proto.collector.logs.v1.ExportLogsServiceRequest logs = 1;
     opentelemetry.proto.collector.metrics.v1.ExportMetricsServiceRequest metrics = 2;
     opentelemetry.proto.collector.trace.v1.ExportTraceServiceRequest traces = 3;
   }
   ```
   Add to `lib/vector-core/build.rs` proto compilation list.

2. `EventArray → OtlpBufferBatch` conversion: split by signal type using
   `EventArray::logs()` / `EventArray::metrics()` / `EventArray::traces()`, convert each
   using existing `lib/opentelemetry-proto/src/` types in reverse.

3. `OtlpBufferBatch → EventArray` conversion: use existing
   `ResourceLogs::into_event_iter()` / `ResourceMetrics::into_event_iter()` /
   `ResourceSpans::into_event_iter()`.

4. Update `encode()` to branch on `BUFFER_FORMAT`:
   - `Vector` → existing `proto::EventArray::from(self).encode(buffer)`
   - `Otlp` | `Migrate` → `OtlpBufferBatch::from(self).encode(buffer)`

5. Update `decode()` to branch on `OtlpEncoding` flag:
   - flag set → `OtlpBufferBatch::decode(buffer)` → `EventArray::from(batch)`
   - flag not set → existing `proto::EventArray` / `proto::EventWrapper` path

6. `buffer_format` field in `lib/vector-core/src/config/global_options.rs`
   (default `"vector"`), with startup wiring: `BUFFER_FORMAT.store(config.buffer_format)`.

7. Startup validation: if `buffer_format = "otlp"` and an existing on-disk buffer is
   detected, force `Migrate` mode and log a warning rather than crashing on unreadable
   records.

8. Integration test: write Vector-proto records, restart in `migrate` mode, assert old
   records decode and new records carry `OtlpEncoding` flag.

Full spec: `DISK_BUFFER_MIGRATION.md`.

### 0b — Per-signal isolation test + span scope fix

**Status: NOT STARTED**


**Isolation test:** `SourceSender::named_outputs` provides independent backpressure per signal
type (confirmed by code audit). Add an integration test: fill metrics channel to capacity,
assert logs and traces continue flowing without `Status::unavailable`.

**Fix A — span scope drop** (`lib/opentelemetry-proto/src/spans.rs`):

Current code (line 32–34):
```rust
self.scope_spans
    .into_iter()
    .flat_map(|ils| ils.spans)  // ← InstrumentationScope dropped here
    .map(move |span| { ResourceSpan { resource: resource.clone(), span }.into_event(now) })
```

Fix: pass `scope` through alongside `span`, store `.scope.name`, `.scope.version`,
`.scope.attributes` on the `TraceEvent`. ~15 additive lines. Mirrors the existing `logs.rs`
pattern where scope is preserved. Must land in Step 0b so all trace events from this point
carry scope.

### Validation gate (Step 0)

- All existing tests pass unchanged.
- Buffer toggle integration test passes.
- Backpressure isolation test passes.
- `rg "scope_spans.*into_iter.*flat_map.*spans" lib/opentelemetry-proto/src/spans.rs` returns
  no scope-dropping pattern.

---

## Step 2 — OTel Metric Encoder (Prerequisite for Step 1)

### Goal

Fix `Event::Metric(_) => Err("not supported")` in
`lib/codecs/src/encoding/format/otlp.rs:127`. This is the blocker for everything downstream.

### What changes

The encoder receives `Event::Metric(m)` and produces `ExportMetricsServiceRequest` protobuf.
The existing `lib/opentelemetry-proto/src/metrics.rs` already converts **from** OTel proto
**to** Vector `Metric` events (442 lines). Step 2 implements the reverse.

| `MetricValue` variant | OTel encoding | Temporality |
|---|---|---|
| `Counter { value }` | `Sum { data_points: [NumberDataPoint] }` | `kind == Absolute` → Cumulative; `Incremental` → Delta |
| `Gauge { value }` | `Gauge { data_points: [NumberDataPoint] }` | N/A |
| `AggregatedHistogram { buckets, count, sum }` | `Histogram { data_points: [HistogramDataPoint] }` | from `kind` |
| `AggregatedSummary { quantiles, count, sum }` | `Summary { data_points: [SummaryDataPoint] }` | N/A |
| `Set` | **Never enters core** — converted at StatsD source boundary | — |
| `Distribution` | **Never enters core** — converted at StatsD source boundary | — |
| `Sketch` / `AgentDDSketch` | **Never enters core** — converted at DD source boundary | — |

`interval_ms` → `start_time_unix_nano = time_unix_nano - (interval_ms * 1_000_000)`. Lossy
(documented in the encoder).

Resource grouping: events from the same `source_id` are grouped into one `ResourceMetrics`.
Pipeline metadata (`source_id`, `source_type`, `upstream_id`) → `Resource.attributes["pipeline.*"]`.

`OtlpSerializerConfig::input_type()` currently returns `DataType::Log | DataType::Trace`.
After Step 2: `DataType::Log | DataType::Metric | DataType::Trace`.

### Validation gate (Step 2)

- `cargo build -p vector-core` and `cargo build -p codecs` clean.
- Round-trip encode/decode tests for each `MetricValue` variant.
- OTel sink end-to-end test: log + metric + trace through the HTTP OTel sink.

---

## Step 1 — Both Sinks Removed; OTel Sink gRPC Added

### What is removed

**DataDog sinks** — `src/sinks/datadog/` (9,882 lines total including tests):

| Subsink | Key files | Lines (non-test) |
|---|---|---|
| metrics | `encoder.rs` (1,792), `sink.rs` (473), `normalizer.rs` (327), `config.rs` (305), `request_builder.rs` (301), `service.rs` (186) | ~3,384 |
| traces + apm_stats | `request_builder.rs` (537), `config.rs` (254), `sink.rs` (159), `apm_stats/aggregation.rs` (434), `apm_stats/bucket.rs` (191), `apm_stats/flusher.rs` (172), `apm_stats/mod.rs` (123), `apm_stats/weight.rs` (95) | ~1,965 |
| logs | `sink.rs` (741), `config.rs` (272), `service.rs` (187) | ~1,200 |
| events | `request_builder.rs` (149), `service.rs` (116), `config.rs` (114), `sink.rs` (94) | ~473 |
| shared | `mod.rs` (314) | 314 |

**Vector sink** — `src/sinks/vector/` (791 lines total):
`config.rs` (247), `mod.rs` (259), `service.rs` (164), `sink.rs` (121)

### Prerequisite: `AgentDDSketch::to_aggregated_histogram`

**This method does not exist yet and must be added in this step.**

`SINK_REMOVAL_STRATEGY.md` §5 refers to "the existing `AgentDDSketch::to_histogram()` method"
— that method does not exist. The only conversion methods on `AgentDDSketch` go the wrong
direction (histogram → sketch via `transform_to_sketch` / `insert_interpolate_buckets`).

Before deleting the match arms below, add to `lib/vector-core/src/metrics/ddsketch.rs`:

```rust
/// Approximate conversion to `AggregatedHistogram` using explicit bucket bounds.
/// Used as a bridge in non-DD sinks between Step 1 (sink removal) and Step 3
/// (AgentDDSketch leaves core). Precision is bounded by the provided bounds.
pub fn to_aggregated_histogram(&self, bounds: &[f64]) -> MetricValue {
    // for each consecutive pair of bounds, query quantile at the midpoint
    // and accumulate counts. count and sum are exact.
}
```

This bridge conversion is intentionally approximate and documented as such. It is deleted
along with `AgentDDSketch` itself in Step 3.

### Residual sketch coupling cleaned in this step

After adding the bridge method and deleting the DD sinks, update the match arms in non-DD
sinks to use `sketch.to_aggregated_histogram(DEFAULT_BOUNDS)` instead of the DD-specific
quantile calls, then the match arms collapse into the `AggregatedHistogram` path:

| File | Current | Change |
|---|---|---|
| `src/sinks/prometheus/collector.rs:184` | `Sketch → ddsketch.quantile(q)` | `Sketch → sketch.to_aggregated_histogram(PROM_BOUNDS)`, then handled by `AggregatedHistogram` arm |
| `src/sinks/influxdb/metrics.rs:366` | `Sketch → ddsketch.avg()/.min()/etc.` | `Sketch → sketch.to_aggregated_histogram(DEFAULT_BOUNDS)` |
| `src/sinks/greptimedb/metrics/batch.rs:40` | `Sketch { .. } => size_estimate` | `Sketch { .. } => AggregatedHistogram size estimate` |
| `src/sinks/util/buffer/metrics/split.rs:122` | `Sketch { .. } => routing` | `Sketch { .. } => same routing as AggregatedHistogram` |

**These conversions are explicitly approximate and documented. They are bridge code only,
deleted in Step 3 when `AgentDDSketch` leaves core.**

### What is added

gRPC module in `src/sinks/opentelemetry/`:
- New `Protocol::Grpc(GrpcSinkConfig)` variant alongside existing `Protocol::Http`
- gRPC for internal Vector→Vector forwarding
- HTTP remains for external OTLP endpoints
- `OtlpSerializerConfig::input_type()` must already include `DataType::Metric` (Step 2 prerequisite)

### Transforms affected

`src/transforms/log_to_metric.rs`: still sets `DatadogMetricOriginMetadata` on converted
metrics. The DD sinks that consumed it are now gone. The field becomes dead in the pipeline
but stays in `EventMetadata` until Step 3 removes it from core.

### Validation gate (Step 1)

- `cargo build` clean.
- `rg "src/sinks/vector\b" src/` returns empty.
- `rg "src/sinks/datadog\b" src/` returns empty.
- `rg "MetricValue::Sketch" src/sinks/ --include="*.rs"` returns empty.
- OTel gRPC sink integration test: Vector→Vector forwarding, all three signal types.
- Throughput benchmark: OTel gRPC within 10% of former Vector gRPC sink.

---

## Step 3 — DataDog Source as Clean OTel Adapter; DD Types Leave Core

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

### Validation gate (Step 3)

- `cargo build -p vector-core` clean.
- `rg "AgentDDSketch|DatadogMetric|datadog_api_key|MetricSketch|DATADOG_API_KEY" lib/vector-core/src/` returns empty.
- DataDog agent integration test: log + counter metric + sketch metric + trace, end-to-end.
- `ExponentialHistogram` round-trip: DD sketch payload → source adapter → OTel sink → assert
  bucket structure and `count`/`sum` preserved.

---

## Step 4 — APM Stats and Tail Sampling as OTel-Native Transforms

### `apm_stats` transform

Full specification: `APM_STATS_OTLP_BACKPORT.md` (canonical). Summary below.

Ports the algorithm from `src/sinks/datadog/traces/apm_stats/` (removed in Step 1) as a
standalone transform in `src/transforms/apm_stats/`. Consumes OTel `Span` events, emits
OTel `Metric` signals per 10-second window:

| Metric | OTel type | Dimensions |
|---|---|---|
| `spans.hits` | Sum (delta, int) | `span.name`, `span.resource`, `span.type`, `http.status_code` |
| `spans.top_level_hits` | Sum (delta, int) | `span.name`, `span.resource`, `span.type` |
| `spans.errors` | Sum (delta, int) | `span.name`, `span.resource`, `span.type`, `http.status_code` |
| `spans.duration` | Sum (delta) ns | `span.name`, `span.resource`, `span.type` |
| `spans.duration.ok` | ExponentialHistogram (delta, scale 7) ns | `span.name`, `span.resource`, `span.type` |
| `spans.duration.error` | ExponentialHistogram (delta, scale 7) ns | `span.name`, `span.resource`, `span.type` |

`AgentDDSketch` accumulator replaced by a ~100-line `ExponentialHistogramAccumulator` at
scale 7. No DD proto, no MessagePack. Two outputs: `apm_stats.spans` (pass-through) and
`apm_stats.stats` (metrics).

### `tail_sample` transform

Full specification: `TAIL_SAMPLING_BACKPORT.md`.

Buffers OTel spans by `trace_id`, applies a VRL policy to the complete trace (`.spans` array),
emits or drops all spans. Shorthand types: `type = "spans_any"` and `type = "spans_all"`.

### Internal pipeline telemetry

All existing internal metrics (`vector_buffer_events`, `vector_component_sent_events_total`,
etc.) are preserved. The `metrics` crate emits them as OTel `Metric` signals.

### Validation gate (Step 4)

- `apm_stats` unit tests: known span inputs → expected OTel metric output.
- `tail_sample` tests: buffer, policy evaluation, emit/drop correctness.
- No DD types referenced in either transform.

---

## Step 5 — Core Event Model → OTel Types; VRL Migration Tool Ships

### Goal

Replace `Event::{Log(LogEvent), Metric(Metric), Trace(TraceEvent)}` with OTel native types
throughout `vector-core`. This is the largest structural change.

### What changes

| Current | OTel replacement |
|---|---|
| `LogEvent` (Value-based flat map, 1,221 lines) | OTel `LogRecord` (body + attributes + resource + scope + typed timestamp fields) |
| `Metric` + `MetricValue` (~2,300 lines) | OTel `Metric` (Sum/Gauge/Histogram/ExpHistogram/Summary) |
| `TraceEvent` (`LogEvent` newtype, 192 lines) | OTel `Span` (typed: trace_id, span_id, kind, status, events, links) |
| `EventMetadata` pipeline fields | `Resource.attributes["pipeline.*"]` on each event |
| `VrlTarget` (`vrl_target.rs`, 1,414 lines) | Rewritten for OTel field paths and `AnyValue` |
| `event/proto.rs` (769 lines) | Removed; superseded by OTel proto |
| `LogNamespace::Legacy` | Deprecated; `Vector` namespace becomes the only mode |
| `Value::Timestamp` in body/attributes | Serialized as `fixed64` nanoseconds; VRL requires explicit `parse_timestamp` |
| `Value::Null` in body/attributes | Absent field; VRL `exists()` pattern replaces `== null` |

**5d — `use_otlp_decoding` flag deleted:**
All four files (`config.rs`, `mod.rs`, `grpc.rs`, `http.rs`) in
`src/sources/opentelemetry/` have their `use_otlp_decoding` conditional branches removed.
The `false` path (OTel proto → Vector event types) no longer exists. The source always emits
OTel-native events.

**VRL migration tool ships:** `vector vrl-migrate <file>` rewrites ~91% of user VRL programs.
Remaining ~9% flagged with `# REVIEW:`. Full spec: `VRL_MIGRATION_TOOL.md`.

### Validation gate (Step 5)

- Full test suite passes.
- `rg "LogEvent|TraceEvent|MetricValue|use_otlp_decoding" lib/vector-core/src/` returns empty
  (except one-release type alias shims if needed).
- VRL migration tool achieves ≥91% auto-rewrite on project's own VRL test corpus.

---

## Step 6 — Native Codecs and Vector Proto Removal

### What is deleted

| File | Lines | Notes |
|---|---|---|
| `lib/codecs/src/decoding/format/native.rs` | 59 | `NativeDeserializer` |
| `lib/codecs/src/encoding/format/native.rs` | 45 | `NativeSerializer` |
| `lib/codecs/src/decoding/format/native_json.rs` | 139 | `NativeJsonDeserializer` |
| `lib/codecs/src/encoding/format/native_json.rs` | 108 | `NativeJsonSerializer` |
| `lib/vector-core/proto/event.proto` | ~230 | Vector-native wire format |

**Flag rule:** `DiskBufferV1CompatibilityMode` and `OtlpEncoding` are **never removed** from
`EventEncodableMetadataFlags`. The `can_decode()` implementation stops accepting
`DiskBufferV1CompatibilityMode`-only records (same precedent as v1→v2 transition). The enum
variant stays permanently.

`vector validate` updated to error if `buffer_format = "vector"` is still set.

Note: `proto/vector/vector.proto` is **retained** — the Vector source still decodes legacy
Vector proto frames from unupgraded upstream instances.

### Validation gate (Step 6)

- `rg "NativeDeserializer|NativeSerializer|native_json" lib/` returns empty.
- `cargo build` clean.

---

## Step 7 — Optional: Vector and DataDog Sink Re-Integration

If re-added, both sinks are clean OTel-native adapters. No proprietary types leak into core.

**Vector sink (new):** OTel events → `ExportLogsServiceRequest`/etc. over gRPC to unupgraded
downstream Vector instances. Backward-compat bridge only.

**DataDog sink (new):** OTel events → DataDog wire format for APIs without OTLP support
(e.g. Events API). `AgentDDSketch` re-introduced only within this adapter if needed.

**Validation gate:** `cargo build -p vector-core` still clean. Round-trip test for all three
signal types including span scope assertion (validates Fix A from Step 0b).

---

## Open Questions and Decisions (All Resolved)

| ID | Question | Resolution |
|---|---|---|
| Q1 | Per-signal channel isolation — benchmark? | Code audit confirmed. Integration test in Step 0b. |
| Q2 | DDSketch approximation vs ExponentialHistogram | ExponentialHistogram in core. Sketch conversion only in DD source adapter. |
| Q3 | OTel sink — gRPC missing | gRPC added in Step 1. Dual-protocol: gRPC internal, HTTP external. |
| Q4 | `MetricValue::Distribution` / `Set` — who uses them? | StatsD source only. Conversion at StatsD boundary (Step 3). |
| Q5 | `datadog_api_key` blast radius | Only DD source/sink + `log_to_metric`. VRL `get_secret` unaffected. |
| Q6 | APM stats — keep or drop? | Kept. Ported as `apm_stats` OTel transform in Step 4. Canonical spec: `APM_STATS_OTLP_BACKPORT.md`. |
| Q7 | VRL tail sampling ergonomics | `spans_any`/`spans_all` shorthand types added. |
| Q8 | VRL migration tool coverage | ~91% after SEM-08/SEM-09 and dynamic path heuristic. |
| Q9 | `NativeDeserializer` external exposure | `publish = false`. Internal only. |
| Q10 | OTel sink grouping + spans.rs scope drop | Scope drop fixed in Step 0b (15 lines). Reverse encoder in Step 2. |
| PC1 | `use_otlp_decoding` flag | Frozen at Step 0; deleted in Step 5d. |
| PC2 | Step 2 ownership | Must be in flight before Step 0 closes. |
| PC3 | Buffer toggle design | Single process-wide `AtomicCell<BufferFormat>`. |
| PC4 | Span scope fix timing | Step 0b. Zero-risk, additive. |
| G1 | `AgentDDSketch::to_histogram()` referenced but missing | Add `to_aggregated_histogram(bounds)` in Step 1 as bridge. Deleted in Step 3. |
| G2 | `EventArray → OtlpBufferBatch` grouping | Split by signal type via `EventArray::logs/metrics/traces()`. Three export request types per batch. |
| G3 | `buffer_format = "otlp"` on existing buffer | Startup: auto-detect existing buffer → force `Migrate` mode, log warning, refuse to start in `Otlp` mode if records present. |
| G4 | VRL `TypeState` after Step 5 | Migration tool uses OTel type schema for TypeState, not Vector schema. Addressed in Step 5 VRL rewrite. |
| G5 | `ByteSizeOf` / `EventCount` for OTel types | Must be implemented for new OTel `Span`, `LogRecord`, `Metric` types in Step 5. |
| G6 | Schema definitions after Step 5 | OTel source `outputs()` schema definitions must be rewritten for OTel field paths in Step 5. |

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| OTel metric encoder misses edge cases (multi-value tags, `interval_ms`, empty points) | Medium | High | Property-based round-trip tests at Step 2 |
| Three batch timers introduce jitter on sparse-signal pipelines | High | Low | Benchmark at Step 1; unified flush if needed |
| VRL user programs break at scale | High | High | `vector vrl-migrate` ships before Step 5; dry-run mode |
| spans.rs scope drop causes trace data loss before fix lands | High | Medium | Fix A in Step 0b — must be first PR after Step 0a |
| DataDog source rewrite misses field edge cases | Medium | High | Integration tests against real DD agent at Step 3 |
| Buffer `migrate` mode regression | Low | High | Golden tests at Step 0a; CI gate |
| `datadog_events` API has no OTLP equivalent | Low | Low | Documented; covered in Step 7 study |
| Upstream Vector instances on old protocol pushing to migrated instance | Medium | Medium | Vector source keeps backward-compat reception; only sink removed |
| `avg` field on AgentDDSketch lost (no OTel equivalent) | Low | Medium | Documented explicitly in DD source adapter code |
| `buffer_format = "otlp"` set on existing buffer → crash | Medium | High | Startup auto-detect: force `Migrate` if existing buffer detected (G3) |
| `to_aggregated_histogram` bridge omitted → Prometheus/InfluxDB/GreptimeDB drop sketch metrics silently at Step 1 | Medium | Medium | Must implement before Step 1 PR is merged |
| VRL migration tool TypeState computed against wrong schema | Low | Medium | Use OTel schema in tool (G4); flagged for Step 5 |

---

## Verified Code Delta

Based on actual file counts from source:

| Category | Removed | Added |
|---|---|---|
| DataDog sinks (`src/sinks/datadog/`) | 9,882 lines (incl. tests) | 0 |
| Vector sink (`src/sinks/vector/`) | 791 lines | 0 |
| Native codecs (4 files) | 351 lines | 0 |
| `event.proto` | ~230 lines | 0 |
| `AgentDDSketch` from core | 1,637 lines | 0 (moved to adapter) |
| OTel sink gRPC module | 0 | ~300 est. |
| OTel metric encoder (Step 2) | 0 | ~400 est. |
| Core event model rewrite (Step 5) | ~6,000 est. | ~3,000 est. |
| Source adaptations DD + Vector | ~500 est. | ~800 est. |
| `apm_stats` + `tail_sample` transforms | 0 | ~1,200 est. |
| VRL migration tool | 0 | ~800 est. |
| Buffer toggle + OtlpBufferBatch | 0 | ~300 est. |
| **Net** | **~19,391** | **~6,800** |

Net reduction: ~12,591 lines. The migration is a major simplification.
