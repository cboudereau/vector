# Performance Optimizations, Tradeoffs, and Competitive Analysis

This document covers three questions:

1. Which performance optimizations in the Vector native protocol would be disrupted or lost in an
   OTLP migration?
2. What are the full pros and cons of migrating to OTLP as the core protocol?
3. How does Vector compare to `opentelemetry-collector-contrib` and what does the migration mean
   in that context?

---

## 1. Vector Native Protocol Performance Optimizations

### 1.1 Unlimited gRPC message size

The vector source explicitly removes tonic's 4 MB default message size cap:

```rust
// src/sources/vector/mod.rs
.accept_compressed(tonic::codec::CompressionEncoding::Gzip)
.max_decoding_message_size(usize::MAX);  // removes the 4 MB tonic default
```

This allows a single gRPC frame to carry arbitrarily large batches — important when Vector
flushes a large disk buffer replay or a high-volume burst. The OTel source already sets the same
override on each of its three services:

```rust
// src/sources/opentelemetry/config.rs
.max_decoding_message_size(usize::MAX);  // set on log_service, metrics_service, trace_service
```

**Migration impact**: none. Both protocols already remove the 4 MB cap. The conscious trade-off
(unlimited size vs. DoS protection) is already consistently applied across both source
implementations.

### 1.2 Mixed-signal single gRPC stream

The vector protocol sends logs, metrics, and traces in a single `PushEvents` RPC call using the
`EventWrapper` oneof:

```protobuf
message EventWrapper {
  oneof event { Log log = 1; Metric metric = 2; Trace trace = 3; }
}
message PushEventsRequest {
  repeated EventWrapper events = 1;
}
```

This means one batch can carry heterogeneous events in a single RPC call.

OTLP requires three separate service RPCs (`LogsService.Export`, `MetricsService.Export`,
`TraceService.Export`). However, **tonic's `RoutesBuilder` already multiplexes all three services
on a single port and a single TCP connection** — Vector's OTel source does exactly this today:

```rust
// src/sources/opentelemetry/config.rs
let mut builder = RoutesBuilder::default();
builder
    .add_service(log_service)
    .add_service(metrics_service)
    .add_service(trace_service);
run_grpc_server_with_routes(self.grpc.address, tls, builder.routes(), shutdown)
```

HTTP/2 stream multiplexing means all three OTLP export calls share one TCP connection with no
extra handshake overhead. There is no "3× connections" cost.

The genuine difference is **batch scheduling**: the vector protocol flushes one batch per timeout
cycle regardless of signal mix; OTLP requires three independent batch timers (one per signal
type). For a low-volume agent emitting sparse metrics alongside dense logs, the metric batch timer
fires mostly empty. This is minor scheduling overhead, not a throughput regression.

**Migration impact**: no connection overhead. Batch scheduling splits into three independent
timers; this is a small scheduling complexity increase, not a performance regression. The
`RealtimeEventBasedDefaultBatchSettings` (1000 events, 1 second timeout) applies independently
per signal stream.

### 1.3 Proto-native batch size computation

The vector sink uses `prost::Message::encoded_len()` to compute the serialized byte size of an
`EventWrapper` directly, before encoding, as the batch size heuristic:

```rust
// src/sinks/vector/sink.rs
.batched(self.batch_settings.as_reducer_config(
    |data: &EventData| data.wrapper.encoded_len(),  // zero-copy size estimate
    ...
))
```

This works because the `event.proto` types are the in-memory representation — there is no
marshalling step between "event in memory" and "event in protobuf". The vector `Event` enum IS
the proto type.

With OTel as core, the internal representation changes to OTel types. `encoded_len()` on OTel
messages will still work, but the type conversion path changes. If a conversion step exists
between the internal type and the wire type (e.g., the current `From<EventArray> for
proto::EventArray` conversion in `event/proto.rs`), then `encoded_len()` requires the
intermediate representation to exist, potentially doubling memory pressure during batching.

**Migration impact**: acceptable if OTel types become the in-memory representation directly
(the goal of Phase 5). During the transition period (Phases 1–4), a conversion step will be
required for batching size estimation, increasing transient allocation.

