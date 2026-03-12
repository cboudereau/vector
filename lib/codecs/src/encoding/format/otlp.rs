use crate::encoding::ProtobufSerializer;
use bytes::BytesMut;
use opentelemetry_proto::{
    metrics::encode_metric_to_request,
    proto::{
        DESCRIPTOR_BYTES, LOGS_REQUEST_MESSAGE_TYPE, METRICS_REQUEST_MESSAGE_TYPE,
        RESOURCE_LOGS_JSON_FIELD, RESOURCE_METRICS_JSON_FIELD, RESOURCE_SPANS_JSON_FIELD,
        TRACES_REQUEST_MESSAGE_TYPE,
        collector::{
            logs::v1::ExportLogsServiceRequest,
            metrics::v1::ExportMetricsServiceRequest,
            trace::v1::ExportTraceServiceRequest,
        },
        common::v1::InstrumentationScope as ProtoScope,
        logs::v1::{LogRecord as ProtoLogRecord, ResourceLogs, ScopeLogs},
        metrics::v1::{Metric as ProtoMetric, ResourceMetrics, ScopeMetrics},
        resource::v1::Resource as ProtoResource,
        trace::v1::{ResourceSpans, ScopeSpans, Span as ProtoSpan},
    },
};
use prost::Message;
use tokio_util::codec::Encoder;
use vector_config_macros::configurable_component;
use vector_core::{
    config::DataType,
    event::{Event, OtelLogEvent, OtelMetricEvent, OtelSpanEvent},
    schema,
};
use vrl::protobuf::encode::Options;

/// Config used to build an `OtlpSerializer`.
#[configurable_component]
#[derive(Debug, Clone, Default)]
pub struct OtlpSerializerConfig {
    // No configuration options needed - OTLP serialization is opinionated
}

impl OtlpSerializerConfig {
    /// Build the `OtlpSerializer` from this configuration.
    pub fn build(&self) -> Result<OtlpSerializer, crate::encoding::BuildError> {
        OtlpSerializer::new()
    }

    /// The data type of events that are accepted by `OtlpSerializer`.
    pub fn input_type(&self) -> DataType {
        DataType::Log | DataType::Metric | DataType::Trace
    }

    /// The schema required by the serializer.
    pub fn schema_requirement(&self) -> schema::Requirement {
        schema::Requirement::empty()
    }
}

/// Serializer that converts an `Event` to bytes using the OTLP (OpenTelemetry Protocol) protobuf format.
///
/// This serializer encodes events using the OTLP protobuf specification, which is the recommended
/// encoding format for OpenTelemetry data. The output is suitable for sending to OTLP-compatible
/// endpoints with `content-type: application/x-protobuf`.
///
/// # Implementation approach
///
/// This serializer converts Vector's internal event representation to the appropriate OTLP message type
/// based on the top-level field in the event:
/// - `resourceLogs` → `ExportLogsServiceRequest`
/// - `resourceMetrics` → `ExportMetricsServiceRequest`
/// - `resourceSpans` → `ExportTraceServiceRequest`
///
/// The implementation is the inverse of what the `opentelemetry` source does when decoding,
/// ensuring round-trip compatibility.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields will be used once encoding is implemented
pub struct OtlpSerializer {
    logs_descriptor: ProtobufSerializer,
    metrics_descriptor: ProtobufSerializer,
    traces_descriptor: ProtobufSerializer,
    options: Options,
}

impl OtlpSerializer {
    /// Creates a new OTLP serializer with the appropriate message descriptors.
    pub fn new() -> vector_common::Result<Self> {
        let options = Options {
            use_json_names: true,
        };

        let logs_descriptor = ProtobufSerializer::new_from_bytes(
            DESCRIPTOR_BYTES,
            LOGS_REQUEST_MESSAGE_TYPE,
            &options,
        )?;

        let metrics_descriptor = ProtobufSerializer::new_from_bytes(
            DESCRIPTOR_BYTES,
            METRICS_REQUEST_MESSAGE_TYPE,
            &options,
        )?;

        let traces_descriptor = ProtobufSerializer::new_from_bytes(
            DESCRIPTOR_BYTES,
            TRACES_REQUEST_MESSAGE_TYPE,
            &options,
        )?;

        Ok(Self {
            logs_descriptor,
            metrics_descriptor,
            traces_descriptor,
            options,
        })
    }
}

impl Encoder<Event> for OtlpSerializer {
    type Error = vector_common::Error;

