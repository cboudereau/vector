# Complexity Study: Migrating Vector's Core Protocol to OpenTelemetry

## 1. Context and Current Architecture

Vector today has three conceptual layers for its internal data model.

### Design Principle (Scope of This Study)

> **Open for input, closed for output.**
> Vector and DataDog protocols are kept as **source (input) adapters only** — mirroring how the
> OTel source already works. Their sinks (output paths) are removed. The only output wire format
> is OTLP.

This mirrors the OTel model: there is a `src/sources/opentelemetry/` but the
`src/sinks/opentelemetry/` is a thin forwarder — not a first-class proprietary protocol.

### Current Architecture Overview

```
Sources                     Core Event Model (vector-core)              Sinks
─────────────────           ──────────────────────────────────          ──────────────────
opentelemetry     ──────►   Event::Log(LogEvent)                 ────►  opentelemetry
datadog_agent     ──────►   Event::Metric(Metric+MetricValue)    ────►  datadog metrics/logs/traces
vector (gRPC)     ──────►   Event::Trace(TraceEvent)             ────►  vector (gRPC)
other sources               EventMetadata                               prometheus, influxdb, …
                              ├─ DatadogMetricOriginMetadata
                              └─ datadog_api_key (secret)
                            ▼
                            event.proto (native vector wire protocol)
```

### Target Architecture Overview

```
Sources                     Core (OTel canonical)                       Sinks
─────────────────           ──────────────────────────────────          ──────────────────
opentelemetry     ──────►   OTel LogRecord                       ────►  opentelemetry (gRPC+HTTP)
datadog_agent  ─┐  adapt►  OTel Metric signal                   ────►  prometheus
vector (gRPC)  ─┘           OTel Span                            ────►  influxdb, loki, …
other sources               (no proprietary fields in core)             (all via OTel adapters)

REMOVED sinks: vector (gRPC), datadog metrics/logs/traces/events
```

### The Two Proprietary Protocols

**1. Vector protocol** (`proto/vector/vector.proto` + `lib/vector-core/proto/event.proto`)

The native gRPC inter-agent wire format. It encodes `Log`, `Metric`, and `Trace` variants,
and the proto schema itself carries DataDog-specific constructs:

```protobuf
// lib/vector-core/proto/event.proto (excerpt)
message DatadogOriginMetadata {
  optional uint32 origin_product = 1;
  optional uint32 origin_category = 2;
  optional uint32 origin_service = 3;
}

message Metadata {
  DatadogOriginMetadata datadog_origin_metadata = 2;
  ...
}

message Sketch {
  message AgentDDSketch { ... }   // DataDog-specific sketch type
  oneof sketch {
    AgentDDSketch agent_dd_sketch = 1;
  }
}
```

**2. DataDog protocol** (`proto/vector/dd_metric.proto`, `dd_trace.proto`, `ddsketch_full.proto`)

Consumed by the `datadog_agent` source and emitted by the DataDog sinks. `dd_trace.proto` has
its own `Span` structure distinct from OTel spans.

### Existing OTel Support (I/O Adapters Only)

The current OTel integration is entirely at the I/O boundary:

| Component | Role |
|---|---|
| `src/sources/opentelemetry/` | gRPC + HTTP source; decodes OTLP into Vector's internal model, or (with `use_otlp_decoding`) preserves raw OTLP JSON blobs |
| `src/sinks/opentelemetry/` | HTTP-only sink; only useful when `use_otlp_decoding` was set on the source |
| `lib/codecs/src/decoding/format/otlp.rs` | Frame-level OTLP deserializer |
| `lib/codecs/src/encoding/format/otlp.rs` | Frame-level OTLP serializer (incomplete: no native `Metric` support) |
| `lib/opentelemetry-proto/` | Generated OTel proto types + conversion to Vector events |

**The goal of this study**: replace `event.proto` and the Vector `Event` in-memory model with OTel
as the canonical in-process representation. All sources (including `vector` and `datadog_agent`)
remain as input adapters converting into OTel. The `vector` and `datadog` sinks are removed —
the only output wire format is OTLP, with other sinks (Prometheus, InfluxDB, etc.) acting as
OTel-consuming adapters.

---

## 2. Component-by-Component Analysis

### 2.1 Core Event Model (`lib/vector-core`) — Highest Complexity

This is the deepest and most impactful layer. Every other component depends on it.