### 1.4 LogEvent size caching with `AtomicCell`

`LogEvent` carries lock-free, lazily-computed byte size caches using crossbeam's `AtomicCell`:

```rust
// lib/vector-core/src/event/log_event.rs
struct Inner {
    fields: Value,
    size_cache: AtomicCell<Option<NonZeroUsize>>,           // in-memory byte size
    json_encoded_size_cache: AtomicCell<Option<NonZeroJsonSize>>, // estimated JSON size
}
```

The cache is invalidated only on `value_mut()` (mutable access). Read-only paths — including
routing, filtering, and the batching size estimator — pay zero cost after the first computation.

This optimization is tied to the `Value` type system. If the internal representation changes to
OTel `AnyValue`, the same caching pattern can be applied, but the estimated JSON size computation
must be re-implemented for `AnyValue` semantics (which differ from `Value` — notably `AnyValue`
has no `Timestamp` or `Null` type, so the size estimation logic changes).

**Migration impact**: the caching pattern survives, but the implementation must be rewritten for
OTel types. No structural performance regression, but non-trivial implementation work.

### 1.5 Arc Copy-on-Write for zero-copy fan-out

Both `LogEvent` and `EventMetadata` wrap their inner data in `Arc`, using `Arc::make_mut()` for
copy-on-write semantics:

```rust
// lib/vector-core/src/event/log_event.rs
pub fn value_mut(&mut self) -> &mut Value {
    let result = Arc::make_mut(&mut self.inner);
    result.invalidate();
    &mut result.fields
}
```

When an event is fanned out to multiple sinks (e.g., a `route` transform sending to both a
Prometheus sink and a Loki sink), the cloned `Event` values share the same `Arc` allocation.
Only the sink that mutates the event (e.g., adds a field) pays the clone cost. Read-only sinks
pay only the `Arc` reference count increment.

This optimization is independent of the wire protocol. It applies to whatever in-memory type
carries the data. OTel types can adopt the same `Arc<Inner>` wrapper pattern.

**Migration impact**: no regression if OTel types are designed with the same Arc CoW wrapper.

### 1.6 VRL program compilation caching

The `remap` transform caches compiled VRL programs keyed by `(EnrichmentTableRegistry,
SchemaDefinition)`:

```rust
// src/transforms/remap.rs
pub cache: Mutex<Vec<(CacheKey, Result<CacheValue, String>)>>,
```

VRL programs are compiled once at startup (or first use) and reused across events. The
compilation itself is independent of the wire protocol. However, the compiled program's type
checker is statically verified against the `SchemaDefinition`, which describes the shape of
the internal event type.

When the internal event type changes from `Value`-based `LogEvent` to OTel `LogRecord`, all
compiled VRL programs are invalidated and must be recompiled against the new schema. This is a
one-time startup cost, not a per-event cost, so there is no runtime regression — but all users
must re-validate their VRL programs for correctness after migration.

**Migration impact**: VRL programs must be recompiled and tested against new OTel field paths.
Runtime throughput is unaffected after the one-time recompilation.

### 1.7 Gzip compression on the gRPC stream

The vector sink supports optional gzip compression using tonic's built-in compression:

```rust
// src/sinks/vector/service.rs
if compression {
    proto_client = proto_client.send_compressed(tonic::codec::CompressionEncoding::Gzip);
}
```

The vector source accepts gzip-compressed messages:

```rust
// src/sources/vector/mod.rs
.accept_compressed(tonic::codec::CompressionEncoding::Gzip)
```

OTLP gRPC also supports gzip compression via tonic. The OTel source already enables it:

```rust
// src/sources/opentelemetry/grpc.rs (implicit via tonic)
```

**Migration impact**: no regression. Compression is a tonic-level feature available identically
for OTLP gRPC.

### 1.8 Event-level byte size tracking for backpressure

The topology uses `ByteSizeOf` on every event for accurate backpressure signalling. The disk
buffer writes checksummed (CRC32C) length-prefixed records sequentially:

- Data files capped at 128 MB
- Up to 65,536 files → max ~8 TB buffer
- Sequential writes, no random I/O

