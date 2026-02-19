# Protocol Gap Analysis: Vector Native Protocol vs OpenTelemetry (OTLP)

This document is a field-by-field comparison between Vector's native internal protocol
(`lib/vector-core/proto/event.proto`) and the OpenTelemetry protocol
(`lib/opentelemetry-proto/src/proto/opentelemetry-proto/`).

The analysis identifies every concept present in the Vector protocol and classifies it against
its OTel equivalent, highlighting regressions and breaking changes that would result from
migrating the core protocol to OTLP.

---

## Legend

| Symbol | Meaning |
|---|---|
| EXACT | Field maps 1:1 with identical semantics |
| LOSSLESS | Field maps to OTel with no data loss, but via a different structure |
| LOSSY | Field maps to OTel but information is degraded or approximated |
| EXTENDED | OTel field is a superset — migration gains expressiveness |
| NO-EQUIV | No equivalent exists in OTel; field is dropped or must be side-channelled |
| BREAKING | Semantic difference that will cause observable behavioral change |

---

## 1. Transport / RPC Layer

### Vector native gRPC service (`proto/vector/vector.proto`)

```protobuf
service Vector {
  rpc PushEvents(PushEventsRequest) returns (PushEventsResponse) {}
  rpc HealthCheck(HealthCheckRequest) returns (HealthCheckResponse) {}
}
```

| Vector concept | OTel equivalent | Status | Notes |
|---|---|---|---|
| Single `PushEvents` RPC carrying mixed event types | Three separate RPCs: `LogsService.Export`, `MetricsService.Export`, `TraceService.Export` | BREAKING | Vector sends logs, metrics, and traces in a single stream; OTel requires separate streams per signal type. Existing topologies using a single `vector` sink will need to be split into three OTel sinks. |
| `PushEventsResponse {}` (empty ack) | `ExportLogsServiceResponse { partial_success }`, same for metrics and traces | EXTENDED | OTel responses carry a `partial_success` field that reports how many items were rejected and an error message. Vector's empty ack provides no partial failure feedback. |
| `HealthCheckRequest / HealthCheckResponse { ServingStatus }` | Not part of OTLP; standard gRPC health check is a separate service (`grpc.health.v1`) | NO-EQUIV | Vector's health check RPC would need to be implemented as a standard gRPC health service alongside the OTel services. |
| `EventWrapper { oneof event { Log, Metric, Trace } }` | No equivalent — signals are transported in typed arrays per request | BREAKING | Vector wraps individual events polymorphically; OTel batches homogeneous signals per request. |
| `EventArray { oneof events { LogArray, MetricArray, TraceArray } }` | `ExportLogsServiceRequest { repeated ResourceLogs }` etc. | LOSSLESS | The batching concept is preserved, but OTel groups by Resource+Scope before batching, not just by signal type. |

---

## 2. Log Signal

### Vector `Log` message

```protobuf
message Log {
  map<string, Value> fields = 1;  // deprecated
  Value value = 2;
  Metadata metadata_full = 4;
}
```

### OTel `LogRecord` message (inside `ResourceLogs > ScopeLogs`)

```protobuf
message LogRecord {
  fixed64 time_unix_nano = 1;
  fixed64 observed_time_unix_nano = 11;
  SeverityNumber severity_number = 2;
  string severity_text = 3;
  AnyValue body = 5;
  repeated KeyValue attributes = 6;
  uint32 dropped_attributes_count = 7;
  fixed32 flags = 8;
  bytes trace_id = 9;
  bytes span_id = 10;
}
```

