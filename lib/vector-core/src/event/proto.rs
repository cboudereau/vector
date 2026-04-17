use std::{collections::BTreeMap, sync::Arc};

use chrono::TimeZone;
use ordered_float::NotNan;
use uuid::Uuid;

use super::{MetricTags, WithMetadata};
use crate::event;

#[allow(warnings, clippy::all, clippy::pedantic)]
mod proto_event {
    include!(concat!(env!("OUT_DIR"), "/event.rs"));
}
pub use event_wrapper::Event;
pub use metric::Value as MetricValue;
pub use proto_event::*;
use vrl::value::{ObjectMap, Value as VrlValue};

use super::EventFinalizers;
use super::metadata::{Inner, default_schema_definition};
use super::{EventMetadata, array};

impl event_array::Events {
    fn from_logs(logs: array::LogArray) -> Self {
        let logs = logs
            .into_iter()
            .map(|otel| WithMetadata::<Log>::from(otel).data)
            .collect();
        Self::Logs(LogArray { logs })
    }

    fn from_metrics(metrics: array::MetricArray) -> Self {
        let metrics = metrics
            .into_iter()
            .map(|otel| WithMetadata::<Metric>::from(otel).data)
            .collect();
        Self::Metrics(MetricArray { metrics })
    }

    fn from_traces(traces: array::TraceArray) -> Self {
        let traces = traces
            .into_iter()
            .map(|otel| WithMetadata::<Trace>::from(otel).data)
            .collect();
        Self::Traces(TraceArray { traces })
    }
}

impl From<array::EventArray> for EventArray {
    fn from(events: array::EventArray) -> Self {
        let events = Some(match events {
            array::EventArray::Logs(array) => event_array::Events::from_logs(array),
            array::EventArray::Metrics(array) => event_array::Events::from_metrics(array),
            array::EventArray::Traces(array) => event_array::Events::from_traces(array),
        });
        Self { events }
    }
}

impl From<EventArray> for array::EventArray {
    fn from(events: EventArray) -> Self {
        let events = events.events.unwrap();

        match events {
            event_array::Events::Logs(logs) => array::EventArray::Logs(
                logs.logs.into_iter().map(super::OtelLog::from).collect(),
            ),
            event_array::Events::Metrics(metrics) => array::EventArray::Metrics(
                metrics
                    .metrics
                    .into_iter()
                    .map(|proto| super::OtelMetric::from_legacy_metric(proto.into()))
                    .collect(),
            ),
            event_array::Events::Traces(traces) => array::EventArray::Traces(
                traces.traces.into_iter().map(super::OtelSpan::from).collect(),
            ),
        }
    }
}

impl From<Event> for EventWrapper {
    fn from(event: Event) -> Self {
        Self { event: Some(event) }
    }
}

impl From<Log> for Event {
    fn from(log: Log) -> Self {
        Self::Log(log)
    }
}

impl From<Metric> for Event {
    fn from(metric: Metric) -> Self {
        Self::Metric(metric)
    }
}

impl From<Trace> for Event {
    fn from(trace: Trace) -> Self {
        Self::Trace(trace)
    }
}

impl From<Log> for super::OtelLog {
    fn from(log: Log) -> Self {
        let metadata = log.metadata_full.map(Into::into).unwrap_or_default();
        let value = log
            .value
            .and_then(decode_value)
            .unwrap_or(VrlValue::Null);
        super::OtelLog::from_value_map(value, metadata)
    }
}

impl From<Trace> for super::OtelSpan {
    fn from(trace: Trace) -> Self {
        let metadata = trace.metadata_full.map(Into::into).unwrap_or_default();
        let fields = trace
            .fields
            .into_iter()
            .filter_map(|(k, v)| decode_value(v).map(|value| (k.into(), value)))
            .collect::<ObjectMap>();
        super::OtelSpan::from_value_map(VrlValue::Object(fields), metadata)
    }
}

