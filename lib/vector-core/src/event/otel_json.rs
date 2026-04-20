//! OTLP-native JSON serialization helpers for OTel proto types.
//!
//! These produce the proto3 JSON mapping (camelCase field names, string-encoded
//! integers for nanosecond timestamps, etc.) matching the OTLP JSON spec.
//!
//! Used by `Serialize for OtelMetric` to produce OTLP-native JSON output.
//! Also provides `OtlpJsonLog` and `OtlpJsonSpan` wrappers for opt-in
//! OTLP JSON serialization of logs and spans (per-sink migration).

use opentelemetry_proto::tonic::common::v1::{
    AnyValue, KeyValue, any_value::Value as OtelValueKind,
};
use serde::Serialize;

use super::otel_event::{OtelLog, OtelSpan};

/// Wrapper for OTLP-native JSON serialization of OtelLog.
///
/// Use `OtlpJsonLog(&log)` when a sink wants OTLP JSON format instead
/// of the default legacy flat format from `Serialize for OtelLog`.
pub struct OtlpJsonLog<'a>(pub &'a OtelLog);

impl Serialize for OtlpJsonLog<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let log = self.0;
        let mut map = serializer.serialize_map(None)?;

        if let Some(ref body) = log.record().body {
            map.serialize_entry("body", &SerializableAnyValue(body))?;
        }
        if !log.record().severity_text.is_empty() {
            map.serialize_entry("severityText", &log.record().severity_text)?;
        }
        if log.record().severity_number != 0 {
            map.serialize_entry("severityNumber", &log.record().severity_number)?;
        }
        if log.record().time_unix_nano != 0 {
            map.serialize_entry("timeUnixNano", &log.record().time_unix_nano.to_string())?;
        }
        if log.record().observed_time_unix_nano != 0 {
            map.serialize_entry("observedTimeUnixNano", &log.record().observed_time_unix_nano.to_string())?;
        }
        if !log.record().trace_id.is_empty() {
            map.serialize_entry("traceId", &hex_encode_str(&log.record().trace_id))?;
        }
        if !log.record().span_id.is_empty() {
            map.serialize_entry("spanId", &hex_encode_str(&log.record().span_id))?;
        }
        if !log.record().attributes.is_empty() {
            map.serialize_entry("attributes", &SerializableAttributes(&log.record().attributes))?;
        }
        if let Some(ref res) = log.resource() {
            map.serialize_entry("resource", &SerializableResource(res))?;
        }
        if let Some(ref scope) = log.scope() {
            map.serialize_entry("scope", &SerializableScope(scope))?;
        }
        map.end()
    }
}

/// Wrapper for OTLP-native JSON serialization of OtelSpan.
pub struct OtlpJsonSpan<'a>(pub &'a OtelSpan);

impl Serialize for OtlpJsonSpan<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let span = self.0;
        let mut map = serializer.serialize_map(None)?;

        if !span.span().name.is_empty() {
            map.serialize_entry("name", &span.span().name)?;
        }
        if !span.span().trace_id.is_empty() {
            map.serialize_entry("traceId", &hex_encode_str(&span.span().trace_id))?;
        }
        if !span.span().span_id.is_empty() {
            map.serialize_entry("spanId", &hex_encode_str(&span.span().span_id))?;
        }
        if !span.span().parent_span_id.is_empty() {
            map.serialize_entry("parentSpanId", &hex_encode_str(&span.span().parent_span_id))?;
        }
        if span.span().kind != 0 {
            map.serialize_entry("kind", &span.span().kind)?;
        }
        if span.span().start_time_unix_nano != 0 {
            map.serialize_entry("startTimeUnixNano", &span.span().start_time_unix_nano.to_string())?;
        }
        if span.span().end_time_unix_nano != 0 {
            map.serialize_entry("endTimeUnixNano", &span.span().end_time_unix_nano.to_string())?;
        }
        if !span.span().attributes.is_empty() {
            map.serialize_entry("attributes", &SerializableAttributes(&span.span().attributes))?;
        }
        if let Some(ref status) = span.span().status {
            map.serialize_entry("status", &serde_json::json!({
                "code": status.code,
                "message": status.message,
            }))?;
        }
        if let Some(ref res) = span.resource() {
            map.serialize_entry("resource", &SerializableResource(res))?;
        }
        if let Some(ref scope) = span.scope() {
            map.serialize_entry("scope", &SerializableScope(scope))?;
        }
        map.end()
    }
}

fn hex_encode_str(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Serialize a slice of KeyValue as OTLP JSON attributes array.
pub(crate) struct SerializableAttributes<'a>(pub &'a [KeyValue]);

impl Serialize for SerializableAttributes<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for kv in self.0 {
            seq.serialize_element(&SerializableKeyValue(kv))?;
        }
        seq.end()
    }
}