| Vector concept | OTel equivalent | Status | Notes |
|---|---|---|---|
| `Log.value` (`Value` — arbitrary nested document) | `LogRecord.body` (`AnyValue`) | LOSSLESS | Both represent an arbitrary structured value as the primary log content. Vector's `Value` and OTel's `AnyValue` support the same primitive types (string, int, float, bool, array, map, null/bytes). |
| `Log.fields` (deprecated map) | `LogRecord.attributes` | LOSSLESS | Legacy field; the migration can treat these as log attributes. |
| Vector timestamp (stored as a field in `value`, convention: `log_schema().timestamp_key`) | `LogRecord.time_unix_nano` | LOSSLESS | Vector stores the timestamp inside the free-form value map under a configurable key. OTel has a dedicated, typed `fixed64` field. This is structurally cleaner in OTel, but accessing the timestamp in VRL changes from `.timestamp` to a typed field. |
| Ingest timestamp (`vector.ingest_timestamp` in metadata under `LogNamespace::Vector`) | `LogRecord.observed_time_unix_nano` | LOSSLESS | OTel has a dedicated field for the observation time. Direct mapping. |
| Severity (stored as a free-form field under `log_schema().message_key`) | `LogRecord.severity_number` + `LogRecord.severity_text` | EXTENDED | OTel provides a 24-level numeric severity scale plus a free-form text label. Vector has no native severity concept in the log model — it is stored as an arbitrary field. Migration gains structured severity. |
| `trace_id` / `span_id` (stored as arbitrary fields under convention keys) | `LogRecord.trace_id` (16 bytes) + `LogRecord.span_id` (8 bytes) | LOSSLESS | OTel provides typed binary fields; Vector stores them as strings. The hex encoding used in Vector's OTel conversion (`to_hex`) is reversed at ingestion. |
| `LogRecord.flags` (W3C trace flags) | No Vector equivalent | EXTENDED | OTel gains W3C trace context flags, which Vector does not carry. |
| `Metadata.value` (arbitrary metadata blob) | `Resource.attributes["pipeline.metadata.*"]` | LOSSLESS | Carries pipeline-internal structured metadata alongside the event. Keys are flattened with dot notation under the reserved `pipeline.metadata.` prefix in resource attributes. |
| `Metadata.source_id` | `Resource.attributes["pipeline.source_id"]` | LOSSLESS | The ID of the source component that produced this event. Preserved as a resource attribute; useful for debugging fan-in topologies and tracing event provenance through the pipeline. |
| `Metadata.source_type` | `Resource.attributes["pipeline.source_type"]` | LOSSLESS | The type name of the ingestion component (e.g., `opentelemetry`, `datadog_agent`, `kafka`). Distinct from OTel's `telemetry.sdk.name` which describes the instrumentation library, not the pipeline ingestion point. Preserved as a dedicated resource attribute. |
| `Metadata.upstream_id` (component + port) | `Resource.attributes["pipeline.upstream_id"]` | LOSSLESS | The upstream component ID and output port that produced this event. Preserved as a resource attribute; valuable for pipeline graph debugging and routing traceability across multi-hop topologies. |
| `Metadata.secrets` | No equivalent | NO-EQUIV | Per-event secrets (e.g., `datadog_api_key`, `splunk_hec_token`) are a pipeline-internal routing mechanism. Must be handled at the connection/sink layer before OTel export. Not mapped to any OTel field. |
| `Metadata.source_event_id` (UUID) | `Resource.attributes["pipeline.source_event_id"]` | LOSSLESS | Per-event deduplication UUID assigned at ingestion. Preserved as a resource attribute for downstream deduplication and exactly-once processing patterns. |
| `Metadata.datadog_origin_metadata` | No equivalent | NO-EQUIV | DataDog-specific provenance; removed from core in Phase 1. Not mapped to OTel. |
| Resource grouping (no concept in Vector Log) | `ResourceLogs.resource` + `ScopeLogs.scope` | BREAKING | OTel requires logs to be grouped by emitting resource and instrumentation scope. Vector has no resource/scope concept. When converting Vector logs to OTel, a synthetic resource is constructed (e.g., using `source_type` as `service.name`). Downstream systems that rely on resource grouping will see a flat single-resource structure. |
| `schema_url` (no concept in Vector) | `ResourceLogs.schema_url` + `ScopeLogs.schema_url` | EXTENDED | OTel allows schema versioning. Not present in Vector; left empty on export. |
| `dropped_attributes_count` (no concept in Vector) | `LogRecord.dropped_attributes_count` | EXTENDED | OTel tracks attribute overflow. Vector has no attribute limit concept; this will always be 0 on export. |

---

## 3. Metric Signal

### Vector `Metric` message

```protobuf
message Metric {
  string name = 1;
  google.protobuf.Timestamp timestamp = 2;
  map<string, string> tags_v1 = 3;           // deprecated
  map<string, TagValues> tags_v2 = 20;        // multi-value tags
  Kind kind = 4;                              // Incremental | Absolute
  oneof value { Counter, Gauge, Set, Distribution1/2,
                AggregatedHistogram1/2/3, AggregatedSummary1/2/3, Sketch }
  string namespace = 11;
  uint32 interval_ms = 18;
  Metadata metadata_full = 21;
}
```