| File | Lines | Nature of Change |
|---|---|---|
| `event/mod.rs` | 497 | Replace `Event::{Log,Metric,Trace}` enum with OTel signal types |
| `event/log_event.rs` | 1,221 | Replace with OTel `LogRecord` + `Resource` + `InstrumentationScope` |
| `event/trace.rs` | 192 | Replace with OTel `Span` (currently just a `LogEvent` newtype) |
| `event/metric/mod.rs` + `value.rs` + `data.rs` | ~2,300 | Replace with OTel metric signal types |
| `event/metadata.rs` | 611 | Remove `DatadogMetricOriginMetadata` and `datadog_api_key` shortcuts |
| `event/proto.rs` | 769 | Remove entirely — superseded by OTel proto |
| `lib/vector-core/proto/event.proto` | ~230 | Remove entirely |
| `metrics/ddsketch.rs` | 1,637 | Isolate as a DataDog-adapter-only type; remove from core |
| `event/vrl_target.rs` | large | Rewrite VRL bridge for OTel data shapes |

**Key structural tensions:**

#### MetricValue::Sketch and AgentDDSketch

`MetricValue::Sketch { sketch: MetricSketch::AgentDDSketch }` is a DataDog-exclusive construct
baked into the public core type. OTel has no equivalent; the closest is `ExponentialHistogram`,
which the current OTel-to-Vector metrics conversion already approximates (with information loss)
by converting to `AggregatedHistogram`. Moving to an OTel core means `AgentDDSketch` must
become a pure DataDog sink/source detail.

```rust
// lib/vector-core/src/event/metric/value.rs — current state
pub enum MetricValue {
    Counter { value: f64 },
    Gauge { value: f64 },
    Set { values: BTreeSet<String> },
    Distribution { samples: Vec<Sample>, statistic: StatisticKind },
    AggregatedHistogram { buckets: Vec<Bucket>, count: u64, sum: f64 },
    AggregatedSummary { quantiles: Vec<Quantile>, count: u64, sum: f64 },
    Sketch { sketch: MetricSketch },   // ← DataDog-only, must leave core
}

pub enum MetricSketch {
    AgentDDSketch(AgentDDSketch),      // ← DataDog agent-specific
}
```

#### DatadogMetricOriginMetadata in EventMetadata

`EventMetadata` carries `DatadogMetricOriginMetadata` (product/category/service) which propagates
through the entire pipeline — from the `log_to_metric` transform through to the DataDog metrics
encoder. This must move to a DataDog-specific side-channel (e.g., OTel resource attributes or a
sink-level extension).

```rust
// lib/vector-core/src/event/metadata.rs — current state
pub(super) struct Inner {
    ...
    pub(crate) datadog_origin_metadata: Option<DatadogMetricOriginMetadata>,  // ← must leave core
}
```

#### DataDog API key as a first-class EventMetadata method

`EventMetadata` exposes `set_datadog_api_key` / `datadog_api_key` as first-class methods with a
hardcoded constant. These should be demoted to the generic `Secrets` map.

```rust
// lib/vector-core/src/event/metadata.rs
const DATADOG_API_KEY: &str = "datadog_api_key";  // hardcoded in core

pub fn datadog_api_key(&self) -> Option<Arc<str>> { ... }
pub fn set_datadog_api_key(&mut self, secret: Arc<str>) { ... }
```

#### LogNamespace duality

The `LogNamespace::Vector` vs `LogNamespace::Legacy` distinction controls where metadata is placed
in a `LogEvent`. All sources carry dual-path code for both modes. With OTel as core,
`LogNamespace::Vector` (metadata separate from body) is the natural fit and `Legacy` can be
progressively deprecated.

#### TraceEvent is provisional

`TraceEvent` is currently just a `LogEvent` newtype. The existing `trace_to_log.rs` transform
comment even acknowledges this:

> "This will need to be updated when Vector's trace data model is finalized to properly handle
> trace-specific semantics and field mappings."

OTel provides that finalization — `TraceID`, `SpanID`, `ParentSpanID`, `SpanKind`, etc. become
first-class typed fields.

---

### 2.2 Vector Protocol — Source Kept, Sink Removed

The Vector gRPC protocol (`proto/vector/vector.proto` + `lib/vector-core/proto/event.proto`) is
used both to receive events from other Vector instances (source) and to forward events to
downstream Vector instances (sink). Under the new design:

- `src/sources/vector/` (~340 lines) — **kept** as an input adapter. It decodes the native Vector
  protobuf frames into OTel events instead of into the current `Event::Log/Metric/Trace` model.
- `src/sinks/vector/` (~791 lines across 4 files) — **removed entirely**. Vector-to-Vector
  forwarding is replaced by the standard OTel sink over OTLP gRPC.

| Component | Disposition | Lines |
|---|---|---|
| `src/sources/vector/mod.rs` | **Keep, adapt** — decode native proto → OTel | 340 |
| `src/sinks/vector/config.rs` | **Remove** | 247 |
| `src/sinks/vector/sink.rs` | **Remove** | 121 |
| `src/sinks/vector/service.rs` | **Remove** | 164 |
| `src/sinks/vector/mod.rs` | **Remove** | 259 |
| `proto/vector/vector.proto` | **Keep for source only** — no outbound RPC needed |  |
| `lib/vector-core/proto/event.proto` | **Remove** — superseded by OTel proto |  |
| `lib/codecs/src/decoding/format/native.rs` | **Remove** `NativeDeserializer` (source uses OTLP after Phase 3) |  |
| `lib/codecs/src/encoding/format/native.rs` | **Remove** `NativeSerializer` |  |
| `lib/codecs/src/decoding/format/native_json.rs` | **Remove** `NativeJsonDeserializer` |  |
| `lib/codecs/src/encoding/format/native_json.rs` | **Remove** `NativeJsonSerializer` |  |
| `lib/codecs/src/encoding/format/otlp.rs` | **Promote** to primary serializer (metric encoding not yet implemented) |  |
| `lib/codecs/src/decoding/format/otlp.rs` | **Promote** to primary deserializer |  |

The vector source today uses `NativeDeserializerConfig` to decode incoming protobuf frames:

```rust
// src/sources/vector/mod.rs — current
use vector_lib::codecs::NativeDeserializerConfig;
// decodes EventWrapper proto → Event::Log/Metric/Trace
```

After the migration the source will instead use the `OtlpDeserializer`, accepting OTLP frames
from upstream Vector instances that have also been migrated. During the transition period both
decoders can coexist behind a version-negotiation flag.

**Critical gap in the OTel encoder**: the OTLP serializer currently refuses native `Metric` events:

```rust
// lib/codecs/src/encoding/format/otlp.rs
Event::Metric(_) => {
    Err("OTLP serializer does not support native Vector metrics yet.".into())
}
```

This must be implemented before the vector sink can be retired and OTLP becomes the sole
inter-agent wire format.

---

### 2.3 VRL (`lib/vector-vrl`) — Medium Complexity

VRL is a transform language that operates on `VrlTarget` wrappers over Vector events. The bridge
lives in `lib/vector-core/src/event/vrl_target.rs`.

- The VRL `Metric` target exposes paths `.name`, `.namespace`, `.kind`, `.tags`, `.type` which
  mirror Vector's `Metric` struct. With OTel metrics, the path structure changes:
  `.resource.attributes`, `.scope.name`, `.data_points[n].attributes`, etc.
- Log field access changes: current `message` key convention becomes OTel `body`, severity maps to
  `severity_number`/`severity_text`, etc.
- `set_semantic_meaning` VRL function ties to Vector schema meaning constants (`SERVICE`, `HOST`,
  `TIMESTAMP`). These must map to OTel semantic conventions.
- `get_secret("datadog_api_key")` documented in `LEGACY_METADATA_KEYS` remains valid at the VRL
  layer — it just becomes a generic secret key lookup.

**VRL itself (language and compiler) does not change** — only the target shape and the mapping
functions from VRL values to OTel types.

---

### 2.4 DataDog Protocol — Source Kept, Sinks Removed

The DataDog protocol is treated symmetrically with the Vector protocol: the `datadog_agent` source
remains as an input adapter, all DataDog sinks are removed.

#### Source — kept and adapted