The disk buffer stores serialized `EventArray` protobuf bytes. After migration to OTel types,
the serialized format changes from `event.proto` to OTLP protobuf. **This is a breaking change
for any existing disk buffers** — persisted data written with the old format cannot be read by
a Vector instance using OTel as the core protocol. Disk buffers must be drained or discarded
before upgrading.

**Migration impact**: hard compatibility break for persisted disk buffers. Requires a planned
drain-before-upgrade operational procedure.

### 1.9 Chunked source send (`CHUNK_SIZE = 1000`)

The `SourceSender` sends events from sources to the topology in chunks of 1000:

```rust
// lib/vector-core/src/source_sender/mod.rs
pub const CHUNK_SIZE: usize = 1000;
```

This is independent of the wire protocol and survives migration unchanged.

### Summary: performance optimizations vs migration impact

| Optimization | Tied to native protocol? | Migration impact |
|---|---|---|
| Unlimited gRPC message size | No — both sources already set this | No regression; OTel source already applies `max_decoding_message_size(usize::MAX)` |
| Single mixed-signal gRPC stream | Yes — vector-protocol-specific | No connection overhead (tonic `RoutesBuilder` multiplexes all 3 OTLP services on one TCP/H2 connection); only difference is 3 independent batch timers instead of 1 |
| Zero-copy `encoded_len()` batch sizing | Partially | Survives if OTel types are the direct in-memory representation (Phase 5+) |
| `AtomicCell` size cache on `LogEvent` | Tied to `Value` type | Must be re-implemented for `AnyValue`; no runtime regression |
| Arc CoW zero-copy fan-out | Independent of protocol | Survives if OTel inner types use same `Arc<Inner>` pattern |
| VRL compilation cache | Independent of protocol | No runtime regression; all programs need re-testing |
| gzip compression | Independent of protocol | No regression; tonic supports it for OTLP identically |
| Disk buffer serialization format | Yes — uses `event.proto` | Hard compatibility break; disk buffers must be drained before upgrade |
| Chunked source send | Independent of protocol | No regression |

---

## 2. Pros and Cons of Migrating to OTLP as Core Protocol

### 2.1 Pros

#### Ecosystem interoperability
OTLP is now the de-facto standard for observability data exchange. Every modern observability
backend (Datadog, Grafana, New Relic, Honeycomb, Splunk, Dynatrace, Google Cloud, AWS) accepts
OTLP natively. Any Vector sink that targets an OTLP endpoint works with all of them without
format-specific adapters.

#### Elimination of double-conversion overhead
Currently, data arriving via the OTel source is converted from OTel proto → Vector `Event` types,
processed, then converted back to OTel (or another format) at the sink. With OTel as the core,
the source-to-core conversion disappears entirely for OTel-originated data. End-to-end pipeline
latency and CPU cost are reduced for the increasingly dominant OTel-native traffic.

#### Standardized semantic conventions
OTel defines semantic conventions for `service.name`, `host.name`, `http.method`, severity
levels, span kinds, etc. VRL and transforms operating on OTel-native data get standard, portable
field paths rather than Vector-specific or DataDog-specific conventions.

#### Richer data model for traces
Vector's `TraceEvent` is an untyped `LogEvent` newtype with no enforcement of span fields. OTel
`Span` provides typed fields for all standard trace attributes (`trace_id`, `span_id`,
`parent_span_id`, `kind`, `status`, `events`, `links`). This enables type-safe trace processing
without relying on string conventions.

#### Structured severity for logs
OTel provides a 24-level numeric `SeverityNumber` alongside `severity_text`. Vector has no native
severity concept in its log model. Migration gains a standardized severity representation that
downstream systems (Grafana, Loki, etc.) understand natively.

#### Significant codebase reduction
As established in the migration study: ~14,000 lines removed vs ~4,200 added. Removing the
DataDog sinks (~6,040 lines), vector sink (~791 lines), native codecs (~800 lines), and
DataDog-specific core types simplifies the maintenance surface considerably.