### OTel `Metric` message (inside `ResourceMetrics > ScopeMetrics`)

```protobuf
message Metric {
  string name = 1;
  string description = 2;
  string unit = 3;
  oneof data { Gauge, Sum, Histogram, ExponentialHistogram, Summary }
}
```

#### 3.1 Metric Identity and Metadata

| Vector concept | OTel equivalent | Status | Notes |
|---|---|---|---|
| `Metric.name` | `Metric.name` | EXACT | Direct mapping. |
| `Metric.namespace` | OTel convention: `{namespace}.{name}` combined in `Metric.name` | LOSSLESS | Vector separates namespace and name. OTel uses a dot-separated convention. On export: `"{namespace}.{name}"`. On import: split at first dot (fragile if name contains dots). |
| `Metric.timestamp` | `NumberDataPoint.time_unix_nano`, `HistogramDataPoint.time_unix_nano`, etc. | LOSSLESS | OTel timestamps are on the data point, not the metric container. Direct value mapping. |
| `interval_ms` (reporting interval) | `NumberDataPoint.start_time_unix_nano` + `time_unix_nano` (implied interval) | LOSSY | Vector carries the reporting interval explicitly. OTel encodes it implicitly via `start_time_unix_nano`. Exact interval recovery requires subtracting the two timestamps, which is only possible when both are set. |
| `Metric.tags_v2` (multi-value tags: `map<string, TagValues>`) | `NumberDataPoint.attributes` (`repeated KeyValue`) | LOSSLESS | Multi-value tags are supported in OTel via multiple `KeyValue` entries sharing the same key — but OTel spec requires unique keys per attribute set. **This is a breaking semantic difference**: Vector allows multiple values per tag key; OTel does not. Multi-value tags must be serialized as a single attribute with an array value or concatenated string. |
| `Metric.tags_v1` (single-value tags, deprecated) | `DataPoint.attributes` | LOSSLESS | Subset of tags_v2; maps directly. |
| `Metric.kind` (`Incremental` / `Absolute`) | OTel `AggregationTemporality` (`Delta` / `Cumulative`) on `Sum` and histogram types | LOSSLESS | `Incremental` → `Delta`; `Absolute` → `Cumulative`. However, OTel only carries temporality on `Sum`, `Histogram`, and `ExponentialHistogram` — not on `Gauge` or `Summary`. Vector's `kind` field applies uniformly to all metric types; this universality is lost. |
| Metric description (no field in Vector) | `Metric.description` | EXTENDED | OTel carries a human-readable description per metric. Not present in Vector; left empty on export. |
| Metric unit (no field in Vector) | `Metric.unit` | EXTENDED | OTel carries UCUM unit strings. Not present in Vector; left empty on export. |
| `Metadata.source_id`, `source_type`, `upstream_id`, `source_event_id` | `Resource.attributes["pipeline.*"]` | LOSSLESS | Same mapping as log signal — preserved under `pipeline.*` resource attribute namespace. |
| `Metadata.datadog_origin_metadata` | No equivalent | NO-EQUIV | Removed from core. |

#### 3.2 Metric Value Types