impl From<MetricValue> for super::MetricValue {
    fn from(value: MetricValue) -> Self {
        match value {
            MetricValue::Counter(counter) => Self::Counter {
                value: counter.value,
            },
            MetricValue::Gauge(gauge) => Self::Gauge { value: gauge.value },
            MetricValue::Set(set) => Self::Set {
                values: set.values.into_iter().collect(),
            },
            MetricValue::Distribution2(dist) => Self::Distribution {
                statistic: dist.statistic().into(),
                samples: dist.samples.into_iter().map(Into::into).collect(),
            },
            MetricValue::AggregatedHistogram3(hist) => Self::AggregatedHistogram {
                buckets: hist.buckets.into_iter().map(Into::into).collect(),
                count: hist.count,
                sum: hist.sum,
            },
            MetricValue::AggregatedSummary3(summary) => Self::AggregatedSummary {
                quantiles: summary.quantiles.into_iter().map(Into::into).collect(),
                count: summary.count,
                sum: summary.sum,
            },
            // Old proto variants are no longer supported.
            // Buffers must be drained before upgrading to this version.
            _ => Self::Gauge { value: 0.0 },
        }
    }
}

impl From<Metric> for super::Metric {
    fn from(metric: Metric) -> Self {
        let kind = match metric.kind() {
            metric::Kind::Incremental => super::MetricKind::Incremental,
            metric::Kind::Absolute => super::MetricKind::Absolute,
        };

        let name = metric.name;

        let namespace = (!metric.namespace.is_empty()).then_some(metric.namespace);

        let timestamp = metric.timestamp.map(|ts| {
            chrono::Utc
                .timestamp_opt(ts.seconds, ts.nanos as u32)
                .single()
                .expect("invalid timestamp")
        });

        let tags = MetricTags(
            metric
                .tags_v2
                .into_iter()
                .map(|(tag, values)| {
                    (
                        tag,
                        values
                            .values
                            .into_iter()
                            .map(|value| super::metric::TagValue::from(value.value))
                            .collect(),
                    )
                })
                .collect(),
        );
        let tags = (!tags.is_empty()).then_some(tags);

        let value = super::MetricValue::from(metric.value.unwrap());

        let metadata = metric.metadata_full.map(Into::into).unwrap_or_default();

        Self::new_with_metadata(name, kind, value, metadata)
            .with_namespace(namespace)
            .with_tags(tags)
            .with_timestamp(timestamp)
            .with_interval_ms(std::num::NonZeroU32::new(metric.interval_ms))
    }
}

impl From<EventWrapper> for super::Event {
    fn from(proto: EventWrapper) -> Self {
        match proto.event.unwrap() {
            Event::Log(proto) => super::Event::Log(proto.into()),
            Event::Metric(proto) => {
                super::Event::Metric(super::OtelMetric::from_legacy_metric(proto.into()))
            }
            Event::Trace(proto) => super::Event::Trace(proto.into()),
        }
    }
}


/// Encode a Value + EventMetadata into a proto Log.
fn encode_log_proto(value: VrlValue, metadata: super::EventMetadata) -> WithMetadata<Log> {
    #[allow(deprecated)]
    let data = Log {
        fields: BTreeMap::new(),
        value: Some(encode_value(value)),
        metadata: None,
        metadata_full: Some(metadata.clone().into()),
    };

    WithMetadata { data, metadata }
}


impl From<super::OtelLog> for WithMetadata<Log> {
    fn from(otel_log: super::OtelLog) -> Self {
        let value = otel_log.to_value_legacy_layout();
        let (_, _, _, metadata) = otel_log.into_parts();
        encode_log_proto(value, metadata)
    }
}

/// Encode an ObjectMap + EventMetadata into a proto Trace.
fn encode_trace_proto(fields: ObjectMap, metadata: super::EventMetadata) -> WithMetadata<Trace> {
    let fields = fields
        .into_iter()
        .map(|(k, v)| (k.into(), encode_value(v)))
        .collect::<BTreeMap<_, _>>();

    #[allow(deprecated)]
    let data = Trace {
        fields,
        metadata: None,
        metadata_full: Some(metadata.clone().into()),
    };

    WithMetadata { data, metadata }
}