| Component | Lines | Change |
|---|---|---|
| `src/sources/datadog_agent/mod.rs` | 598 | Convert DD HTTP payloads → OTel events (instead of → Vector events) |
| `src/sources/datadog_agent/logs.rs` | 281 | Convert DD log payload → OTel `LogRecord` |
| `src/sources/datadog_agent/metrics.rs` | 609 | Convert `MetricPayload` / `SketchPayload` → OTel `Metric` signal |
| `src/sources/datadog_agent/traces.rs` | 333 | Convert `TracePayload` / `dd_trace.proto` → OTel `Span` |
| `src/common/datadog.rs` | ~200 | Unchanged (shared DD HTTP helpers, API key extraction) |
| `proto/vector/dd_metric.proto` | — | Retained for source-side decoding only |
| `proto/vector/dd_trace.proto` | — | Retained for source-side decoding only |
| `proto/vector/ddsketch_full.proto` | — | Retained for source-side DDSketch deserialization only |

The primary adaptation challenge in the source is the DataDog metrics → OTel mapping (see section 3).
The `AgentDDSketch` deserialized from a `SketchPayload` must be converted to an OTel
`ExponentialHistogram` at source ingestion time, with the precision trade-off documented.

#### Sinks — removed entirely

| Component | Lines | Reason for removal |
|---|---|---|
| `src/sinks/datadog/metrics/` (encoder + normalizer + sink) | ~2,600 | Replaced by OTel sink → DataDog OTel ingest endpoint |
| `src/sinks/datadog/traces/` (sink + apm_stats) | ~1,700 | Replaced by OTel sink; APM stats feature is dropped |
| `src/sinks/datadog/logs/` | ~1,300 | Replaced by OTel sink → DataDog OTel ingest endpoint |
| `src/sinks/datadog/events/` | ~440 | Replaced by OTel sink |

**Total DataDog sink code removed: ~6,040 lines.**

DataDog now natively accepts OTLP at its ingest endpoints. Users previously forwarding to DataDog
via the DataDog sink will reconfigure to use the OTel sink pointed at `https://api.datadoghq.com`.

#### APM Stats — dropped with the sink

The APM stats subsystem (`src/sinks/datadog/traces/apm_stats/`, ~1,451 lines) computes
Datadog-specific throughput and error statistics from trace spans, using `AgentDDSketch` for
latency distribution tracking. This is a proprietary DataDog feature with no OTel equivalent.
It is removed along with the DataDog traces sink. Users requiring APM stats must use the DataDog
Agent directly or a DataDog-native pipeline.

---

### 2.5 OTel Source and Sink — Promoted to Primary I/O

The OTel source and sink become the canonical inter-system protocol, replacing both the Vector
gRPC sink and all DataDog sinks.

#### OTel source — promoted to canonical input

The existing OTel source operates in two modes today:

1. **Legacy mode** (default): converts OTel protos → Vector `Event::Log / Metric / Trace`
   (lossy; flattens OTel resource/scope hierarchy into flat maps)
2. **`use_otlp_decoding` mode**: stores raw OTLP blobs as `Event::Log` with `resourceLogs`,
   `resourceMetrics`, `resourceSpans` top-level keys

With OTel as core, mode 2 becomes the only mode and mode 1 is eliminated. The `use_otlp_decoding`
flag and associated conditional code paths in `src/sources/opentelemetry/grpc.rs` and
`src/sources/opentelemetry/http.rs` (~460 lines) are simplified.

#### OTel sink — promoted to primary output, gRPC support required

| Current state | Required change |
|---|---|
| HTTP only (`src/sinks/opentelemetry/mod.rs`, ~104 lines) | Add gRPC support — essential for replacing the Vector gRPC sink in inter-agent topologies |
| Only forwards OTLP blobs (no conversion from native events) | Must accept any OTel-core event, not just pre-encoded OTLP blobs |
| No metric support for native `Metric` events | Blocked on OTel encoder completion (section 2.2) |

The OTel sink, once it supports gRPC, becomes the universal output adapter: users previously using
the `vector` sink, `datadog_metrics` sink, or `datadog_logs` sink all migrate to the OTel sink
pointed at their respective OTLP-compatible endpoints.

---

### 2.6 Transforms — Medium Complexity

| Transform | Change |
|---|---|
| `log_to_metric.rs` | Significant: sets `DatadogMetricOriginMetadata` on converted metrics; must adapt to OTel metric structure |
| `metric_to_log.rs` | Needs update for OTel metric field paths |
| `remap.rs` (VRL) | Depends on VrlTarget shape changes |
| `trace_to_log.rs` | Already provisional; becomes "OTel Span → OTel Log" |
| `aggregate.rs`, `reduce/`, `sample/` | Touch `MetricValue` matching; must adapt |
| `tag_cardinality_limit/` | Uses `MetricTags`; needs OTel `attributes` alignment |