| Vector `MetricValue` variant | OTel `Metric.data` equivalent | Status | Notes |
|---|---|---|---|
| `Counter { value: f64 }` | `Sum { is_monotonic: true, data_points: [NumberDataPoint { as_double }] }` | LOSSLESS | Direct mapping. Temporality from `Metric.kind`. |
| `Gauge { value: f64 }` | `Gauge { data_points: [NumberDataPoint { as_double }] }` | LOSSLESS | Direct mapping. |
| `Set { values: BTreeSet<String> }` | **No equivalent** | NO-EQUIV | A set of unique string values has no representation in OTel. Options: (a) encode cardinality as a `Gauge`; (b) encode as a `Summary` with quantile 0.0 = min, 1.0 = max, count = cardinality; (c) encode as attributes on a synthetic event. All options lose the individual values. |
| `Distribution { samples, statistic }` | **No direct equivalent** | LOSSY | Raw sample distributions are not an OTel data type. Must be aggregated into `Histogram` (for `statistic = Histogram`) or `Summary` (for `statistic = Summary`) at conversion time. Individual sample values are lost. Alternatively, map to `ExponentialHistogram` which preserves more structure. |
| `AggregatedHistogram { buckets, count, sum }` | `Histogram { data_points: [HistogramDataPoint { bucket_counts, explicit_bounds, count, sum }] }` | LOSSLESS | Direct structural mapping. Vector `Bucket.upper_limit` → OTel `explicit_bounds`. Vector `Bucket.count` → OTel `bucket_counts`. `min` and `max` are optional in OTel (`HistogramDataPoint.min`, `HistogramDataPoint.max`) and not present in Vector's histogram — left absent. |
| `AggregatedSummary { quantiles, count, sum }` | `Summary { data_points: [SummaryDataPoint { quantile_values, count, sum }] }` | LOSSLESS | Direct structural mapping. |
| `Sketch { AgentDDSketch }` | `ExponentialHistogram` (approximate) | LOSSY | `AgentDDSketch` uses gamma-based bucket indexing (`k` = bin indexes, `n` = counts) with `γ ≈ 1.0156` (±0.78% relative error per quantile). OTel `ExponentialHistogram` at scale 7 uses base `2^(2^-7) ≈ 1.0055` (±0.27% — more precise). Conversion re-buckets the DDSketch `k[]`/`n[]` arrays into OTel `BucketSpan`/`bucket_counts`; `count` and `sum` map directly. **`AgentDDSketch.avg` is permanently lost**: it has no equivalent in `ExponentialHistogram`. It can be approximated as `sum/count` but the source code itself documents that `avg` loses precision during cross-instance merges ("loses precision when the two averages are close"). Negative `k` keys map to OTel negative buckets; `k=0` maps to `zero_count`. **This is a permanent precision loss for all DataDog-originated sketch data unless the DataDog source adapter performs the conversion before the event enters the OTel core.** |

#### 3.3 Historical / Versioned Metric Types (deprecated in Vector)

Vector's `event.proto` retains multiple versioned metric types for backward-compatible
deserialization of older event buffers:

| Vector legacy type | Status | Notes |
|---|---|---|
| `Distribution1` (parallel arrays) | NO-EQUIV (same as `Distribution2`) | Legacy encoding; dropped. |
| `AggregatedHistogram1` (parallel arrays with `uint32` counts) | Superseded by `AggregatedHistogram3` | All map to OTel `Histogram`; the version differences are internal encoding details. |
| `AggregatedHistogram2` (structured buckets, `uint32` counts) | Superseded | See above. |
| `AggregatedSummary1` (parallel arrays with `uint32` counts) | Superseded | All map to OTel `Summary`. |
| `AggregatedSummary2` (structured quantiles, `uint32` counts) | Superseded | See above. |

These versioned types exist only for buffer replay compatibility. On migration to OTel they are
all mapped to their current equivalent OTel type. Any persisted buffers using these legacy
encoding will need a migration tool or a buffer drain before cutover.

---

## 4. Trace Signal

### Vector `Trace` message

```protobuf
message Trace {
  map<string, Value> fields = 1;
  Metadata metadata_full = 3;
}
```

### OTel `Span` message (inside `ResourceSpans > ScopeSpans`)

```protobuf
message Span {
  bytes trace_id = 1;          // 16 bytes
  bytes span_id = 2;           // 8 bytes
  string trace_state = 3;
  bytes parent_span_id = 4;
  string name = 5;
  SpanKind kind = 6;
  fixed64 start_time_unix_nano = 7;
  fixed64 end_time_unix_nano = 8;
  repeated KeyValue attributes = 9;
  uint32 dropped_attributes_count = 10;
  repeated Event events = 11;
  uint32 dropped_events_count = 12;
  repeated Link links = 13;
  uint32 dropped_links_count = 14;
  Status status = 15;
}
```

