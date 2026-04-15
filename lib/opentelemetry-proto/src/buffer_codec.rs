/// `OtlpCodec` implementation for `EventArray ↔ OtlpBufferBatch`.
///
/// This codec is registered at process startup via
/// `vector_core::event::register_otlp_codec` so that `vector-core`'s disk-buffer
/// layer can encode/decode without a circular crate dependency.
///
/// The encode path uses `otel_logs_to_export`, `otel_metrics_to_export`, and
/// `otel_spans_to_export` to produce proto directly from OTel-native event
/// arrays.
use bytes::Bytes;
use prost::Message as _;
use vector_core::event::{
    EventArray, LogArray, MetricArray, OtelLogArray, OtelMetricArray, OtelSpanArray, OtlpCodec,
    TraceArray,
};
use vrl::{event_path, value::Value};

use crate::{
    spans,
    proto::{
        collector::{
            logs::v1::ExportLogsServiceRequest,
            metrics::v1::ExportMetricsServiceRequest,
            trace::v1::ExportTraceServiceRequest,
        },
        common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value},
        logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
        resource::v1::Resource,
        trace::v1::{
            ResourceSpans, ScopeSpans, Span, Status as SpanStatus,
            span::{Event as SpanEvent, Link},
        },
    },
};

/// Wire format: `OtlpBufferBatch` protobuf.
///
/// Defined here instead of in `vector-core` to avoid a circular dependency
/// (`opentelemetry-proto` → `vector-core` already exists).
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, prost::Message)]
struct OtlpBufferBatch {
    #[prost(message, optional, tag = "1")]
    logs: Option<ExportLogsServiceRequest>,
    #[prost(message, optional, tag = "2")]
    metrics: Option<ExportMetricsServiceRequest>,
    #[prost(message, optional, tag = "3")]
    traces: Option<ExportTraceServiceRequest>,
}

/// Register the OTLP buffer codec with `vector-core`.
///
/// Must be called once at process startup, before any disk buffer is opened with
/// `buffer_format = "otlp"` or `buffer_format = "migrate"`.
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn init() {
    vector_core::event::register_otlp_codec(Box::new(VectorOtlpCodec));
}

pub struct VectorOtlpCodec;

impl OtlpCodec for VectorOtlpCodec {
    fn encode(&self, array: &EventArray, buf: &mut Vec<u8>) -> Result<(), String> {
        event_array_to_batch(array)
            .encode(buf)
            .map_err(|e| format!("OtlpBufferBatch encode: {e}"))
    }

    fn decode(&self, buf: Bytes) -> Result<EventArray, String> {
        let batch =
            OtlpBufferBatch::decode(buf).map_err(|e| format!("OtlpBufferBatch decode: {e}"))?;
        Ok(batch_to_event_array(batch))
    }
}

// ---------------------------------------------------------------------------
// EventArray → OtlpBufferBatch
// ---------------------------------------------------------------------------

fn event_array_to_batch(array: &EventArray) -> OtlpBufferBatch {
    match array {
        EventArray::Logs(logs) => OtlpBufferBatch {
            logs: Some(otel_logs_to_export(logs)),
            ..Default::default()
        },
        EventArray::Metrics(metrics) => OtlpBufferBatch {
            metrics: Some(otel_metrics_to_export(metrics)),
            ..Default::default()
        },
        EventArray::Traces(traces) => OtlpBufferBatch {
            traces: Some(otel_spans_to_export(traces)),
            ..Default::default()
        },
    }
}

/// Re-encode a prost Message from `upstream_opentelemetry_proto::tonic` to the equivalent
/// `crate::proto` type. Both are generated from the same `.proto` files so the
/// wire format is identical.
fn transcode<S: prost::Message, D: prost::Message + Default>(src: &S) -> D {
    let mut buf = Vec::with_capacity(src.encoded_len());
    src.encode(&mut buf).expect("prost encode infallible");
    D::decode(buf.as_slice()).expect("wire-compatible proto decode")
}