---

### 2.7 Non-DataDog Metric Sinks — Low to Medium

Prometheus, InfluxDB, GrepTimeDB, etc. all match on `MetricValue` variants including
`MetricValue::Sketch`. With OTel as core, these receive OTel metric data points. The
`AgentDDSketch` handling in Prometheus and InfluxDB would be removed or moved to a dedicated
conversion step at the sink boundary.

---

## 3. The Critical Semantic Gap: DDSketch vs OTel Histograms

This is the single most technically challenging semantic issue in the migration.

### OTel metric types

| OTel Type | Vector equivalent |
|---|---|
| `Gauge` | `MetricValue::Gauge` |
| `Sum (monotonic, delta)` | `MetricValue::Counter` (incremental) |
| `Sum (monotonic, cumulative)` | `MetricValue::Counter` (absolute) |
| `Sum (non-monotonic)` | `MetricValue::Gauge` |
| `Histogram` | `MetricValue::AggregatedHistogram` |
| `ExponentialHistogram` | *(approximated as `AggregatedHistogram`, lossy)* |
| `Summary` | `MetricValue::AggregatedSummary` |
| *(no OTel equivalent)* | `MetricValue::Sketch { AgentDDSketch }` |
| *(no OTel equivalent)* | `MetricValue::Distribution` (raw samples) |
| *(no OTel equivalent)* | `MetricValue::Set` |

**The gap**: `AgentDDSketch` is the Datadog Agent's implementation of DDSketch, used both as a
Vector internal metric type and as the direct serialization format for the DataDog sketch ingest
endpoint. OTel's `ExponentialHistogram` is structurally similar but not equivalent. A lossless
migration path requires one of:

- Keeping `AgentDDSketch` as a DataDog-adapter-only extension type, never entering the OTel core
  model (preferred)
- Accepting that the Vector → DataDog sketch pipeline loses precision on non-DataDog-originated
  data

`MetricValue::Distribution` (raw samples) and `MetricValue::Set` (unique value sets) also have no
direct OTel counterparts. These would either need to be downconverted at source ingestion or
carried as opaque OTel attributes.

---

## 4. Complexity Summary

### By tier

| Tier | Scope | Files / Lines affected | Risk |
|---|---|---|---|
| **1 — Core event model** | `lib/vector-core/src/event/` + `EventMetadata` | ~6,000 lines across 10+ files | Breaking change for all downstream crates; all other tiers depend on it |
| **2 — DDSketch isolation** | `metrics/ddsketch.rs` + `MetricValue::Sketch` variant | 1,637 lines + call sites in 12 files | Must be converted at DataDog source ingestion; information loss acknowledged |
| **3 — VRL target rewrite** | `event/vrl_target.rs` + semantic meaning mapping | large + user-facing path API | User-visible breakage of existing VRL expressions |
| **4 — Vector sink removal** | `src/sinks/vector/` (4 files) | 791 lines removed | Breaks vector→vector forwarding until OTel gRPC sink is ready |
| **5 — DataDog sinks removal** | `src/sinks/datadog/` (all subsinks) | ~6,040 lines removed | Loss of APM stats feature; users must migrate to OTLP endpoints |
| **6 — Vector source adaptation** | `src/sources/vector/` | 340 lines | Must switch from `NativeDeserializer` to `OtlpDeserializer` |
| **7 — DataDog source adaptation** | `src/sources/datadog_agent/` | ~1,820 lines | DDSketch → OTel ExponentialHistogram conversion at ingestion |
| **8 — OTel codec promotion** | `otlp.rs` encoder (metric encoding missing) | medium | Gap in metric pipelines until completed |
| **9 — OTel sink gRPC addition** | `src/sinks/opentelemetry/` | medium (new gRPC module) | Inter-agent forwarding gap until gRPC is available |
| **10 — Native codec/proto removal** | 4 codec files + `event.proto` | ~800 lines removed | Depends on tiers 4, 6, 8, 9 completing first |
| **11 — Transforms adaptation** | ~8 transforms | medium | Behavioral changes in `log_to_metric` DD origin metadata |
| **12 — Non-DataDog metric sinks** | Prometheus, InfluxDB, GrepTimeDB | low | Remove sketch match arm; update field paths |