| Vector concept | OTel equivalent | Status | Notes |
|---|---|---|---|
| `Trace` (a `LogEvent` newtype with arbitrary fields map) | `Span` (strongly typed) | BREAKING | This is the most structurally significant gap. Vector's trace is an untyped flat map — exactly the same structure as a `Log`. OTel `Span` has explicit typed fields for every standard trace attribute. Any span data carried in Vector's `TraceEvent` as arbitrary string fields must be explicitly mapped to OTel typed fields. |
| `fields["trace_id"]` (hex string, convention) | `Span.trace_id` (16 bytes, binary) | LOSSLESS | Convention → typed field. Hex decode required. |
| `fields["span_id"]` (hex string, convention) | `Span.span_id` (8 bytes, binary) | LOSSLESS | Convention → typed field. Hex decode required. |
| `fields["parent_span_id"]` (hex string, convention) | `Span.parent_span_id` (8 bytes, binary) | LOSSLESS | Convention → typed field. |
| `fields["name"]` | `Span.name` | LOSSLESS | Convention → typed field. |
| `fields["kind"]` (integer) | `Span.kind` (`SpanKind` enum: UNSPECIFIED/INTERNAL/SERVER/CLIENT/PRODUCER/CONSUMER) | LOSSLESS | Integer encoding → enum. |
| `fields["start_time_unix_nano"]` | `Span.start_time_unix_nano` (`fixed64`) | LOSSLESS | Convention → typed field. |
| `fields["end_time_unix_nano"]` | `Span.end_time_unix_nano` (`fixed64`) | LOSSLESS | Convention → typed field. |
| `fields["attributes"]` (nested map) | `Span.attributes` (`repeated KeyValue`) | LOSSLESS | Convention map → typed repeated field. |
| `fields["events"]` (array of maps) | `Span.events` (`repeated Event { time_unix_nano, name, attributes }`) | LOSSLESS | Convention array → typed repeated field. |
| `fields["links"]` (array of maps) | `Span.links` (`repeated Link { trace_id, span_id, trace_state, attributes }`) | LOSSLESS | Convention array → typed repeated field. |
| `fields["status"]` (nested map `{ code, message }`) | `Span.status` (`Status { code: StatusCode, message: string }`) | LOSSLESS | Convention map → typed struct. |
| `fields["trace_state"]` | `Span.trace_state` (W3C format string) | LOSSLESS | Convention → typed field. |
| `fields["resources"]` (convention from OTel source) | `ResourceSpans.resource.attributes` | LOSSLESS | Convention map → OTel `Resource`. |
| `fields["dropped_attributes_count"]` | `Span.dropped_attributes_count` (`uint32`) | LOSSLESS | Convention integer → typed field. |
| `fields["dropped_events_count"]` | `Span.dropped_events_count` (`uint32`) | LOSSLESS | Convention integer → typed field. |
| `fields["dropped_links_count"]` | `Span.dropped_links_count` (`uint32`) | LOSSLESS | Convention integer → typed field. |
| Any **non-standard fields** in `fields` (custom span attributes not in the OTel schema) | Must be encoded in `Span.attributes` | LOSSLESS | Arbitrary extra fields become span attributes. |
| `Metadata.*` (same as Log) | Same analysis as section 2 | (same) | See section 2. |
| `SpanKind` typed enum | `Span.kind` (`SpanKind`) | EXTENDED | Vector's integer kind has no enforced semantics. OTel's enum is standard and interoperable. |
| `Span.status.code` (`STATUS_CODE_UNSET / OK / ERROR`) | No equivalent in Vector Trace | EXTENDED | OTel provides a standardized status code. Vector has no native error/success concept on traces. |
| `InstrumentationScope` (no concept in Vector Trace) | `ScopeSpans.scope` (`InstrumentationScope { name, version, attributes }`) | EXTENDED | OTel groups spans by the library that produced them. Vector has no scope concept; synthetic scope is constructed (empty or from `source_type`). |

**Key observation**: because `TraceEvent` in Vector is just a `LogEvent` newtype, trace data
currently flows as convention-named string fields. The OTel migration effectively formalizes
and type-checks what was previously an informal schema. The conversion is lossless for all
fields that the existing OTel source already populates (since it already uses these conventions),
but **any span data that was produced by the DataDog agent source** uses different field
conventions (`service`, `resource`, `type`, `error`, etc.) and requires an explicit
field-by-field mapping.

---

## 5. Value Type System

### Vector `Value` (used everywhere in Log and Trace)