#### Future-proof protocol
OTel is governed by the CNCF, is vendor-neutral, and has strong industry backing. The vector
native protocol and DataDog protocols are maintained by a single vendor each. Basing the core
on OTel removes the risk of protocol divergence driven by vendor interests.

#### Instrumentation scope and resource grouping
OTel's `Resource` and `InstrumentationScope` carry richer provenance metadata than Vector's
`source_type` + `source_id` approach. Downstream analysis tools can use resource attributes
(`service.name`, `deployment.environment`, etc.) for filtering without VRL preprocessing.

### 2.2 Cons

#### Breaking changes for existing users

10 breaking changes are identified in the gap analysis:

- VRL expressions using `.timestamp` fields, `== null` checks, `.tags.key` on metrics, `.name`/
  `.namespace` on metrics, and `.message` on logs all require updates.
- Mixed-signal vector→vector topologies must be reconfigured to use three separate OTel sinks.
- Multi-value metric tags require an encoding decision that downstream consumers must handle.

#### Permanent data loss for specific types

- `MetricValue::Set` — unique string sets have no OTel representation. Cardinality-only
  conversion loses the individual values.
- `MetricValue::Distribution` (raw samples) — must be aggregated to histogram; individual
  observations are discarded.
- `AgentDDSketch` → `ExponentialHistogram` — precision loss for DataDog-originated sketch data
  outside the DataDog adapter path.

#### No per-event delivery acknowledgement

Vector's `EventFinalizers` provide per-event delivery confirmation, enabling precise
end-to-end acknowledgement from sink back to source. OTLP only provides batch-level
`partial_success` feedback. Any source that relies on per-event ack for flow control (e.g.,
Kafka sources with per-message commit, AMQP sources) loses that granularity.

#### Disk buffer migration is a hard cutover

Persisted disk buffers encoded with `event.proto` cannot be read by a Vector instance using OTel
types. An upgrade requires draining all disk buffers first, which means accepting potential data
loss during the upgrade window unless a parallel drain topology is run.

#### OTel has no pipeline metadata concept

`Metadata.secrets`, `upstream_id`, and `source_event_id` have no OTel equivalent. The secrets
mechanism (per-event `datadog_api_key`, `splunk_hec_token`) must be redesigned as
connection-level configuration rather than event-level routing. This affects any topology that
routes events to different backends based on per-event secrets.

#### Three-stream architecture: operational complexity vs congestion isolation

An agent collecting logs, metrics, and traces now manages three independent gRPC streams at the
wire level rather than one. Each stream has its own batch timeout. Operational monitoring must
track three streams per downstream Vector hop.

However, the three-stream model also provides a concrete **congestion isolation benefit** that
the single-stream Vector model does not. Each OTel signal type maps to its own `SourceSender`
named output channel (`logs`, `metrics`, `traces`), backed by an independent `Output` buffer
with independent backpressure:

```rust
// lib/vector-core/src/source_sender/sender.rs
pub(super) named_outputs: HashMap<String, Output>,  // one Output per signal type
```

The consequence: a metrics burst (e.g. a large Prometheus scrape) fills the metrics channel and
causes `MetricsService.export()` to return `Status::unavailable` to the sender. Logs and traces
continue flowing and acking on their own channels, unaffected. With the Vector single-channel
model (`send_batch` to `default_output`), the same metrics burst fills the shared channel and
stalls all signal types simultaneously.

This isolation is at the **channel level**, not the TCP level. All three streams share one TCP
connection and one HTTP/2 session via `RoutesBuilder`. An OS-level TCP stall or connection drop
still affects all three signals. The isolation only applies to internal pipeline congestion —
which is the dominant failure mode in practice (slow sinks, full disk buffers).

**Net assessment**: the operational complexity (three batch timers, three backpressure metrics
to monitor) is real and worth tracking. The congestion isolation benefit is also real and
meaningful for mixed-signal workloads where signal volume is asymmetric. For a dedicated
single-signal pipeline (logs only), the three-stream model adds overhead without benefit and
the Vector single-channel model would have been more efficient. The trade-off favours the
three-stream model for production mixed-signal deployments.

#### APM stats — ported, not dropped