### Code delta summary

| Category | Lines added | Lines removed |
|---|---|---|
| DataDog sinks | 0 | ~6,040 |
| Vector sink | 0 | ~791 |
| Native codecs | 0 | ~800 |
| `event.proto` (vector native) | 0 | ~230 |
| OTel sink gRPC module | ~300 (est.) | 0 |
| OTel metric encoder | ~400 (est.) | 0 |
| Core event model rewrite | ~3,000 (est.) | ~6,000 |
| Source adaptations (vector + DD) | ~500 (est.) | ~500 |
| **Net** | **~4,200** | **~14,361** |

The migration is a significant net reduction in codebase size, primarily from removing the
proprietary sink stacks.

### Architecture after migration

```
Sources (input adapters)         Core (OTel canonical)                  Sinks (output adapters)
──────────────────────────────   ────────────────────────────────────   ───────────────────────────
opentelemetry (gRPC + HTTP)  ──► OTel LogRecord                    ──► opentelemetry (gRPC + HTTP)
datadog_agent ───────────────►   OTel Metric signal                ──► prometheus
  DD proto → OTel at boundary    OTel Span                         ──► influxdb, loki, …
vector (gRPC) ───────────────►                                     ──► (all OTLP-consuming adapters)
  native proto → OTel at boundary
other sources ───────────────►

REMOVED: src/sinks/vector/, src/sinks/datadog/
```

---

## 5. What Can Be Preserved As-Is

| Component | Reason |
|---|---|
| All source/sink transport infrastructure | HTTP/gRPC framing, TLS, acknowledgements, backpressure, buffers are protocol-agnostic |
| VRL language and compiler | Only the event target shape adapts, not the language itself |
| Enrichment tables | No dependency on the event wire format |
| `DataType::Log \| Metric \| Trace` topology dispatch | Maps cleanly to OTel signal types |
| `src/sources/datadog_agent/` | Kept as input adapter; conversion logic updated, transport unchanged |
| `src/sources/vector/` | Kept as input adapter; decoder swapped from native to OTLP |
| `src/sources/opentelemetry/` | Kept; `use_otlp_decoding` mode becomes the only mode |
| All non-DataDog, non-vector sinks | Prometheus, InfluxDB, Loki, Kafka, S3, etc. remain; only their metric field paths adapt |
| Secret management (`get_secret`, `set_secret`) | Becomes fully protocol-agnostic; `datadog_api_key` demoted to a generic key |

## What Is Definitively Removed

| Component | Lines | Notes |
|---|---|---|
| `src/sinks/vector/` | 791 | Replaced by `src/sinks/opentelemetry/` with gRPC support |
| `src/sinks/datadog/metrics/` | ~2,600 | Users migrate to OTel sink → DataDog OTLP endpoint |
| `src/sinks/datadog/traces/` + `apm_stats/` | ~3,150 | APM stats feature dropped; no OTel equivalent |
| `src/sinks/datadog/logs/` | ~1,300 | Users migrate to OTel sink → DataDog OTLP endpoint |
| `src/sinks/datadog/events/` | ~440 | Dropped |
| `lib/vector-core/proto/event.proto` | ~230 | Superseded by OTel proto |
| Native codecs (`native.rs`, `native_json.rs`) | ~800 | Superseded by `otlp.rs` codecs |
| `DatadogMetricOriginMetadata` in `EventMetadata` | — | Moves to DD source adapter boundary only |
| `MetricValue::Sketch` / `AgentDDSketch` in core | — | Moves to DD source adapter boundary only |

---

## 6. Recommended Phased Approach

A big-bang rewrite carries extreme risk given the deep coupling. The recommended path is
incremental, keeping the system working at each phase boundary.

### Phase 1 — Decouple DataDog-specific types from core

- Move `DatadogMetricOriginMetadata` out of `EventMetadata` into a DataDog-specific extension
- Make `MetricValue::Sketch` / `AgentDDSketch` a DataDog source-adapter-only construct
- Demote `datadog_api_key` to a generic secret key (remove the named constant from `metadata.rs`)
- Deprecate `LogNamespace::Legacy` in favor of `LogNamespace::Vector`

**Deliverable**: core event types with no DataDog-specific fields. DataDog sources and sinks still
work via a compatibility shim.