struct SerializableKeyValue<'a>(&'a KeyValue);

impl Serialize for SerializableKeyValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("key", &self.0.key)?;
        if let Some(ref av) = self.0.value {
            map.serialize_entry("value", &SerializableAnyValue(av))?;
        }
        map.end()
    }
}

pub(crate) struct SerializableAnyValue<'a>(pub &'a AnyValue);

impl Serialize for SerializableAnyValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        if let Some(ref v) = self.0.value {
            match v {
                OtelValueKind::StringValue(s) => map.serialize_entry("stringValue", s)?,
                OtelValueKind::IntValue(i) => map.serialize_entry("intValue", &i.to_string())?,
                OtelValueKind::DoubleValue(d) => map.serialize_entry("doubleValue", d)?,
                OtelValueKind::BoolValue(b) => map.serialize_entry("boolValue", b)?,
                OtelValueKind::BytesValue(b) => {
                    // Hex-encode bytes (OTLP spec uses base64 but we avoid
                    // adding base64 as a non-dev dependency to vector-core)
                    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
                    map.serialize_entry("bytesValue", &hex)?;
                }
                OtelValueKind::ArrayValue(arr) => {
                    let values: Vec<SerializableAnyValue> =
                        arr.values.iter().map(SerializableAnyValue).collect();
                    map.serialize_entry("arrayValue", &ArrayWrapper { values: &values })?;
                }
                OtelValueKind::KvlistValue(kvl) => {
                    map.serialize_entry("kvlistValue", &KvListWrapper(&kvl.values))?;
                }
            }
        }
        map.end()
    }
}

#[derive(Serialize)]
struct ArrayWrapper<'a> {
    values: &'a [SerializableAnyValue<'a>],
}

struct KvListWrapper<'a>(&'a [KeyValue]);

impl Serialize for KvListWrapper<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("values", &SerializableAttributes(self.0))?;
        map.end()
    }
}

/// Serialize an OTel Resource as OTLP JSON.
pub(crate) struct SerializableResource<'a>(
    pub &'a opentelemetry_proto::tonic::resource::v1::Resource,
);

impl Serialize for SerializableResource<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("attributes", &SerializableAttributes(&self.0.attributes))?;
        map.end()
    }
}

/// Serialize an OTel InstrumentationScope as OTLP JSON.
pub(crate) struct SerializableScope<'a>(
    pub &'a opentelemetry_proto::tonic::common::v1::InstrumentationScope,
);

impl Serialize for SerializableScope<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut len = 0;
        if !self.0.name.is_empty() {
            len += 1;
        }
        if !self.0.version.is_empty() {
            len += 1;
        }
        let mut map = serializer.serialize_map(Some(len))?;
        if !self.0.name.is_empty() {
            map.serialize_entry("name", &self.0.name)?;
        }
        if !self.0.version.is_empty() {
            map.serialize_entry("version", &self.0.version)?;
        }
        map.end()
    }
}

/// Build an OTLP JSON data point object from number data point fields.
pub(crate) fn number_data_point_to_json(
    dp: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if !dp.attributes.is_empty() {
        m.insert(
            "attributes".into(),
            serde_json::to_value(&SerializableAttributes(&dp.attributes)).unwrap_or_default(),
        );
    }
    if dp.time_unix_nano != 0 {
        m.insert("timeUnixNano".into(), dp.time_unix_nano.to_string().into());
    }
    if dp.start_time_unix_nano != 0 {
        m.insert(
            "startTimeUnixNano".into(),
            dp.start_time_unix_nano.to_string().into(),
        );
    }
    if let Some(ref v) = dp.value {
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value;
        match v {
            Value::AsDouble(d) => {
                m.insert("asDouble".into(), (*d).into());
            }
            Value::AsInt(i) => {
                m.insert("asInt".into(), i.to_string().into());
            }
        }
    }
    serde_json::Value::Object(m)
}

/// Serialize OTel Sum as OTLP JSON.
pub(crate) struct SerializableSum<'a>(
    pub &'a opentelemetry_proto::tonic::metrics::v1::Sum,
);

impl Serialize for SerializableSum<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        let dps: Vec<serde_json::Value> =
            self.0.data_points.iter().map(number_data_point_to_json).collect();
        map.serialize_entry("dataPoints", &dps)?;
        map.serialize_entry("aggregationTemporality", &self.0.aggregation_temporality)?;
        map.serialize_entry("isMonotonic", &self.0.is_monotonic)?;
        map.end()
    }
}

/// Serialize OTel Gauge as OTLP JSON.
pub(crate) struct SerializableGauge<'a>(
    pub &'a opentelemetry_proto::tonic::metrics::v1::Gauge,
);