/// Encode MetricSeries + MetricData + EventMetadata into a proto Metric with metadata.
fn encode_metric_proto(
    series: super::metric::MetricSeries,
    data: super::metric::MetricData,
    metadata: super::EventMetadata,
) -> WithMetadata<Metric> {
    let name = series.name.name;
    let namespace = series.name.namespace.unwrap_or_default();

    let timestamp = data.time.timestamp.map(|ts| prost_types::Timestamp {
        seconds: ts.timestamp(),
        nanos: ts.timestamp_subsec_nanos() as i32,
    });

    let interval_ms = data.time.interval_ms.map_or(0, std::num::NonZeroU32::get);

    let tags = series.tags.unwrap_or_default();

    let kind = match data.kind {
        super::MetricKind::Incremental => metric::Kind::Incremental,
        super::MetricKind::Absolute => metric::Kind::Absolute,
    }
    .into();

    let metric = MetricValue::from(data.value);

    let tags_v2 = tags
        .0
        .into_iter()
        .map(|(tag, values)| {
            let values = values
                .into_iter()
                .map(|value| TagValue {
                    value: value.into_option(),
                })
                .collect();
            (tag, TagValues { values })
        })
        .collect();

    #[allow(deprecated)]
    let data = Metric {
        name,
        namespace,
        timestamp,
        tags_v1: BTreeMap::new(),
        tags_v2,
        kind,
        interval_ms,
        value: Some(metric),
        metadata: None,
        metadata_full: Some(metadata.clone().into()),
    };

    WithMetadata { data, metadata }
}

impl From<super::OtelSpan> for WithMetadata<Trace> {
    fn from(otel_span: super::OtelSpan) -> Self {
        let value = otel_span.to_value_legacy_layout();
        let (_, _, _, metadata) = otel_span.into_parts();
        let fields = match value {
            VrlValue::Object(fields) => fields,
            _ => ObjectMap::new(),
        };
        encode_trace_proto(fields, metadata)
    }
}

impl From<super::OtelMetric> for WithMetadata<Metric> {
    fn from(otel_metric: super::OtelMetric) -> Self {
        let (series, data, metadata) = otel_metric.into_metric_parts();
        encode_metric_proto(series, data, metadata)
    }
}

impl From<super::Metric> for Metric {
    fn from(metric: super::Metric) -> Self {
        WithMetadata::<Self>::from(metric).data
    }
}

impl From<super::MetricValue> for MetricValue {
    fn from(value: super::MetricValue) -> Self {
        match value {
            super::MetricValue::Counter { value } => Self::Counter(Counter { value }),
            super::MetricValue::Gauge { value } => Self::Gauge(Gauge { value }),
            super::MetricValue::Set { values } => Self::Set(Set {
                values: values.into_iter().collect(),
            }),
            super::MetricValue::Distribution { samples, statistic } => {
                Self::Distribution2(Distribution2 {
                    samples: samples.into_iter().map(Into::into).collect(),
                    statistic: match statistic {
                        super::StatisticKind::Histogram => StatisticKind::Histogram,
                        super::StatisticKind::Summary => StatisticKind::Summary,
                    }
                    .into(),
                })
            }
            super::MetricValue::AggregatedHistogram {
                buckets,
                count,
                sum,
            } => Self::AggregatedHistogram3(AggregatedHistogram3 {
                buckets: buckets.into_iter().map(Into::into).collect(),
                count,
                sum,
            }),
            super::MetricValue::AggregatedSummary {
                quantiles,
                count,
                sum,
            } => Self::AggregatedSummary3(AggregatedSummary3 {
                quantiles: quantiles.into_iter().map(Into::into).collect(),
                count,
                sum,
            }),
        }
    }
}

impl From<super::Metric> for WithMetadata<Metric> {
    fn from(metric: super::Metric) -> Self {
        let (series, data, metadata) = metric.into_parts();
        encode_metric_proto(series, data, metadata)
    }
}

impl From<super::Event> for Event {
    fn from(event: super::Event) -> Self {
        WithMetadata::<Self>::from(event).data
    }
}

impl From<super::Event> for WithMetadata<Event> {
    fn from(event: super::Event) -> Self {
        match event {
            super::Event::Log(otel_log) => {
                WithMetadata::<Log>::from(otel_log).into()
            }
            super::Event::Metric(otel_metric) => {
                WithMetadata::<Metric>::from(otel_metric).into()
            }
            super::Event::Trace(otel_span) => {
                WithMetadata::<Trace>::from(otel_span).into()
            }
        }
    }
}

impl From<super::Event> for EventWrapper {
    fn from(event: super::Event) -> Self {
        WithMetadata::<EventWrapper>::from(event).data
    }
}