### Phase 2 — Complete OTel metric encoding and add OTel sink gRPC

Two parallel workstreams with no mutual dependency:

**2a — OTel metric encoder** (unblocks the vector sink removal):
- `Counter` → OTel `Sum` (monotonic)
- `Gauge` → OTel `Gauge`
- `AggregatedHistogram` → OTel `Histogram`
- `AggregatedSummary` → OTel `Summary`
- `Distribution` → OTel `ExponentialHistogram` (best effort) or `Histogram`
- `Set` → OTel attributes (opaque, with documented precision loss)

**2b — OTel sink gRPC support** (unblocks the vector sink removal):
- Add a gRPC protocol option to `src/sinks/opentelemetry/`
- Mirror the existing `src/sources/opentelemetry/grpc.rs` structure on the sink side

**Deliverable**: the OTel sink can replace the vector sink for inter-agent forwarding.

### Phase 3 — Remove the vector sink and DataDog sinks

With Phase 2 complete:
- **Remove** `src/sinks/vector/` (791 lines)
- **Remove** `src/sinks/datadog/` (all ~6,040 lines)
- Publish migration guide: `vector` sink → OTel sink gRPC; `datadog_*` sinks → OTel sink HTTP
- Mark native codecs (`native`, `native_json`) as deprecated

**Deliverable**: no proprietary output protocols. Users forwarding to DataDog use the OTel sink.
Vector-to-Vector topologies use the OTel sink over gRPC.

### Phase 4 — Adapt sources to emit OTel natively

- **`src/sources/datadog_agent/`**: convert DD proto payloads → OTel types at ingestion boundary
  (`AgentDDSketch` → `ExponentialHistogram`; `dd_trace` Span → OTel Span; DD log → OTel LogRecord)
- **`src/sources/vector/`**: switch decoder from `NativeDeserializer` to `OtlpDeserializer`;
  accept OTLP frames from upstream Vector instances running Phase 3+
- Eliminate `use_otlp_decoding` flag from OTel source (it becomes the only mode)

**Deliverable**: all sources emit OTel natively; the legacy conversion paths are gone.

### Phase 5 — Refactor `Event` enum to OTel signal types

- Replace `Event::Log(LogEvent)` with OTel `LogRecord`
- Replace `Event::Metric(Metric)` with OTel metric signal
- Replace `Event::Trace(TraceEvent)` with OTel `Span`
- Rewrite `vrl_target.rs` for OTel data shapes
- Update all transforms (`log_to_metric`, `metric_to_log`, `aggregate`, `reduce`, `sample`, etc.)

**Deliverable**: OTel as the canonical in-memory event type.

### Phase 6 — Remove native proto and codecs

- Delete `lib/vector-core/proto/event.proto`
- Delete `proto/vector/vector.proto` (source-side ingest path also upgraded to OTLP)
- Delete native codec files (`native.rs`, `native_json.rs`)
- Remove `LogNamespace::Legacy`

**Deliverable**: clean codebase — no proprietary wire protocol anywhere in core or sinks.

---

## 7. Key Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Breaking vector→vector topologies when the vector sink is removed | Phase 2b (OTel gRPC sink) must be validated before Phase 3 removes the vector sink; both can coexist during transition |
| Users relying on DataDog sinks for APM stats | APM stats feature is explicitly dropped; document this as a known capability loss; direct users to DataDog Agent for APM stats |
| DDSketch precision loss when DataDog source converts to OTel ExponentialHistogram | Document the conversion trade-off; DataDog-originated sketches that flow only to DataDog endpoints are unaffected since the DataDog OTLP endpoint reconstructs sketches from ExponentialHistogram |
| VRL expression breakage for users using field paths that change with OTel model | Provide a migration guide and deprecation warnings; Phase 5 is the latest this can break users, giving maximum lead time |
| OTel metric encoder gap (`Distribution`, `Set`) | Map at the DataDog source boundary with documented rules; `Set` can become a gauge with cardinality count |
| `log_to_metric` transform sets `DatadogMetricOriginMetadata` | Remove the DataDog origin injection from the transform in Phase 1; the DataDog sinks that consumed it are gone by Phase 3 |
| Upstream Vector instances on older protocol cannot talk to Phase 3+ instances | The vector source keeps backward-compatible reception; only the vector sink is removed, so older upstream nodes can still push to a new instance |