// --- OTel-native logs -------------------------------------------------------

fn otel_logs_to_export(otel_logs: &OtelLogArray) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: otel_logs
            .iter()
            .map(|otel| {
                let record: LogRecord = transcode(otel.record());
                let resource: Option<Resource> = otel.resource().map(|r| transcode(r));
                let scope: Option<InstrumentationScope> = otel.scope().map(|s| transcode(s));
                ResourceLogs {
                    resource,
                    scope_logs: vec![ScopeLogs {
                        scope,
                        log_records: vec![record],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                }
            })
            .collect(),
    }
}

// --- OTel-native metrics ----------------------------------------------------

fn otel_metrics_to_export(otel_metrics: &OtelMetricArray) -> ExportMetricsServiceRequest {
    use crate::proto::metrics::v1::{Metric, ResourceMetrics, ScopeMetrics};

    ExportMetricsServiceRequest {
        resource_metrics: otel_metrics
            .iter()
            .map(|otel| {
                let metric: Metric = transcode(otel.metric());
                let resource: Option<Resource> = otel.resource().map(|r| transcode(r));
                let scope: Option<InstrumentationScope> = otel.scope().map(|s| transcode(s));
                ResourceMetrics {
                    resource,
                    scope_metrics: vec![ScopeMetrics {
                        scope,
                        metrics: vec![metric],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                }
            })
            .collect(),
    }
}

// --- OTel-native spans ------------------------------------------------------

fn otel_spans_to_export(otel_spans: &OtelSpanArray) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: otel_spans
            .iter()
            .map(|otel| {
                let span: Span = transcode(otel.span());
                let resource: Option<Resource> = otel.resource().map(|r| transcode(r));
                let scope: Option<InstrumentationScope> = otel.scope().map(|s| transcode(s));
                ResourceSpans {
                    resource,
                    scope_spans: vec![ScopeSpans {
                        scope,
                        spans: vec![span],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                }
            })
            .collect(),
    }
}

// --- Traces -----------------------------------------------------------------

/// Convert a single `TraceEvent` into an OTel `Span`.
///
/// Retained for backward compat with legacy buffer reads. Not used in the
/// current OTLP buffer path (which uses OtelSpan directly).
#[allow(dead_code)]
fn trace_event_to_span(trace: &vector_core::event::TraceEvent) -> Span {
    let trace_id = trace
        .get(event_path!(spans::TRACE_ID_KEY))
        .and_then(|v| hex_value_to_bytes(v, 16))
        .unwrap_or_default();

    let span_id = trace
        .get(event_path!(spans::SPAN_ID_KEY))
        .and_then(|v| hex_value_to_bytes(v, 8))
        .unwrap_or_default();

    let parent_span_id = trace
        .get(event_path!("parent_span_id"))
        .and_then(|v| hex_value_to_bytes(v, 8))
        .unwrap_or_default();

    let trace_state = trace
        .get(event_path!("trace_state"))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let name = trace
        .get(event_path!("name"))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let kind = trace
        .get(event_path!("kind"))
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as i32;

    let start_nanos = trace
        .get(event_path!("start_time_unix_nano"))
        .and_then(|v| v.as_timestamp())
        .and_then(|ts| ts.timestamp_nanos_opt())
        .unwrap_or(0) as u64;

    let end_nanos = trace
        .get(event_path!("end_time_unix_nano"))
        .and_then(|v| v.as_timestamp())
        .and_then(|ts| ts.timestamp_nanos_opt())
        .unwrap_or(0) as u64;

    let attributes = trace
        .get(event_path!(spans::ATTRIBUTES_KEY))
        .and_then(|v| value_to_kv_list(v))
        .unwrap_or_default();

    let dropped_attributes_count = trace
        .get(event_path!(spans::DROPPED_ATTRIBUTES_COUNT_KEY))
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;

    let events = trace
        .get(event_path!("events"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(value_to_span_event).collect())
        .unwrap_or_default();

    let dropped_events_count = trace
        .get(event_path!("dropped_events_count"))
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;

    let links = trace
        .get(event_path!("links"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(value_to_span_link).collect())
        .unwrap_or_default();

    let dropped_links_count = trace
        .get(event_path!("dropped_links_count"))
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;

    let status = trace
        .get(event_path!("status"))
        .and_then(value_to_span_status);

    Span {
        trace_id,
        span_id,
        trace_state,
        parent_span_id,
        name,
        kind,
        start_time_unix_nano: start_nanos,
        end_time_unix_nano: end_nanos,
        attributes,
        dropped_attributes_count,
        events,
        dropped_events_count,
        links,
        dropped_links_count,
        status,
    }
}

// ---------------------------------------------------------------------------
// OtlpBufferBatch → EventArray
// ---------------------------------------------------------------------------

fn batch_to_event_array(batch: OtlpBufferBatch) -> EventArray {
    if let Some(req) = batch.logs {
        let logs: LogArray = req
            .resource_logs
            .into_iter()
            .flat_map(|rl| {
                rl.into_otel_event_iter()
                    .filter_map(|e| e.try_into_log())
            })
            .collect();
        EventArray::Logs(logs)
    } else if let Some(req) = batch.metrics {
        let metrics: MetricArray = req
            .resource_metrics
            .into_iter()
            .flat_map(|rm| {
                rm.into_otel_event_iter()
                    .filter_map(|e| e.try_into_otel_metric())
            })
            .collect();
        EventArray::Metrics(metrics)
    } else if let Some(req) = batch.traces {
        let traces: TraceArray = req
            .resource_spans
            .into_iter()
            .flat_map(|rs| {
                rs.into_otel_event_iter()
                    .filter_map(|e| e.try_into_trace())
            })
            .collect();
        EventArray::Traces(traces)
    } else {
        EventArray::Logs(LogArray::default())
    }
}

// ---------------------------------------------------------------------------
// Shared metadata readers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn read_scope_from_trace_event(
    trace: &vector_core::event::TraceEvent,
) -> Option<InstrumentationScope> {
    let name = trace
        .get(event_path!(spans::SCOPE_KEY, spans::SCOPE_NAME_KEY))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let version = trace
        .get(event_path!(spans::SCOPE_KEY, spans::SCOPE_VERSION_KEY))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    let attributes = trace
        .get(event_path!(spans::SCOPE_KEY, spans::ATTRIBUTES_KEY))
        .and_then(|v| value_to_kv_list(v))
        .unwrap_or_default();

    if name.is_empty() && version.is_empty() && attributes.is_empty() {
        return None;
    }
    Some(InstrumentationScope {
        name,
        version,
        attributes,
        dropped_attributes_count: 0,
    })
}

// ---------------------------------------------------------------------------
// Span sub-object converters (Value → proto)
// ---------------------------------------------------------------------------

fn value_to_span_event(v: &Value) -> Option<SpanEvent> {
    let obj = v.as_object()?;
    Some(SpanEvent {
        time_unix_nano: obj
            .get("time_unix_nano")
            .and_then(|v| v.as_timestamp())
            .and_then(|ts| ts.timestamp_nanos_opt())
            .unwrap_or(0) as u64,
        name: obj
            .get("name")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
        attributes: obj
            .get("attributes")
            .and_then(|v| value_to_kv_list(v))
            .unwrap_or_default(),
        dropped_attributes_count: obj
            .get("dropped_attributes_count")
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as u32,
    })
}

fn value_to_span_link(v: &Value) -> Option<Link> {
    let obj = v.as_object()?;
    Some(Link {
        trace_id: obj
            .get("trace_id")
            .and_then(|v| hex_value_to_bytes(v, 16))
            .unwrap_or_default(),
        span_id: obj
            .get("span_id")
            .and_then(|v| hex_value_to_bytes(v, 8))
            .unwrap_or_default(),
        trace_state: obj
            .get("trace_state")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
        attributes: obj
            .get("attributes")
            .and_then(|v| value_to_kv_list(v))
            .unwrap_or_default(),
        dropped_attributes_count: obj
            .get("dropped_attributes_count")
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as u32,
    })
}

fn value_to_span_status(v: &Value) -> Option<SpanStatus> {
    let obj = v.as_object()?;
    Some(SpanStatus {
        message: obj
            .get("body")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
        code: obj
            .get("code")
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as i32,
    })
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

pub fn value_into_any_value(v: Value) -> any_value::Value {
    match v {
        Value::Bytes(b) => {
            any_value::Value::StringValue(String::from_utf8_lossy(&b).into_owned())
        }
        Value::Integer(i) => any_value::Value::IntValue(i),
        Value::Float(f) => any_value::Value::DoubleValue(f.into_inner()),
        Value::Boolean(b) => any_value::Value::BoolValue(b),
        Value::Null => any_value::Value::StringValue(String::new()),
        Value::Timestamp(ts) => any_value::Value::StringValue(
            ts.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        ),
        Value::Object(map) => {
            use crate::proto::common::v1::KeyValueList;
            let kvs = map
                .into_iter()
                .map(|(k, val)| KeyValue {
                    key: k.to_string(),
                    value: Some(AnyValue {
                        value: Some(value_into_any_value(val)),
                    }),
                })
                .collect();
            any_value::Value::KvlistValue(KeyValueList { values: kvs })
        }
        Value::Array(arr) => {
            use crate::proto::common::v1::ArrayValue;
            let vals = arr
                .into_iter()
                .map(|val| AnyValue {
                    value: Some(value_into_any_value(val)),
                })
                .collect();
            any_value::Value::ArrayValue(ArrayValue { values: vals })
        }
        Value::Regex(r) => any_value::Value::StringValue(r.to_string()),
    }
}

pub fn hex_value_to_bytes(v: &Value, expected_len: usize) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    let bytes = hex::decode(s.as_ref()).ok()?;
    (bytes.len() == expected_len).then_some(bytes)
}

pub fn value_to_kv_list(v: &Value) -> Option<Vec<KeyValue>> {
    let map = v.as_object()?;
    Some(
        map.iter()
            .map(|(k, val)| KeyValue {
                key: k.to_string(),
                value: Some(AnyValue {
                    value: Some(value_into_any_value(val.clone())),
                }),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use vector_core::event::{
        BufferFormat, EventArray, LogEvent, Metric, MetricKind, MetricValue, BUFFER_FORMAT,
    };
    use vrl::value::Value;

    use super::{VectorOtlpCodec, init};
    use vector_core::event::OtlpCodec as _;

    fn setup() {
        init();
    }

    #[test]
    fn round_trip_log() {
        setup();
        let log = LogEvent::from(Value::from("hello otlp"));
        let array = EventArray::from(log);

        let codec = VectorOtlpCodec;
        let mut buf = Vec::new();
        codec.encode(&array, &mut buf).expect("encode failed");

        let decoded = codec
            .decode(bytes::Bytes::from(buf))
            .expect("decode failed");

        match decoded {
            EventArray::Logs(logs) => {
                assert_eq!(logs.len(), 1);
                assert_eq!(
                    logs[0].body_string(),
                    "hello otlp"
                );
            }
            other => panic!("expected Logs, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_counter() {
        setup();
        let metric = Metric::new(
            "requests_total",
            MetricKind::Incremental,
            MetricValue::Counter { value: 42.0 },
        );
        let array = EventArray::from(metric);

        let codec = VectorOtlpCodec;
        let mut buf = Vec::new();
        codec.encode(&array, &mut buf).expect("encode failed");

        let decoded = codec
            .decode(bytes::Bytes::from(buf))
            .expect("decode failed");

        match decoded {
            EventArray::Metrics(metrics) => {
                assert_eq!(metrics.len(), 1);
                assert_eq!(metrics[0].name(), "requests_total");
            }
            other => panic!("expected Metrics, got {other:?}"),
        }
    }

    /// Simulates a buffer format migration through the `Encodable` trait on
    /// `EventArray`, exercising the full `encode()`/`decode()` data path that
    /// the disk buffer layer uses.
    ///
    /// Phases:
    /// 1. Vector mode: encode a log, capture the encoded bytes + metadata.
    /// 2. Migrate mode: decode the Vector-era bytes, then encode+decode a new
    ///    record (which should be OTLP-encoded).
    /// 3. Otlp mode: verify OTLP records still decode, and Vector metadata is
    ///    rejected.
    #[test]
    fn migrate_mode_decodes_vector_records_and_writes_otlp() {
        use vector_buffers::encoding::Encodable;

        setup();

        let log = LogEvent::from(Value::from("vector-era record"));
        let array = EventArray::from(log);

        // Phase 1: write a record in Vector mode.
        BUFFER_FORMAT.store(BufferFormat::Vector);
        let vector_metadata = EventArray::get_metadata();
        let mut vector_buf = Vec::new();
        array.clone().encode(&mut vector_buf).expect("Vector encode failed");

        // Phase 2: switch to Migrate mode.
        BUFFER_FORMAT.store(BufferFormat::Migrate);

        assert!(
            EventArray::can_decode(vector_metadata),
            "Migrate mode must accept Vector-encoded metadata"
        );
        let decoded = EventArray::decode(vector_metadata, vector_buf.as_slice())
            .expect("Migrate mode must decode Vector-encoded records");
        match &decoded {
            EventArray::Logs(logs) => assert_eq!(logs.len(), 1),
            other => panic!("expected Logs, got {other:?}"),
        }

        // Write a new record in Migrate mode — should use OTLP encoding.
        let migrate_metadata = EventArray::get_metadata();
        assert!(
            {
                use vector_core::event::EventEncodableMetadataFlags::*;
                let flags: vector_core::event::EventEncodableMetadata =
                    (DiskBufferV1CompatibilityMode | OtlpEncoding).into();
                migrate_metadata == flags
            },
            "Migrate mode metadata must include both V1 compat and OtlpEncoding flags"
        );

        let log2 = LogEvent::from(Value::from("migrate-era record"));
        let array2 = EventArray::from(log2);
        let mut otlp_buf = Vec::new();
        array2.encode(&mut otlp_buf).expect("Migrate encode failed");

        let decoded2 = EventArray::decode(migrate_metadata, otlp_buf.as_slice())
            .expect("Migrate mode must decode its own OTLP records");
        match decoded2 {
            EventArray::Logs(logs) => assert_eq!(logs.len(), 1),
            other => panic!("expected Logs, got {other:?}"),
        }

        // Phase 3: switch to Otlp mode.
        BUFFER_FORMAT.store(BufferFormat::Otlp);
        assert!(
            EventArray::can_decode(migrate_metadata),
            "Otlp mode must accept OTLP-encoded metadata"
        );
        assert!(
            !EventArray::can_decode(vector_metadata),
            "Otlp mode must reject Vector-encoded metadata"
        );

        let decoded3 = EventArray::decode(migrate_metadata, otlp_buf.as_slice())
            .expect("Otlp mode must decode OTLP records written during Migrate");
        match decoded3 {
            EventArray::Logs(logs) => assert_eq!(logs.len(), 1),
            other => panic!("expected Logs, got {other:?}"),
        }

        // Reset to default.
        BUFFER_FORMAT.store(BufferFormat::Vector);
    }
}