```protobuf
message Value {
  oneof kind {
    bytes raw_bytes = 1;
    google.protobuf.Timestamp timestamp = 2;
    int64 integer = 4;
    double float = 5;
    bool boolean = 6;
    ValueMap map = 7;
    ValueArray array = 8;
    ValueNull null = 9;
  }
}
```

### OTel `AnyValue` (used in LogRecord body and attributes)

```protobuf
message AnyValue {
  oneof value {
    string string_value = 1;
    bool bool_value = 2;
    int64 int_value = 3;
    double double_value = 4;
    ArrayValue array_value = 5;
    KeyValueList kvlist_value = 6;
    bytes bytes_value = 7;
  }
}
```

| Vector `Value` kind | OTel `AnyValue` equivalent | Status | Notes |
|---|---|---|---|
| `raw_bytes` | `bytes_value` | EXACT | Direct mapping. |
| `timestamp` (`google.protobuf.Timestamp`) | **No equivalent** | BREAKING | OTel's `AnyValue` has no timestamp type. Vector timestamps stored inside log body or as metric tags must be serialized to a string (ISO 8601) or integer (nanoseconds since epoch) when converting to OTel attributes or log body. **This is a breaking change**: any VRL expression reading a field as a timestamp will break because the field will be a string or integer in OTel. |
| `integer` (`int64`) | `int_value` (`int64`) | EXACT | Direct mapping. |
| `float` (`double`) | `double_value` (`double`) | EXACT | Direct mapping. |
| `boolean` | `bool_value` | EXACT | Direct mapping. |
| `map` (`ValueMap`) | `kvlist_value` (`KeyValueList`) | LOSSLESS | Both represent string-keyed maps of nested values. `ValueMap` uses `map<string, Value>` (unordered); `KeyValueList` uses `repeated KeyValue` (ordered, but keys must be unique per spec). No data loss, but iteration order becomes defined. |
| `array` (`ValueArray`) | `array_value` (`ArrayValue`) | EXACT | Both are heterogeneous arrays of nested values. |
| `null` | **No equivalent** | BREAKING | OTel `AnyValue` has no null type. Null values must be omitted, replaced with an empty string, or encoded as a string literal `"null"`. **Any VRL expression checking `field == null` will need updating.** |

### Impact summary for the Value type gap

The two missing types — `Timestamp` and `Null` — affect a large surface area because Vector's
VRL runtime operates on `Value` and both types are widely used:

- **Null**: used as the default/missing value sentinel throughout VRL (`if exists(.field) { ... }`),
  in conditional expressions, and in optional field handling. Every such expression must be
  reviewed.
- **Timestamp**: used as a first-class type for time-based operations in VRL (`now()`, `format_timestamp()`, `parse_timestamp()`, arithmetic). Timestamps stored in log/trace fields will
  become strings or integers, breaking any VRL that reads them without an explicit parse step.

---

## 6. Metadata / Pipeline Metadata

Vector's `Metadata` message carries fields that enable pipeline routing, provenance tracking, and
per-event secret injection. None of these have OTel equivalents because OTel is a transport
protocol, not a pipeline orchestration protocol.

| Vector `Metadata` field | OTel equivalent | Status | Notes |
|---|---|---|---|
| `value` (arbitrary metadata blob) | `Resource.attributes["pipeline.metadata.*"]` | LOSSLESS | Flattened under `pipeline.metadata.` prefix in resource attributes. |
| `datadog_origin_metadata` | None | NO-EQUIV | Removed from core; handled only in the DataDog source adapter. |
| `source_id` | `Resource.attributes["pipeline.source_id"]` | LOSSLESS | Source component ID. Preserved for pipeline provenance and fan-in debugging. |
| `source_type` | `Resource.attributes["pipeline.source_type"]` | LOSSLESS | Ingestion component type name. Distinct from `telemetry.sdk.name`. |
| `upstream_id` (component + port) | `Resource.attributes["pipeline.upstream_id"]` | LOSSLESS | Upstream component ID and port. Preserved for topology graph debugging. |
| `secrets` | None | NO-EQUIV | Pipeline-internal routing secrets. Resolved at the connection/sink layer; not carried in the event payload. |
| `source_event_id` (UUID) | `Resource.attributes["pipeline.source_event_id"]` | LOSSLESS | Per-event deduplication UUID. Preserved for exactly-once processing patterns. |