    fn encode(&mut self, event: Event, buffer: &mut BytesMut) -> Result<(), Self::Error> {
        // Determine which descriptor to use based on top-level OTLP fields
        match &event {
            Event::Log(log) => {
                if log.contains(RESOURCE_LOGS_JSON_FIELD) {
                    self.logs_descriptor.encode(event, buffer)
                } else if log.contains(RESOURCE_METRICS_JSON_FIELD) {
                    // Currently the OTLP metrics are Vector logs (not metrics).
                    self.metrics_descriptor.encode(event, buffer)
                } else {
                    Err(format!(
                        "Log event does not contain OTLP top-level fields ({RESOURCE_LOGS_JSON_FIELD} or {RESOURCE_METRICS_JSON_FIELD})",
                    )
                        .into())
                }
            }
            Event::Trace(trace) => {
                if trace.contains(RESOURCE_SPANS_JSON_FIELD) {
                    self.traces_descriptor.encode(event, buffer)
                } else {
                    Err(format!(
                        "Trace event does not contain OTLP top-level field ({RESOURCE_SPANS_JSON_FIELD})",
                    )
                        .into())
                }
            }
            Event::Metric(metric) => {
                encode_metric_to_request(metric, buffer);
                Ok(())
            }
            Event::OtelLog(log_event) => {
                let request = otel_log_to_export_request(log_event);
                request.encode(buffer).map_err(|e| e.to_string())?;
                Ok(())
            }
            Event::OtelMetric(metric_event) => {
                let request = otel_metric_to_export_request(metric_event);
                request.encode(buffer).map_err(|e| e.to_string())?;
                Ok(())
            }
            Event::OtelSpan(span_event) => {
                let request = otel_span_to_export_request(span_event);
                request.encode(buffer).map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }
}

fn proto_convert<S: Message, D: Message + Default>(src: &S) -> D {
    D::decode(bytes::Bytes::from(src.encode_to_vec())).expect("proto roundtrip")
}

fn otel_log_to_export_request(log_event: &OtelLogEvent) -> ExportLogsServiceRequest {
    let record: ProtoLogRecord = proto_convert(log_event.record());
    let resource = log_event.resource().map(|r| proto_convert::<_, ProtoResource>(r));
    let scope = log_event.scope().map(|s| proto_convert::<_, ProtoScope>(s));

    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource,
            scope_logs: vec![ScopeLogs {
                scope,
                log_records: vec![record],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn otel_metric_to_export_request(metric_event: &OtelMetricEvent) -> ExportMetricsServiceRequest {
    let metric: ProtoMetric = proto_convert(metric_event.metric());
    let resource = metric_event.resource().map(|r| proto_convert::<_, ProtoResource>(r));
    let scope = metric_event.scope().map(|s| proto_convert::<_, ProtoScope>(s));

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource,
            scope_metrics: vec![ScopeMetrics {
                scope,
                metrics: vec![metric],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn otel_span_to_export_request(span_event: &OtelSpanEvent) -> ExportTraceServiceRequest {
    let span: ProtoSpan = proto_convert(span_event.span());
    let resource = span_event.resource().map(|r| proto_convert::<_, ProtoResource>(r));
    let scope = span_event.scope().map(|s| proto_convert::<_, ProtoScope>(s));

    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource,
            scope_spans: vec![ScopeSpans {
                scope,
                spans: vec![span],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use prost::Message;
    use tokio_util::codec::Encoder as _;
    use vector_core::event::{
        Event, EventMetadata, Metric, MetricKind, MetricValue, OtelLogEvent, OtelMetricEvent,
        OtelSpanEvent, metric::Bucket,
    };

    use super::OtlpSerializer;

    fn make_serializer() -> OtlpSerializer {
        OtlpSerializer::new().expect("OtlpSerializer::new must succeed")
    }

    #[test]
    fn encodes_counter_without_error() {
        let mut ser = make_serializer();
        let metric = Metric::new(
            "http_requests_total",
            MetricKind::Incremental,
            MetricValue::Counter { value: 100.0 },
        );
        let mut buf = BytesMut::new();
        ser.encode(Event::Metric(metric), &mut buf)
            .expect("counter encode must succeed");
        assert!(!buf.is_empty(), "encoded bytes must not be empty");
    }

    #[test]
    fn encodes_gauge_without_error() {
        let mut ser = make_serializer();
        let metric = Metric::new(
            "cpu_usage",
            MetricKind::Absolute,
            MetricValue::Gauge { value: 0.75 },
        );
        let mut buf = BytesMut::new();
        ser.encode(Event::Metric(metric), &mut buf)
            .expect("gauge encode must succeed");
        assert!(!buf.is_empty());
    }

    #[test]
    fn encodes_histogram_without_error() {
        let mut ser = make_serializer();
        let metric = Metric::new(
            "request_latency",
            MetricKind::Absolute,
            MetricValue::AggregatedHistogram {
                buckets: vec![
                    Bucket { upper_limit: 0.1, count: 10 },
                    Bucket { upper_limit: 1.0, count: 25 },
                    Bucket { upper_limit: f64::INFINITY, count: 5 },
                ],
                count: 40,
                sum: 12.5,
            },
        );
        let mut buf = BytesMut::new();
        ser.encode(Event::Metric(metric), &mut buf)
            .expect("histogram encode must succeed");
        assert!(!buf.is_empty());
    }

    #[test]
    fn encodes_otel_log_event() {
        use otel_proto_types::{
            common::v1::{AnyValue, KeyValue, any_value::Value as AnyValueKind},
            logs::v1::LogRecord,
            resource::v1::Resource,
        };

        let record = LogRecord {
            severity_text: "ERROR".into(),
            severity_number: 17,
            body: Some(AnyValue {
                value: Some(AnyValueKind::StringValue("something broke".into())),
            }),
            ..Default::default()
        };
        let resource = Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(AnyValue {
                    value: Some(AnyValueKind::StringValue("my-svc".into())),
                }),
            }],
            ..Default::default()
        };
        let event = Event::OtelLog(OtelLogEvent::from_parts(
            record,
            Some(resource),
            None,
            EventMetadata::default(),
        ));

        let mut ser = make_serializer();
        let mut buf = BytesMut::new();
        ser.encode(event, &mut buf)
            .expect("OtelLog encode must succeed");
        assert!(!buf.is_empty());

        let decoded = opentelemetry_proto::proto::collector::logs::v1::ExportLogsServiceRequest::decode(
            bytes::Bytes::from(buf.to_vec()),
        )
        .expect("must decode as ExportLogsServiceRequest");
        assert_eq!(decoded.resource_logs.len(), 1);
        let rl = &decoded.resource_logs[0];
        assert!(rl.resource.is_some());
        assert_eq!(rl.resource.as_ref().unwrap().attributes[0].key, "service.name");
        assert_eq!(rl.scope_logs.len(), 1);
        assert_eq!(rl.scope_logs[0].log_records.len(), 1);
        assert_eq!(rl.scope_logs[0].log_records[0].severity_text, "ERROR");
    }

    #[test]
    fn encodes_otel_metric_event() {
        use otel_proto_types::metrics::v1::{
            Gauge, NumberDataPoint, Metric as OtelMetric,
            metric::Data as OtelMetricData,
            number_data_point::Value as NdpValue,
        };

        let metric = OtelMetric {
            name: "cpu.usage".into(),
            description: "CPU usage".into(),
            unit: "1".into(),
            data: Some(OtelMetricData::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    value: Some(NdpValue::AsDouble(0.85)),
                    ..Default::default()
                }],
            })),
        };
        let event = Event::OtelMetric(OtelMetricEvent::from_parts(
            metric,
            None,
            None,
            EventMetadata::default(),
        ));

        let mut ser = make_serializer();
        let mut buf = BytesMut::new();
        ser.encode(event, &mut buf)
            .expect("OtelMetric encode must succeed");
        assert!(!buf.is_empty());

        let decoded =
            opentelemetry_proto::proto::collector::metrics::v1::ExportMetricsServiceRequest::decode(
                bytes::Bytes::from(buf.to_vec()),
            )
            .expect("must decode as ExportMetricsServiceRequest");
        assert_eq!(decoded.resource_metrics.len(), 1);
        let rm = &decoded.resource_metrics[0];
        assert_eq!(rm.scope_metrics.len(), 1);
        assert_eq!(rm.scope_metrics[0].metrics.len(), 1);
        assert_eq!(rm.scope_metrics[0].metrics[0].name, "cpu.usage");
    }

    #[test]
    fn encodes_otel_span_event() {
        use otel_proto_types::trace::v1::Span;

        let span = Span {
            name: "GET /api/users".into(),
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 2_000_000_000,
            ..Default::default()
        };
        let event = Event::OtelSpan(OtelSpanEvent::from_parts(
            span,
            None,
            None,
            EventMetadata::default(),
        ));

        let mut ser = make_serializer();
        let mut buf = BytesMut::new();
        ser.encode(event, &mut buf)
            .expect("OtelSpan encode must succeed");
        assert!(!buf.is_empty());

        let decoded =
            opentelemetry_proto::proto::collector::trace::v1::ExportTraceServiceRequest::decode(
                bytes::Bytes::from(buf.to_vec()),
            )
            .expect("must decode as ExportTraceServiceRequest");
        assert_eq!(decoded.resource_spans.len(), 1);
        let rs = &decoded.resource_spans[0];
        assert_eq!(rs.scope_spans.len(), 1);
        assert_eq!(rs.scope_spans[0].spans.len(), 1);
        assert_eq!(rs.scope_spans[0].spans[0].name, "GET /api/users");
    }
}