impl Serialize for SerializableGauge<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        let dps: Vec<serde_json::Value> =
            self.0.data_points.iter().map(number_data_point_to_json).collect();
        map.serialize_entry("dataPoints", &dps)?;
        map.end()
    }
}

/// Serialize OTel Histogram as OTLP JSON.
pub(crate) struct SerializableHistogram<'a>(
    pub &'a opentelemetry_proto::tonic::metrics::v1::Histogram,
);

impl Serialize for SerializableHistogram<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        let dps: Vec<serde_json::Value> = self.0.data_points.iter().map(|dp| {
            let mut m = serde_json::Map::new();
            if !dp.attributes.is_empty() {
                m.insert("attributes".into(),
                    serde_json::to_value(&SerializableAttributes(&dp.attributes)).unwrap_or_default());
            }
            if dp.time_unix_nano != 0 {
                m.insert("timeUnixNano".into(), dp.time_unix_nano.to_string().into());
            }
            m.insert("count".into(), dp.count.to_string().into());
            if let Some(sum) = dp.sum {
                m.insert("sum".into(), sum.into());
            }
            if !dp.bucket_counts.is_empty() {
                m.insert("bucketCounts".into(),
                    dp.bucket_counts.iter().map(|c| c.to_string()).collect::<Vec<_>>().into());
            }
            if !dp.explicit_bounds.is_empty() {
                m.insert("explicitBounds".into(), dp.explicit_bounds.clone().into());
            }
            serde_json::Value::Object(m)
        }).collect();
        map.serialize_entry("dataPoints", &dps)?;
        map.serialize_entry("aggregationTemporality", &self.0.aggregation_temporality)?;
        map.end()
    }
}

/// Serialize OTel Summary as OTLP JSON.
pub(crate) struct SerializableSummary<'a>(
    pub &'a opentelemetry_proto::tonic::metrics::v1::Summary,
);

impl Serialize for SerializableSummary<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        let dps: Vec<serde_json::Value> = self.0.data_points.iter().map(|dp| {
            let mut m = serde_json::Map::new();
            if !dp.attributes.is_empty() {
                m.insert("attributes".into(),
                    serde_json::to_value(&SerializableAttributes(&dp.attributes)).unwrap_or_default());
            }
            if dp.time_unix_nano != 0 {
                m.insert("timeUnixNano".into(), dp.time_unix_nano.to_string().into());
            }
            m.insert("count".into(), dp.count.to_string().into());
            m.insert("sum".into(), dp.sum.into());
            let qvs: Vec<serde_json::Value> = dp.quantile_values.iter().map(|q| {
                serde_json::json!({"quantile": q.quantile, "value": q.value})
            }).collect();
            m.insert("quantileValues".into(), qvs.into());
            serde_json::Value::Object(m)
        }).collect();
        map.serialize_entry("dataPoints", &dps)?;
        map.end()
    }
}

/// Serialize OTel ExponentialHistogram as OTLP JSON.
pub(crate) struct SerializableExpHistogram<'a>(
    pub &'a opentelemetry_proto::tonic::metrics::v1::ExponentialHistogram,
);

impl Serialize for SerializableExpHistogram<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        let dps: Vec<serde_json::Value> = self.0.data_points.iter().map(|dp| {
            let mut m = serde_json::Map::new();
            if !dp.attributes.is_empty() {
                m.insert("attributes".into(),
                    serde_json::to_value(&SerializableAttributes(&dp.attributes)).unwrap_or_default());
            }
            if dp.time_unix_nano != 0 {
                m.insert("timeUnixNano".into(), dp.time_unix_nano.to_string().into());
            }
            m.insert("count".into(), dp.count.to_string().into());
            if let Some(sum) = dp.sum { m.insert("sum".into(), sum.into()); }
            m.insert("scale".into(), dp.scale.into());
            m.insert("zeroCount".into(), dp.zero_count.to_string().into());
            if let Some(ref pos) = dp.positive {
                m.insert("positive".into(), serde_json::json!({
                    "offset": pos.offset,
                    "bucketCounts": pos.bucket_counts.iter().map(|c| c.to_string()).collect::<Vec<_>>()
                }));
            }
            if let Some(ref neg) = dp.negative {
                m.insert("negative".into(), serde_json::json!({
                    "offset": neg.offset,
                    "bucketCounts": neg.bucket_counts.iter().map(|c| c.to_string()).collect::<Vec<_>>()
                }));
            }
            serde_json::Value::Object(m)
        }).collect();
        map.serialize_entry("dataPoints", &dps)?;
        map.serialize_entry("aggregationTemporality", &self.0.aggregation_temporality)?;
        map.end()
    }
}