**Migration note**: `source_id` and `source_type` are used by Vector's schema system
(`schema::Definition`) and some sinks (e.g., the DataDog logs sink reads `source_type` to
populate `ddsource`). After migration these are read from `Resource.attributes["pipeline.source_id"]`
and `Resource.attributes["pipeline.source_type"]` rather than from `EventMetadata` methods.
The `pipeline.*` namespace is reserved and must not collide with application-level resource
attributes.

---

## 7. Acknowledgement and Delivery Semantics

| Vector concept | OTel equivalent | Status | Notes |
|---|---|---|---|
| `BatchNotifier` / `BatchStatus` (Delivered / Errored / Rejected) | `ExportLogsPartialSuccess.rejected_log_records` + `error_message` | LOSSY | Vector's three-state batch acknowledgement (Delivered / Errored / Rejected) maps to OTel's `partial_success` which only distinguishes accepted vs rejected count, with no per-event status. The OTel response does not distinguish between errors and explicit rejections. |
| Per-event `EventFinalizers` | None | NO-EQUIV | Vector tracks delivery confirmation at the individual event level. OTel operates at the batch level. Events within a batch are either all delivered or the batch is partially/fully rejected. Individual event-level acknowledgement is not possible in OTLP. |

---

## 8. Gap Summary Table

### Gaps by severity

#### Breaking changes (observable behavioral regressions)

| Gap | Affected signal | Impact |
|---|---|---|
| Mixed-signal single RPC replaced by three separate RPCs | All | Topology configurations using a single `vector` sink for all signal types must be reconfigured. |
| `Metric.tags_v2` multi-value tags → OTel single-value attributes | Metrics | Tags with multiple values must be serialized as array attributes. Downstream systems expecting multi-value tags will see a different structure. |
| `Value::Timestamp` has no OTel equivalent | Logs, Traces | Timestamps stored as fields become strings. VRL timestamp operations break without explicit parse. |
| `Value::Null` has no OTel equivalent | Logs, Traces | Null sentinel values disappear or become strings. VRL null-checks break. |
| `TraceEvent` (untyped flat map) → OTel `Span` (strongly typed) | Traces | Field conventions used by the DataDog source and any custom VRL trace manipulation must be explicitly remapped. |
| No `interval_ms` equivalent | Metrics | Reporting interval is implicit in OTel. Sinks that rely on `interval_ms` (e.g., InfluxDB write rate) must derive it from `start_time_unix_nano` instead. |
| `Sketch { AgentDDSketch }` → `ExponentialHistogram` (approximation) | Metrics | Irreversible precision loss for sketch data. `avg` field has no OTel equivalent (approximated as `sum/count`, with known floating-point drift during merges). Bucket re-indexing introduces ±0.78% → ±0.27% precision change (OTel is actually more precise at scale 7). |
| `Set { values }` → no equivalent | Metrics | Set cardinality data is lost unless encoded as opaque attributes. |
| `Distribution { samples }` → `Histogram` or `ExponentialHistogram` | Metrics | Raw sample values are lost; only aggregated bucket representation survives. |
| Per-event `EventFinalizers` (event-level ack) → batch-level OTel ack | All | Individual event delivery confirmation is no longer possible. |

#### Lossy mappings (data degraded but no hard failure)

| Gap | Affected signal | Impact |
|---|---|---|
| `Metric.namespace` encoded in `Metric.name` | Metrics | Namespace/name split must be recovered by splitting on the first `.`. Metric names containing `.` are ambiguous. |
| `Metadata.*` pipeline fields → `Resource.attributes["pipeline.*"]` | All | Pipeline provenance fields (`source_id`, `source_type`, `upstream_id`, `source_event_id`, `metadata.*`) are preserved under the `pipeline.` resource attribute namespace. Semantically a slight mismatch (resource attributes describe the emitting entity, not the pipeline hop), but the data is fully preserved and queryable. |
| `interval_ms` → implicit interval from timestamps | Metrics | Requires both `start_time_unix_nano` and `time_unix_nano` to be set for recovery. |
| `AggregatedHistogram` min/max | Metrics | OTel `HistogramDataPoint` has optional `min`/`max` fields that Vector does not carry. They will be absent on export. |
| Delivery status 3-state → 2-state | All | `BatchStatus::Errored` and `BatchStatus::Rejected` both map to `partial_success.rejected_*` with no distinction. |

