/// `OtlpCodec` implementation for `EventArray ↔ OtlpBufferBatch`.
///
/// This codec is registered at process startup via
/// `vector_core::event::register_otlp_codec` so that `vector-core`'s disk-buffer
/// layer can encode/decode without a circular crate dependency.
use bytes::Bytes;
use prost::Message as _;
use vector_core::event::{EventArray, LogArray, MetricArray, OtlpCodec, TraceArray};
use vrl::{event_path, value::Value};

use crate::{
    proto::{
        collector::{
            logs::v1::ExportLogsServiceRequest,
            metrics::v1::ExportMetricsServiceRequest,
            trace::v1::ExportTraceServiceRequest,
        },
        common::v1::{AnyValue, KeyValue, any_value},
        logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
        trace::v1::{ResourceSpans, ScopeSpans, Span},
    },
    spans,
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
            logs: Some(logs_to_export(logs)),
            ..Default::default()
        },
        EventArray::Metrics(metrics) => OtlpBufferBatch {
            metrics: Some(metrics_to_export(metrics)),
            ..Default::default()
        },
        EventArray::Traces(traces) => OtlpBufferBatch {
            traces: Some(traces_to_export(traces)),
            ..Default::default()
        },
    }
}

// --- Logs -------------------------------------------------------------------

fn logs_to_export(logs: &LogArray) -> ExportLogsServiceRequest {
    let records: Vec<LogRecord> = logs.iter().map(|log| {
        let body = value_into_any_value(log.value().clone());

        let trace_id = log
            .get(event_path!(crate::logs::TRACE_ID_KEY))
            .and_then(|v| hex_value_to_bytes(v, 16))
            .unwrap_or_default();

        let span_id = log
            .get(event_path!(crate::logs::SPAN_ID_KEY))
            .and_then(|v| hex_value_to_bytes(v, 8))
            .unwrap_or_default();

        LogRecord {
            time_unix_nano: 0,
            observed_time_unix_nano: 0,
            severity_number: 0,
            severity_text: String::new(),
            body: Some(AnyValue {
                value: Some(body),
            }),
            attributes: vec![],
            dropped_attributes_count: 0,
            flags: 0,
            trace_id,
            span_id,
        }
    }).collect();

    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: None,
            scope_logs: vec![ScopeLogs {
                scope: None,
                log_records: records,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

// --- Metrics ----------------------------------------------------------------

fn metrics_to_export(metrics: &MetricArray) -> ExportMetricsServiceRequest {
    use crate::proto::metrics::v1::{ResourceMetrics, ScopeMetrics};

    let otel_metrics: Vec<crate::proto::metrics::v1::Metric> = metrics
        .iter()
        .map(crate::metrics::metric_event_to_otel_metric)
        .collect();

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: None,
            scope_metrics: vec![ScopeMetrics {
                scope: None,
                metrics: otel_metrics,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

// --- Traces -----------------------------------------------------------------

fn traces_to_export(traces: &TraceArray) -> ExportTraceServiceRequest {
    let otel_spans: Vec<Span> = traces.iter().map(trace_event_to_span).collect();
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: None,
            scope_spans: vec![ScopeSpans {
                scope: None,
                spans: otel_spans,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

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

    Span {
        trace_id,
        span_id,
        trace_state: String::new(),
        parent_span_id,
        name,
        kind,
        start_time_unix_nano: start_nanos,
        end_time_unix_nano: end_nanos,
        attributes,
        dropped_attributes_count: 0,
        events: vec![],
        dropped_events_count: 0,
        links: vec![],
        dropped_links_count: 0,
        status: None,
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
                rl.into_event_iter(vector_core::config::LogNamespace::Vector)
                    .filter_map(|e| e.try_into_log())
            })
            .collect();
        EventArray::Logs(logs)
    } else if let Some(req) = batch.metrics {
        let metrics: MetricArray = req
            .resource_metrics
            .into_iter()
            .flat_map(|rm| {
                rm.into_event_iter()
                    .filter_map(|e| e.try_into_metric())
            })
            .collect();
        EventArray::Metrics(metrics)
    } else if let Some(req) = batch.traces {
        let traces: TraceArray = req
            .resource_spans
            .into_iter()
            .flat_map(|rs| {
                rs.into_event_iter()
                    .filter_map(|e| e.try_into_trace())
            })
            .collect();
        EventArray::Traces(traces)
    } else {
        EventArray::Logs(LogArray::default())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn value_into_any_value(v: Value) -> any_value::Value {
    match v {
        Value::Bytes(b) => {
            any_value::Value::StringValue(String::from_utf8_lossy(&b).into_owned())
        }
        Value::Integer(i) => any_value::Value::IntValue(i),
        Value::Float(f) => any_value::Value::DoubleValue(f.into_inner()),
        Value::Boolean(b) => any_value::Value::BoolValue(b),
        Value::Null => any_value::Value::StringValue(String::new()),
        Value::Timestamp(ts) => any_value::Value::StringValue(ts.to_rfc3339()),
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

fn hex_value_to_bytes(v: &Value, expected_len: usize) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    let bytes = hex::decode(s.as_ref()).ok()?;
    (bytes.len() == expected_len).then_some(bytes)
}

fn value_to_kv_list(v: &Value) -> Option<Vec<KeyValue>> {
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
    use vector_core::event::{EventArray, LogEvent, Metric, MetricKind, MetricValue};
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
                    logs[0].value(),
                    &Value::from("hello otlp")
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
}
