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
use vrl::value::Value;

use crate::{
    proto::{
        collector::{
            logs::v1::ExportLogsServiceRequest,
            metrics::v1::ExportMetricsServiceRequest,
            trace::v1::ExportTraceServiceRequest,
        },
        common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value},
        logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
        resource::v1::Resource,
        trace::v1::{ResourceSpans, ScopeSpans, Span},
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
    use vector_core::event::{EventArray, MetricKind, OtelLog, OtelMetric};
    use vrl::value::Value;

    use super::{VectorOtlpCodec, init};
    use vector_core::event::OtlpCodec as _;

    fn setup() {
        init();
    }

    #[test]
    fn round_trip_log() {
        setup();
        let log = OtelLog::from(Value::from("hello otlp"));
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
        let metric = OtelMetric::new_counter("requests_total", MetricKind::Incremental, 42.0);
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

    #[test]
    fn otlp_buffer_round_trip_via_encodable() {
        use vector_buffers::encoding::Encodable;

        setup();

        let log = OtelLog::from(Value::from("otlp buffer record"));
        let array = EventArray::from(log);

        let metadata = EventArray::get_metadata();
        let mut buf = Vec::new();
        array.encode(&mut buf).expect("encode failed");

        assert!(EventArray::can_decode(metadata));
        let decoded = EventArray::decode(metadata, buf.as_slice())
            .expect("decode failed");
        match decoded {
            EventArray::Logs(logs) => assert_eq!(logs.len(), 1),
            other => panic!("expected Logs, got {other:?}"),
        }
    }
}