The DataDog APM stats subsystem (`~514 lines` of algorithm) is being backported as a
vendor-neutral `apm_stats` transform that emits standard OTel metrics (`spans.hits`,
`spans.errors`, `spans.duration.ok`, `spans.duration.error` as `ExponentialHistogram`).
The `AgentDDSketch` dependency is replaced by an OTel-native accumulator at scale 7.
See `APM_STATS_OTLP_BACKPORT.md` for the full specification.

#### `Timestamp` and `Null` type gaps require pervasive VRL changes

The absence of `Value::Timestamp` and `Value::Null` in OTel's `AnyValue` is not a localized
change. Any VRL expression that reads a timestamp from a field, checks for null, or uses
`now()` in combination with field arithmetic requires review and updating. The scope of this
change depends on each user's VRL programs but could be extensive.

---

## 3. Vector vs opentelemetry-collector-contrib

`opentelemetry-collector-contrib` (otel-col-contrib) is the reference OTLP pipeline
implementation. Understanding the competitive and complementary positioning helps clarify what
the OTLP migration means strategically.

### 3.1 Where Vector has a structural advantage today

#### VRL — a typed, compiled transformation language

Vector's killer differentiator is VRL. It is a domain-specific language compiled to a typed
program, with schema-aware static analysis at startup. otel-col-contrib has no equivalent —
its transform processors (`transform`, `filter`) use OTTL (OpenTelemetry Transformation
Language), which is less powerful than VRL.

OTTL differences vs VRL:
- OTTL operates on individual statements without composable functions; VRL is a full expression
  language with closures, loops, pattern matching, and error handling
- OTTL has no equivalent to VRL's enrichment table lookups
- VRL has a compile-time type system that catches errors before deployment; OTTL validates at
  runtime
- VRL can call DNS lookups, parse structured formats (CSV, nginx logs, syslog, etc.) natively;
  OTTL cannot

**After migration**: VRL is preserved, but its field paths change to match OTel structure. The
language advantage remains.

#### Disk buffer with durable persistence

Vector's disk buffer provides durable, CRC32C-checksummed, sequential event storage up to ~8 TB
with configurable `WhenFull` semantics (`Block`, `DropNewest`, `Overflow`). The overflow
semantics allow chaining memory and disk buffers in a cascade.

otel-col-contrib has a persistent queue (`PersistentQueue`) backed by `bbolt` (a key-value
store). It provides durability but with different semantics — it is per-exporter, not
per-component, and does not support the cascade model.

**After migration**: disk buffer survives unchanged. The disk buffer serialization format changes
(from `event.proto` to OTel proto), requiring a drain-before-upgrade, but the operational
semantics remain.

#### Unified multi-signal topology with backpressure

Vector models its entire pipeline as a single topology graph with end-to-end backpressure
propagated through `SourceSender` → buffer → sink. The `WhenFull::Block` mode propagates
backpressure all the way to the source, which can slow down a gRPC source's accept loop.

otel-col-contrib pipelines are per-signal (a `logs` pipeline, a `metrics` pipeline, a `traces`
pipeline) and do not share backpressure across signal types. Cross-signal correlation within a
single topology is not natively supported.

**After migration**: the unified topology model is preserved. The three-stream OTel wire format
at the edges does not affect intra-topology backpressure.

#### Enrichment tables

Vector supports enrichment table lookups (CSV, VictoriaMetrics, GeoIP) directly in VRL
transforms with sub-millisecond latency via in-memory caching. otel-col-contrib has no
equivalent mechanism — attribute enrichment requires an external lookup processor or a custom
connector.

**After migration**: enrichment tables are preserved unchanged.

#### Sources with proprietary protocol support

Vector sources include Kafka (with offset management), AMQP, NATS, AWS Kinesis, GCP Pub/Sub,
Azure Event Hubs, Fluent, LogStash, etc. Many of these are not available or are less mature in
otel-col-contrib's receiver ecosystem.

The DataDog agent source (ingesting `dd_metric.proto`, `dd_trace.proto`) is a particularly
important example — keeping it as a source adapter means Vector can still act as an
intermediary for DataDog agents, which otel-col-contrib cannot do natively.