impl From<super::Event> for WithMetadata<EventWrapper> {
    fn from(event: super::Event) -> Self {
        WithMetadata::<Event>::from(event).into()
    }
}


impl From<super::metadata::Secrets> for Secrets {
    fn from(value: super::metadata::Secrets) -> Self {
        Self {
            entries: value.into_iter().map(|(k, v)| (k, v.to_string())).collect(),
        }
    }
}

impl From<Secrets> for super::metadata::Secrets {
    fn from(value: Secrets) -> Self {
        let mut secrets = Self::new();
        for (k, v) in value.entries {
            secrets.insert(k, v);
        }

        secrets
    }
}


impl From<crate::config::OutputId> for OutputId {
    fn from(value: crate::config::OutputId) -> Self {
        Self {
            component: value.component.into_id(),
            port: value.port,
        }
    }
}

impl From<OutputId> for crate::config::OutputId {
    fn from(value: OutputId) -> Self {
        Self::from((value.component, value.port))
    }
}

impl From<EventMetadata> for Metadata {
    fn from(value: EventMetadata) -> Self {
        let super::metadata::Inner {
            value,
            secrets,
            source_id,
            source_type,
            upstream_id,
            source_event_id,
            ..
        } = value.into_owned();

        let secrets = (!secrets.is_empty()).then(|| secrets.into());

        Self {
            value: Some(encode_value(value)),
            datadog_origin_metadata: None,
            source_id: source_id.map(|s| s.to_string()),
            source_type: source_type.map(|s| s.to_string()),
            upstream_id: upstream_id.map(|id| id.as_ref().clone()).map(Into::into),
            secrets,
            source_event_id: source_event_id.map_or(vec![], std::convert::Into::into),
        }
    }
}

impl From<Metadata> for EventMetadata {
    fn from(value: Metadata) -> Self {
        let Metadata {
            value: metadata_value,
            source_id,
            source_type,
            upstream_id,
            secrets,
            source_event_id,
            ..
        } = value;

        let metadata_value = metadata_value.and_then(decode_value);
        let source_id = source_id.map(|s| Arc::new(s.into()));
        let upstream_id = upstream_id.map(|id| Arc::new(id.into()));
        let secrets = secrets.map(Into::into);
        let source_event_id = if source_event_id.is_empty() {
            None
        } else {
            match Uuid::from_slice(&source_event_id) {
                Ok(id) => Some(id),
                Err(error) => {
                    error!(
                        %error,
                        source_event_id = %String::from_utf8_lossy(&source_event_id),
                        "Failed to parse source_event_id.",
                    );
                    None
                }
            }
        };

        EventMetadata {
            inner: Arc::new(Inner {
                value: metadata_value
                    .unwrap_or_else(|| vrl::value::Value::Object(ObjectMap::new())),
                secrets: secrets.unwrap_or_default(),
                finalizers: EventFinalizers::default(),
                source_id,
                source_type: source_type.map(Into::into),
                upstream_id,
                schema_definition: default_schema_definition(),
                dropped_fields: ObjectMap::new(),
                source_event_id,
            }),
            last_transform_timestamp: None,
        }
    }
}

fn decode_value(input: Value) -> Option<super::Value> {
    match input.kind {
        Some(value::Kind::RawBytes(data)) => Some(super::Value::Bytes(data)),
        Some(value::Kind::Timestamp(ts)) => Some(super::Value::Timestamp(
            chrono::Utc
                .timestamp_opt(ts.seconds, ts.nanos as u32)
                .single()
                .expect("invalid timestamp"),
        )),
        Some(value::Kind::Integer(value)) => Some(super::Value::Integer(value)),
        Some(value::Kind::Float(value)) => Some(super::Value::Float(NotNan::new(value).unwrap())),
        Some(value::Kind::Boolean(value)) => Some(super::Value::Boolean(value)),
        Some(value::Kind::Map(map)) => decode_map(map.fields),
        Some(value::Kind::Array(array)) => decode_array(array.items),
        Some(value::Kind::Null(_)) => Some(super::Value::Null),
        None => {
            error!("Encoded event contains unknown value kind.");
            None
        }
    }
}

fn decode_map(fields: BTreeMap<String, Value>) -> Option<super::Value> {
    fields
        .into_iter()
        .map(|(key, value)| decode_value(value).map(|value| (key.into(), value)))
        .collect::<Option<ObjectMap>>()
        .map(event::Value::Object)
}