#### No-equivalent fields (permanently dropped)

| Vector field | Affected signal | Notes |
|---|---|---|
| `Metadata.secrets` (per-event secrets) | All | Pipeline-internal routing mechanism; handled at the connection/sink layer. Not mapped. |
| `HealthCheck` RPC | Transport | Replaced by standard gRPC health check service. |
| `DatadogOriginMetadata` | Metrics | Removed from core in Phase 1 of the migration. |
| `Set` metric values (individual strings) | Metrics | No encoding path; cardinality-only representation is the fallback. |

#### Fields where OTel is a superset (gains from migration)

| OTel field | Affected signal | Gain |
|---|---|---|
| `LogRecord.severity_number` / `severity_text` | Logs | Standardized 24-level severity scale replaces ad-hoc severity fields. |
| `LogRecord.flags` (W3C trace flags) | Logs | Trace correlation flags on log records. |
| `Metric.description` + `Metric.unit` | Metrics | Human-readable documentation and UCUM units per metric. |
| `Span.kind` (`SpanKind` enum) | Traces | Standardized span relationship semantics. |
| `Span.status.code` | Traces | Standardized OK/ERROR/UNSET status codes. |
| `InstrumentationScope` on all signals | All | Library-level provenance for all signals. |
| `schema_url` on resource and scope | All | Schema versioning for telemetry data. |
| `ExportResponse.partial_success` | Transport | Per-batch rejection count and error message. |
| `Exemplar` on histogram and gauge data points | Metrics | OTel supports exemplar samples linking metrics to traces. Not present in Vector. |

---

## 9. VRL Impact Summary

Because VRL operates on field paths over `Value`, every gap in the value type system and every
field path change creates a VRL compatibility break.

| VRL pattern | Status after migration | Required change |
|---|---|---|
| `.timestamp` or custom timestamp field read as `Timestamp` type | BREAKS | Add explicit `parse_timestamp!(field, format)` |
| `field == null` / `exists(field)` for null sentinel | BREAKS | Fields become absent instead of null; `exists()` pattern still works but `== null` does not |
| `.tags.key` on metrics | BREAKS | Becomes `.attributes[n].value` — no direct key lookup syntax in OTel attributes |
| `.name`, `.kind`, `.namespace` on metrics | BREAKS | Metric name includes namespace; kind is per-data-point temporality |
| Trace field access `.trace_id`, `.span_id` as strings | BREAKS | Still string in OTel (hex-encoded bytes), but now explicitly in typed fields |
| Log `.message` field (legacy namespace) | BREAKS | Becomes `LogRecord.body`; accessed as `.body` |
| `get_secret("datadog_api_key")` | Works | Demoted to generic secret key; function still works |
| `set_semantic_meaning(...)` with `SERVICE`, `HOST`, `TIMESTAMP` | Needs update | Semantic meanings must map to OTel semantic conventions (`service.name`, `host.name`, etc.) |

---

## 10. Conclusion

The Vector native protocol and OTLP are not semantically equivalent. The migration introduces
**10 breaking changes**, **3 lossy mappings**, and **3 no-equivalent drops** alongside
**8 OTel superset gains**. Pipeline provenance metadata (`source_id`, `source_type`,
`upstream_id`, `source_event_id`, `metadata.*`) that previously had no OTel equivalent is now
preserved losslessly under the `pipeline.*` resource attribute namespace.

The most critical gaps are:

1. **`Value::Timestamp` and `Value::Null`** — first-class types in Vector with no OTel equivalent,
   affecting all VRL that manipulates timestamps or checks for null.
2. **Multi-value metric tags** — Vector allows multiple values per tag key; OTel requires unique
   attribute keys, requiring a structural transformation.
3. **`AgentDDSketch`** — precision loss is unavoidable unless kept entirely within the DataDog
   source/sink boundary.
4. **`Set` and `Distribution` metric types** — no OTel representation; data is lost or degraded
   at conversion.
5. **Mixed-signal RPC** — the single `vector` gRPC stream must become three separate OTLP streams,
   requiring topology reconfiguration for all existing vector→vector pipelines.
6. **Per-event delivery acknowledgement** — event-level ack is not possible in OTLP; only
   batch-level feedback is available.