**After migration**: all sources are preserved. The DataDog agent source stays as an input
adapter. This is a continuing differentiation point.

### 3.2 Where otel-col-contrib has an advantage today

#### Native OTLP throughput
otel-col-contrib is purpose-built for OTLP. Its internal pipeline operates directly on OTel
proto types without any conversion layer. Vector currently pays a conversion cost at every OTel
source/sink boundary (OTel proto → Vector `Event` → OTel proto).

**After migration**: this conversion cost disappears. Vector's throughput for OTel-native traffic
approaches otel-col-contrib's, because both operate directly on OTel types.

#### Connector ecosystem
otel-col-contrib has 100+ receivers, processors, and exporters contributed by the broader
community, including vendor-specific exporters for every major observability backend. New
connectors follow a standard interface and are added at high frequency.

Vector has a smaller but growing sink/source ecosystem. Many of Vector's sinks are maintained
by the core team rather than contributed by vendors.

**After migration**: no structural change. Vector's ecosystem grows independently; the migration
does not affect how sources and sinks are added.

#### Tail-sampling and span-level routing
otel-col-contrib's `tailsampling` processor makes sampling decisions on complete traces (all
spans for a trace ID collected before the decision). Vector has no equivalent today — it can
only do head-based or probabilistic sampling via the `sample` transform.

**After migration**: two new transforms are planned — `trace_assembler` (groups spans by
`trace_id`, detects completeness, hard-evicts after a deadline) and `tail_sample` (evaluates
VRL policies against the assembled trace, explodes kept spans back to individual events).
Span-level routing after sampling reuses the existing `route` transform unchanged.
See `TAIL_SAMPLING_BACKPORT.md` for the full specification. After implementation, Vector
gains capabilities that exceed otel-col-contrib in this area (VRL policies, enrichment table
lookups, span-level routing, durable disk buffering).

#### Schema management with OTLP schema URL
otel-col-contrib processors respect `schema_url` for automatic field migration between OTel
semantic convention versions. Vector has no schema URL concept today.

**After migration**: `schema_url` appears in OTel signals but Vector does not actively use it.
This remains a gap relative to otel-col-contrib.

### 3.3 Positioning after migration

```
Before migration:
  OTel sources ──► [OTel→Vector conversion] ──► Vector pipeline ──► [Vector→OTel conversion] ──► OTel backends
                    ^ performance cost                                  ^ performance cost
  DataDog sources ─────────────────────────────────────────────────► DataDog backends (exclusive)

After migration:
  OTel sources ──────────────────────────────► Vector pipeline ──► OTel backends (zero conversion)
  DataDog sources ─► [DD→OTel conversion] ──► Vector pipeline ──► OTel backends (DataDog's OTLP endpoint)
  Kafka / AMQP / etc. ──────────────────────► Vector pipeline ──► OTel backends (unique to Vector)
```

After migration, Vector's positioning becomes:

> **The high-performance OTLP-native pipeline with the richest ingestion source ecosystem, durable
> disk buffering, and VRL — for users who need more than what otel-col-contrib provides at the
> edges but want standards-compliant OTLP in the core.**

The migration does not make Vector a replacement for otel-col-contrib in all scenarios. The two
remain complementary:

- Vector excels at **edge ingestion** (diverse sources, VRL enrichment, disk buffering)
- otel-col-contrib excels at **collector-tier processing** (tail sampling, schema migration,
  100+ vendor exporters)
- A Vector → otel-col-contrib pipeline (Vector at the edge, otel-col-contrib at the collector
  tier) becomes entirely natural after migration because the wire format is identical.

### 3.4 Migration benefit specific to otel-col-contrib interoperability

Today, connecting a Vector instance to an otel-col-contrib collector requires either:
- Configuring Vector's OTel sink (HTTP only, no native metric support, `use_otlp_decoding` quirks)
- Running a custom adapter

After migration, any Vector instance can connect to any otel-col-contrib receiver with zero
configuration friction. The `PushEvents` → `Export` RPC translation disappears. Vector becomes
a first-class node in any OTLP pipeline graph.