fn decode_array(items: Vec<Value>) -> Option<super::Value> {
    items
        .into_iter()
        .map(decode_value)
        .collect::<Option<Vec<_>>>()
        .map(super::Value::Array)
}

fn encode_value(value: super::Value) -> Value {
    Value {
        kind: match value {
            super::Value::Bytes(b) => Some(value::Kind::RawBytes(b)),
            super::Value::Regex(regex) => Some(value::Kind::RawBytes(regex.as_bytes())),
            super::Value::Timestamp(ts) => Some(value::Kind::Timestamp(prost_types::Timestamp {
                seconds: ts.timestamp(),
                nanos: ts.timestamp_subsec_nanos() as i32,
            })),
            super::Value::Integer(value) => Some(value::Kind::Integer(value)),
            super::Value::Float(value) => Some(value::Kind::Float(value.into_inner())),
            super::Value::Boolean(value) => Some(value::Kind::Boolean(value)),
            super::Value::Object(fields) => Some(value::Kind::Map(encode_map(fields))),
            super::Value::Array(items) => Some(value::Kind::Array(encode_array(items))),
            super::Value::Null => Some(value::Kind::Null(ValueNull::NullValue as i32)),
        },
    }
}

fn encode_map(fields: ObjectMap) -> ValueMap {
    ValueMap {
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.into(), encode_value(value)))
            .collect(),
    }
}

fn encode_array(items: Vec<super::Value>) -> ValueArray {
    ValueArray {
        items: items.into_iter().map(encode_value).collect(),
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;
    use super::*;
    use crate::event::{Metric, MetricKind, MetricValue, OtelLog, OtelMetric, OtelSpan};

    /// Verify that OtelLog encodes to proto and round-trips correctly.
    #[test]
    fn otel_log_proto_round_trip() {
        let mut otel = OtelLog::from("hello world");
        otel.insert(vrl::event_path!("key"), "value");
        otel.insert(vrl::event_path!("num"), 42);

        let encoded = WithMetadata::<Log>::from(otel);
        let bytes = encoded.data.encode_to_vec();
        let decoded_log = Log::decode(bytes.as_slice()).expect("proto must decode");
        let round_tripped = OtelLog::from(decoded_log);

        // Verify key fields survive the round-trip
        assert_eq!(
            round_tripped.get(vrl::event_path!("key")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("value".to_string()),
        );
    }

    /// Verify that From<OtelMetric> and From<Metric> produce identical proto
    /// bytes for the same logical metric.
    #[test]
    fn otel_metric_proto_matches_legacy_metric_proto() {
        let metric = Metric::new(
            "test_counter",
            MetricKind::Incremental,
            MetricValue::Counter { value: 42.0 },
        )
        .with_namespace(Some("ns"))
        .with_tags(Some(crate::metric_tags!("env" => "prod")));

        // Path A: Metric -> proto
        let via_legacy = WithMetadata::<super::Metric>::from(metric.clone());

        // Path B: Metric -> OtelMetric -> proto
        let otel = OtelMetric::from_legacy_metric(metric);
        let via_otel = WithMetadata::<super::Metric>::from(otel);

        assert_eq!(
            via_legacy.data.encode_to_vec(),
            via_otel.data.encode_to_vec(),
            "OtelMetric proto encoding must match legacy Metric proto encoding"
        );
    }

    /// Verify that From<OtelSpan> produces valid proto that round-trips
    /// through decode.
    #[test]
    fn otel_span_proto_encodes_fields_correctly() {
        let mut otel_log = OtelLog::from("span data");
        otel_log.insert(vrl::event_path!("trace_id"), "abc123");

        let otel_span = OtelSpan::from_otel_log(otel_log);
        let encoded = WithMetadata::<Trace>::from(otel_span);

        // Verify fields are present and non-empty
        assert!(!encoded.data.fields.is_empty(), "trace fields must not be empty");
        assert!(encoded.data.metadata_full.is_some(), "must have metadata");

        // Verify proto round-trips through encode/decode
        let bytes = encoded.data.encode_to_vec();
        let decoded = Trace::decode(bytes.as_slice()).expect("proto must decode");
        assert_eq!(decoded.fields.len(), encoded.data.fields.len());
    }
}
