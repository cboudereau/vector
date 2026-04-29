use std::marker::PhantomData;

use lookup::{OwnedTargetPath, OwnedValuePath, PathPrefix};
use opentelemetry_proto::tonic::common::v1::{
    AnyValue as OtelAnyValue, KeyValue as OtelKeyValue,
    InstrumentationScope as OtelScope,
};
#[cfg(test)]
use opentelemetry_proto::tonic::common::v1::any_value::Value as OtelValueKind;
use opentelemetry_proto::tonic::resource::v1::Resource as OtelResource;
use snafu::Snafu;
use vrl::{
    compiler::{ProgramInfo, SecretTarget, Target, value::VrlValueConvert},
    value::{KeyString, ObjectMap, Value},
};

use super::{Event, EventMetadata, OtelLog, OtelMetric, OtelSpan};
use super::otel_fields as f;
use crate::schema::Definition;
#[cfg(test)]
use lookup::owned_value_path;
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use vrl::{prelude::Collection, value::Kind};

const VALID_OTEL_METRIC_PATHS_SET: &str = ".name, .description, .unit, .resource, .scope, .attributes, .kind, .namespace, .data.*.data_points[*].attributes";
const VALID_OTEL_METRIC_PATHS_GET: &str =
    ".name, .description, .unit, .resource, .scope, .data, .attributes";
const MAX_OTEL_METRIC_PATH_DEPTH: usize = 4;

// ---------------------------------------------------------------------------
// OTel AnyValue <-> VRL Value conversion
// ---------------------------------------------------------------------------

fn otel_any_value_to_vrl(av: &OtelAnyValue) -> Value {
    super::otel_event::any_value_to_vrl(av)
}

fn vrl_value_to_otel_any_value(val: &Value) -> OtelAnyValue {
    super::vrl_value_to_any_value(val)
}

fn otel_kvlist_to_object_map(kvs: &[OtelKeyValue]) -> ObjectMap {
    kvs.iter()
        .map(|kv| {
            let v = kv
                .value
                .as_ref()
                .map(otel_any_value_to_vrl)
                .unwrap_or(Value::Null);
            (kv.key.clone().into(), v)
        })
        .collect()
}

fn object_map_to_otel_kvlist(map: &ObjectMap) -> Vec<OtelKeyValue> {
    map.iter()
        .map(|(k, v)| OtelKeyValue {
            key: k.to_string(),
            value: Some(vrl_value_to_otel_any_value(v)),
        })
        .collect()
}

fn otel_resource_to_value(resource: &OtelResource) -> Value {
    let mut map = ObjectMap::new();
    map.insert(
        f::ATTRIBUTES.into(),
        Value::Object(otel_kvlist_to_object_map(&resource.attributes)),
    );
    map.insert(
        f::DROPPED_ATTRIBUTES_COUNT.into(),
        Value::Integer(resource.dropped_attributes_count as i64),
    );
    Value::Object(map)
}

fn value_to_otel_resource(val: &Value) -> Option<OtelResource> {
    let map = val.as_object()?;
    let attributes = map
        .get(f::ATTRIBUTES)
        .and_then(|v| v.as_object())
        .map(object_map_to_otel_kvlist)
        .unwrap_or_default();
    let dropped_attributes_count = map
        .get(f::DROPPED_ATTRIBUTES_COUNT)
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;
    Some(OtelResource {
        attributes,
        dropped_attributes_count,
    })
}

fn otel_scope_to_value(scope: &OtelScope) -> Value {
    let mut map = ObjectMap::new();
    map.insert(f::NAME.into(), Value::Bytes(scope.name.clone().into()));
    map.insert(f::VERSION.into(), Value::Bytes(scope.version.clone().into()));
    map.insert(
        f::ATTRIBUTES.into(),
        Value::Object(otel_kvlist_to_object_map(&scope.attributes)),
    );
    map.insert(
        f::DROPPED_ATTRIBUTES_COUNT.into(),
        Value::Integer(scope.dropped_attributes_count as i64),
    );
    Value::Object(map)
}

fn value_to_otel_scope(val: &Value) -> Option<OtelScope> {
    let map = val.as_object()?;
    let name = map
        .get(f::NAME)
        .and_then(|v| v.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let version = map
        .get(f::VERSION)
        .and_then(|v| v.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let attributes = map
        .get(f::ATTRIBUTES)
        .and_then(|v| v.as_object())
        .map(object_map_to_otel_kvlist)
        .unwrap_or_default();
    let dropped_attributes_count = map
        .get(f::DROPPED_ATTRIBUTES_COUNT)
        .and_then(|v| v.as_integer())
        .unwrap_or(0) as u32;
    Some(OtelScope {
        name,
        version,
        attributes,
        dropped_attributes_count,
    })
}

fn insert_at_segments(val: &mut Value, segments: &[&str], new_value: Value) {
    let mut current = val;
    for (i, seg) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            if let Some(map) = current.as_object_mut() {
                map.insert((*seg).into(), new_value);
            }
            return;
        }
        if !current.is_object() {
            *current = Value::Object(ObjectMap::new());
        }
        let map = current.as_object_mut().expect("just ensured object");
        if !map.contains_key(*seg) {
            map.insert((*seg).into(), Value::Object(ObjectMap::new()));
        }
        current = map.get_mut(*seg).expect("just inserted");
    }
}

use super::otel_event::hex_encode as hex_encode_bytes;

fn hex_decode_value(val: &Value) -> Vec<u8> {
    let Some(b) = val.as_bytes() else {
        return Vec::new();
    };
    let s = String::from_utf8_lossy(b);
    if s.len() % 2 != 0 {
        return b.to_vec();
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut chars = s.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        match (hi.to_digit(16), lo.to_digit(16)) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return b.to_vec(),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// OtelLog -> Value projection
// ---------------------------------------------------------------------------

/// OTel-native LogRecord → VRL Value projection.
///
/// Proto fields become top-level keys. LogRecord attributes are **flattened**
/// into the top-level map so that VRL programs can use `.key` directly
/// (matching the OTel Collector's `transform` processor behavior where
/// `attributes["key"]` is the standard access pattern, but Vector's VRL
/// convention is flat top-level access).
///
/// The `.body` key holds the log body (no `.message` alias).
/// `.resource` and `.scope` are nested objects.
fn otel_log_event_to_value(event: &OtelLog) -> Value {
    let record = &event.record;
    let mut map = ObjectMap::new();

    if let Some(body) = &record.body {
        map.insert(f::BODY.into(), otel_any_value_to_vrl(body));
    }

    if record.severity_number != 0 {
        map.insert(f::SEVERITY_NUMBER.into(), Value::Integer(i64::from(record.severity_number)));
    }
    if !record.severity_text.is_empty() {
        map.insert(f::SEVERITY_TEXT.into(), Value::Bytes(record.severity_text.clone().into()));
    }
    if record.time_unix_nano != 0 {
        map.insert(f::TIME_UNIX_NANO.into(), Value::Integer(record.time_unix_nano as i64));
    }
    if record.observed_time_unix_nano != 0 {
        map.insert(f::OBSERVED_TIME_UNIX_NANO.into(), Value::Integer(record.observed_time_unix_nano as i64));
    }
    if !record.trace_id.is_empty() {
        map.insert(f::LOG_TRACE_ID.into(), hex_encode_bytes(&record.trace_id));
    }
    if !record.span_id.is_empty() {
        map.insert(f::LOG_SPAN_ID.into(), hex_encode_bytes(&record.span_id));
    }
    if record.flags != 0 {
        map.insert(f::LOG_FLAGS.into(), Value::Integer(i64::from(record.flags)));
    }

    // Flatten LogRecord attributes into top-level (VRL convention).
    // This means .attributes."key" AND ."key" both work.
    for (key, val) in event.record_attrs.iter() {
        map.insert(key.clone().into(), otel_any_value_to_vrl(val));
    }
    // Also keep .attributes as nested object for explicit access
    if !event.record_attrs.is_empty() {
        let attrs_map: ObjectMap = event.record_attrs.iter()
            .map(|(k, v)| (KeyString::from(k.clone()), otel_any_value_to_vrl(v)))
            .collect();
        map.insert(
            f::ATTRIBUTES.into(),
            Value::Object(attrs_map),
        );
    }

    if let Some(resource) = event.resource_proto() {
        map.insert(f::RESOURCE.into(), otel_resource_to_value(&resource));
    }
    if let Some(scope) = event.scope_proto() {
        map.insert(f::SCOPE.into(), otel_scope_to_value(&scope));
    }

    Value::Object(map)
}

/// VRL Value → OtelLog reconstruction.
///
/// Known OTel fields (.body, .severity_text, etc.) are extracted into proto
/// fields. All other top-level keys become LogRecord attributes.
fn value_to_otel_log_event(value: Value, metadata: EventMetadata) -> OtelLog {
    use opentelemetry_proto::tonic::logs::v1::LogRecord;

    let mut map = match value {
        Value::Object(m) => m,
        other => {
            let mut m = ObjectMap::new();
            m.insert(f::BODY.into(), other);
            m
        }
    };

    let body = map.remove(f::BODY)
        .map(|v| vrl_value_to_otel_any_value(&v));

    let severity_number = map.remove(f::SEVERITY_NUMBER)
        .and_then(|v| v.as_integer()).unwrap_or(0) as i32;
    let severity_text = map.remove(f::SEVERITY_TEXT)
        .and_then(|v| v.as_bytes().map(|b| String::from_utf8_lossy(&b).into_owned()))
        .unwrap_or_default();
    let time_unix_nano = map.remove(f::TIME_UNIX_NANO)
        .and_then(|v| v.as_integer()).unwrap_or(0) as u64;
    let observed_time_unix_nano = map.remove(f::OBSERVED_TIME_UNIX_NANO)
        .and_then(|v| v.as_integer()).unwrap_or(0) as u64;
    let trace_id_val = map.remove(f::LOG_TRACE_ID);
    let trace_id = trace_id_val.as_ref().map(hex_decode_value).unwrap_or_default();
    let span_id_val = map.remove(f::LOG_SPAN_ID);
    let span_id = span_id_val.as_ref().map(hex_decode_value).unwrap_or_default();
    let flags = map.remove(f::LOG_FLAGS)
        .and_then(|v| v.as_integer()).unwrap_or(0) as u32;
    let dropped_attributes_count = map.remove(f::DROPPED_ATTRIBUTES_COUNT)
        .and_then(|v| v.as_integer()).unwrap_or(0) as u32;

    map.remove(f::ATTRIBUTES);

    let resource_val = map.remove(f::RESOURCE);
    let resource = resource_val.as_ref().and_then(|v| value_to_otel_resource(v));
    let scope_val = map.remove(f::SCOPE);
    let scope = scope_val.as_ref().and_then(|v| value_to_otel_scope(v));

    // Everything remaining in map becomes LogRecord attributes.
    let attributes: Vec<OtelKeyValue> = map.into_iter()
        .map(|(k, v)| OtelKeyValue {
            key: k.to_string(),
            value: Some(vrl_value_to_otel_any_value(&v)),
        })
        .collect();

    let record = LogRecord {
        body,
        severity_number,
        severity_text,
        time_unix_nano,
        observed_time_unix_nano,
        trace_id,
        span_id,
        flags,
        dropped_attributes_count,
        attributes,
    };

    OtelLog::from_parts(record, resource, scope, metadata)
}

// ---------------------------------------------------------------------------
// OtelSpan -> Value projection
// ---------------------------------------------------------------------------

fn otel_span_event_to_value(event: &OtelSpan) -> Value {
    let span_proto = event.span();
    let mut map = ObjectMap::new();
    map.insert(f::SPAN_TRACE_ID.into(), hex_encode_bytes(&span_proto.trace_id));
    map.insert(f::SPAN_SPAN_ID.into(), hex_encode_bytes(&span_proto.span_id));
    map.insert("trace_state".into(), Value::Bytes(span_proto.trace_state.clone().into()));
    map.insert(f::PARENT_SPAN_ID.into(), hex_encode_bytes(&span_proto.parent_span_id));
    map.insert(f::NAME.into(), Value::Bytes(span_proto.name.clone().into()));
    map.insert(f::SPAN_KIND.into(), Value::Integer(span_proto.kind as i64));
    map.insert(f::START_TIME_UNIX_NANO.into(), Value::Integer(span_proto.start_time_unix_nano as i64));
    map.insert(f::END_TIME_UNIX_NANO.into(), Value::Integer(span_proto.end_time_unix_nano as i64));
    map.insert(
        f::ATTRIBUTES.into(),
        Value::Object(event.attributes().to_object_map()),
    );
    map.insert(f::DROPPED_ATTRIBUTES_COUNT.into(), Value::Integer(span_proto.dropped_attributes_count as i64));

    let events_arr: Vec<Value> = span_proto.events.iter().map(|e| {
        let mut em = ObjectMap::new();
        em.insert(f::TIME_UNIX_NANO.into(), Value::Integer(e.time_unix_nano as i64));
        em.insert(f::NAME.into(), Value::Bytes(e.name.clone().into()));
        em.insert(f::ATTRIBUTES.into(), Value::Object(otel_kvlist_to_object_map(&e.attributes)));
        em.insert(f::DROPPED_ATTRIBUTES_COUNT.into(), Value::Integer(e.dropped_attributes_count as i64));
        Value::Object(em)
    }).collect();
    map.insert(f::SPAN_EVENTS.into(), Value::Array(events_arr));
    map.insert("dropped_events_count".into(), Value::Integer(span_proto.dropped_events_count as i64));

    let links_arr: Vec<Value> = span_proto.links.iter().map(|l| {
        let mut lm = ObjectMap::new();
        lm.insert(f::SPAN_TRACE_ID.into(), hex_encode_bytes(&l.trace_id));
        lm.insert(f::SPAN_SPAN_ID.into(), hex_encode_bytes(&l.span_id));
        lm.insert("trace_state".into(), Value::Bytes(l.trace_state.clone().into()));
        lm.insert(f::ATTRIBUTES.into(), Value::Object(otel_kvlist_to_object_map(&l.attributes)));
        lm.insert(f::DROPPED_ATTRIBUTES_COUNT.into(), Value::Integer(l.dropped_attributes_count as i64));
        Value::Object(lm)
    }).collect();
    map.insert(f::SPAN_LINKS.into(), Value::Array(links_arr));
    map.insert("dropped_links_count".into(), Value::Integer(span_proto.dropped_links_count as i64));

    if let Some(status) = &span_proto.status {
        let mut sm = ObjectMap::new();
        sm.insert("message".into(), Value::Bytes(status.message.clone().into()));
        sm.insert("code".into(), Value::Integer(status.code as i64));
        map.insert(f::SPAN_STATUS.into(), Value::Object(sm));
    }

    if let Some(resource) = event.resource_proto() {
        map.insert(f::RESOURCE.into(), otel_resource_to_value(&resource));
    }
    if let Some(scope) = event.scope_proto() {
        map.insert(f::SCOPE.into(), otel_scope_to_value(&scope));
    }

    Value::Object(map)
}

fn value_to_otel_span_event(value: Value, metadata: EventMetadata) -> OtelSpan {
    use opentelemetry_proto::tonic::trace::v1::{Span, Status, span};

    let map = match value {
        Value::Object(m) => m,
        _ => ObjectMap::new(),
    };

    let events = map
        .get(f::SPAN_EVENTS)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let em = v.as_object()?;
                    Some(span::Event {
                        time_unix_nano: em.get(f::TIME_UNIX_NANO).and_then(|v| v.as_integer()).unwrap_or(0) as u64,
                        name: em.get(f::NAME).and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
                        attributes: em.get(f::ATTRIBUTES).and_then(|v| v.as_object()).map(object_map_to_otel_kvlist).unwrap_or_default(),
                        dropped_attributes_count: em.get(f::DROPPED_ATTRIBUTES_COUNT).and_then(|v| v.as_integer()).unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let links = map
        .get(f::SPAN_LINKS)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let lm = v.as_object()?;
                    Some(span::Link {
                        trace_id: lm.get(f::SPAN_TRACE_ID).map(hex_decode_value).unwrap_or_default(),
                        span_id: lm.get(f::SPAN_SPAN_ID).map(hex_decode_value).unwrap_or_default(),
                        trace_state: lm.get("trace_state").and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
                        attributes: lm.get(f::ATTRIBUTES).and_then(|v| v.as_object()).map(object_map_to_otel_kvlist).unwrap_or_default(),
                        dropped_attributes_count: lm.get(f::DROPPED_ATTRIBUTES_COUNT).and_then(|v| v.as_integer()).unwrap_or(0) as u32,
                        flags: lm.get(f::SPAN_FLAGS).and_then(|v| v.as_integer()).unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let status = map.get(f::SPAN_STATUS).and_then(|v| {
        let sm = v.as_object()?;
        Some(Status {
            message: sm.get("message").and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
            code: sm.get("code").and_then(|v| v.as_integer()).unwrap_or(0) as i32,
        })
    });

    let span_proto = Span {
        trace_id: map.get(f::SPAN_TRACE_ID).map(hex_decode_value).unwrap_or_default(),
        span_id: map.get(f::SPAN_SPAN_ID).map(hex_decode_value).unwrap_or_default(),
        trace_state: map.get("trace_state").and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        parent_span_id: map.get(f::PARENT_SPAN_ID).map(hex_decode_value).unwrap_or_default(),
        name: map.get(f::NAME).and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        kind: map.get(f::SPAN_KIND).and_then(|v| v.as_integer()).unwrap_or(0) as i32,
        start_time_unix_nano: map.get(f::START_TIME_UNIX_NANO).and_then(|v| v.as_integer()).unwrap_or(0) as u64,
        end_time_unix_nano: map.get(f::END_TIME_UNIX_NANO).and_then(|v| v.as_integer()).unwrap_or(0) as u64,
        attributes: map.get(f::ATTRIBUTES).and_then(|v| v.as_object()).map(object_map_to_otel_kvlist).unwrap_or_default(),
        dropped_attributes_count: map.get(f::DROPPED_ATTRIBUTES_COUNT).and_then(|v| v.as_integer()).unwrap_or(0) as u32,
        events,
        dropped_events_count: map.get("dropped_events_count").and_then(|v| v.as_integer()).unwrap_or(0) as u32,
        links,
        dropped_links_count: map.get("dropped_links_count").and_then(|v| v.as_integer()).unwrap_or(0) as u32,
        status,
        flags: map.get(f::SPAN_FLAGS).and_then(|v| v.as_integer()).unwrap_or(0) as u32,
    };

    let resource = map.get(f::RESOURCE).and_then(value_to_otel_resource);
    let scope = map.get(f::SCOPE).and_then(value_to_otel_scope);

    OtelSpan::from_parts(span_proto, resource, scope, metadata)
}

// ---------------------------------------------------------------------------
// OtelMetric -> Value projection (restricted, read-heavy)
// ---------------------------------------------------------------------------

/// OTel-native Metric → VRL Value projection.
///
/// Exposes legacy paths (.kind, .namespace) and OTel-native paths (.data).
/// `.attributes` flattens first data point's attributes.
fn precompute_otel_metric_value(
    event: &OtelMetric,
    _info: &ProgramInfo,
) -> Value {
    use opentelemetry_proto::tonic::metrics::v1::metric;

    let mut map = ObjectMap::new();

    // Proto fields
    map.insert("name".into(), Value::Bytes(event.name().to_owned().into()));
    if !event.metric().description.is_empty() {
        map.insert("description".into(), Value::Bytes(event.metric().description.clone().into()));
    }
    if !event.metric().unit.is_empty() {
        map.insert("unit".into(), Value::Bytes(event.metric().unit.clone().into()));
    }

    if let Some(resource) = event.resource_proto() {
        map.insert("resource".into(), otel_resource_to_value(&resource));
    }
    if let Some(scope) = event.scope_proto() {
        map.insert("scope".into(), otel_scope_to_value(&scope));
    }

    // .attributes — shorthand for first data point's attributes
    if let Some(dp) = event.first_dp_attrs() {
        if !dp.is_empty() {
            map.insert("attributes".into(), Value::Object(dp.to_object_map()));
        }
    }

    // .kind — "absolute" or "incremental"
    let kind_str = match event.kind() {
        crate::event::MetricKind::Absolute => "absolute",
        crate::event::MetricKind::Incremental => "incremental",
    };
    map.insert("kind".into(), Value::Bytes(kind_str.into()));

    // .namespace
    if let Some(ns) = event.namespace() {
        map.insert("namespace".into(), Value::Bytes(ns.to_owned().into()));
    }

    // .data — full OTel proto structure (with dp attrs populated)
    let metric_with_attrs = event.metric_proto();
    if let Some(data) = &metric_with_attrs.data {
        let mut data_map = ObjectMap::new();
        match data {
            metric::Data::Sum(sum) => {
                data_map.insert("type".into(), Value::Bytes("sum".into()));
                let mut sum_map = ObjectMap::new();
                sum_map.insert("is_monotonic".into(), Value::Boolean(sum.is_monotonic));
                sum_map.insert("aggregation_temporality".into(), Value::Integer(i64::from(sum.aggregation_temporality)));
                sum_map.insert("data_points".into(), number_data_points_to_value(&sum.data_points));
                data_map.insert("sum".into(), Value::Object(sum_map));
            }
            metric::Data::Gauge(gauge) => {
                data_map.insert("type".into(), Value::Bytes("gauge".into()));
                let mut gauge_map = ObjectMap::new();
                gauge_map.insert("data_points".into(), number_data_points_to_value(&gauge.data_points));
                data_map.insert("gauge".into(), Value::Object(gauge_map));
            }
            metric::Data::Histogram(histo) => {
                data_map.insert("type".into(), Value::Bytes("histogram".into()));
                let mut h_map = ObjectMap::new();
                h_map.insert("aggregation_temporality".into(), Value::Integer(i64::from(histo.aggregation_temporality)));
                let dps: Vec<Value> = histo.data_points.iter().map(|dp| {
                    let mut m = ObjectMap::new();
                    m.insert("attributes".into(), Value::Object(otel_kvlist_to_object_map(&dp.attributes)));
                    m.insert("count".into(), Value::Integer(dp.count as i64));
                    if let Some(sum) = dp.sum { m.insert("sum".into(), Value::Float(ordered_float::NotNan::new(sum).unwrap_or(ordered_float::NotNan::new(0.0).unwrap()))); }
                    m.insert("bucket_counts".into(), Value::Array(dp.bucket_counts.iter().map(|c| Value::Integer(*c as i64)).collect()));
                    m.insert("explicit_bounds".into(), Value::Array(dp.explicit_bounds.iter().map(|b| Value::Float(ordered_float::NotNan::new(*b).unwrap_or(ordered_float::NotNan::new(0.0).unwrap()))).collect()));
                    m.insert("time_unix_nano".into(), Value::Integer(dp.time_unix_nano as i64));
                    Value::Object(m)
                }).collect();
                h_map.insert("data_points".into(), Value::Array(dps));
                data_map.insert("histogram".into(), Value::Object(h_map));
            }
            metric::Data::ExponentialHistogram(eh) => {
                data_map.insert("type".into(), Value::Bytes("exponential_histogram".into()));
                let mut eh_map = ObjectMap::new();
                eh_map.insert("aggregation_temporality".into(), Value::Integer(i64::from(eh.aggregation_temporality)));
                let dps: Vec<Value> = eh.data_points.iter().map(|dp| {
                    let mut m = ObjectMap::new();
                    m.insert("attributes".into(), Value::Object(otel_kvlist_to_object_map(&dp.attributes)));
                    m.insert("count".into(), Value::Integer(dp.count as i64));
                    if let Some(sum) = dp.sum { m.insert("sum".into(), Value::Float(ordered_float::NotNan::new(sum).unwrap_or(ordered_float::NotNan::new(0.0).unwrap()))); }
                    m.insert("scale".into(), Value::Integer(i64::from(dp.scale)));
                    m.insert("zero_count".into(), Value::Integer(dp.zero_count as i64));
                    m.insert("time_unix_nano".into(), Value::Integer(dp.time_unix_nano as i64));
                    Value::Object(m)
                }).collect();
                eh_map.insert("data_points".into(), Value::Array(dps));
                data_map.insert("exponential_histogram".into(), Value::Object(eh_map));
            }
            metric::Data::Summary(summary) => {
                data_map.insert("type".into(), Value::Bytes("summary".into()));
                let mut s_map = ObjectMap::new();
                let dps: Vec<Value> = summary.data_points.iter().map(|dp| {
                    let mut m = ObjectMap::new();
                    m.insert("attributes".into(), Value::Object(otel_kvlist_to_object_map(&dp.attributes)));
                    m.insert("count".into(), Value::Integer(dp.count as i64));
                    m.insert("sum".into(), Value::Float(ordered_float::NotNan::new(dp.sum).unwrap_or(ordered_float::NotNan::new(0.0).unwrap())));
                    m.insert("time_unix_nano".into(), Value::Integer(dp.time_unix_nano as i64));
                    Value::Object(m)
                }).collect();
                s_map.insert("data_points".into(), Value::Array(dps));
                data_map.insert("summary".into(), Value::Object(s_map));
            }
        }
        map.insert("data".into(), Value::Object(data_map));
    }

    Value::Object(map)
}

/// Convert NumberDataPoint array to VRL Value.
fn number_data_points_to_value(dps: &[opentelemetry_proto::tonic::metrics::v1::NumberDataPoint]) -> Value {
    use opentelemetry_proto::tonic::metrics::v1::number_data_point;
    Value::Array(dps.iter().map(|dp| {
        let mut m = ObjectMap::new();
        m.insert("attributes".into(), Value::Object(otel_kvlist_to_object_map(&dp.attributes)));
        m.insert("time_unix_nano".into(), Value::Integer(dp.time_unix_nano as i64));
        if dp.start_time_unix_nano != 0 {
            m.insert("start_time_unix_nano".into(), Value::Integer(dp.start_time_unix_nano as i64));
        }
        match &dp.value {
            Some(number_data_point::Value::AsDouble(d)) => {
                m.insert("value".into(), Value::Float(ordered_float::NotNan::new(*d).unwrap_or(ordered_float::NotNan::new(0.0).unwrap())));
            }
            Some(number_data_point::Value::AsInt(i)) => {
                m.insert("value".into(), Value::Integer(*i));
            }
            None => {}
        }
        Value::Object(m)
    }).collect())
}

/// An adapter to turn `Event`s into `vrl_lib::Target`s.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum VrlTarget {
    OtelLog(Value, EventMetadata),
    OtelSpan(Value, EventMetadata),
    OtelMetric {
        event: OtelMetric,
        value: Value,
    },
}

pub enum TargetEvents {
    One(Event),
    OtelLogs(TargetIter<OtelLog>),
    OtelSpans(TargetIter<OtelSpan>),
}

pub struct TargetIter<T> {
    iter: std::vec::IntoIter<Value>,
    metadata: EventMetadata,
    _marker: PhantomData<T>,
}

impl Iterator for TargetIter<OtelLog> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|v| {
            Event::Log(value_to_otel_log_event(v, self.metadata.clone()))
        })
    }
}

impl Iterator for TargetIter<OtelSpan> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|v| {
            Event::Trace(value_to_otel_span_event(v, self.metadata.clone()))
        })
    }
}

impl VrlTarget {
    pub fn new(event: Event, info: &ProgramInfo, _multi_value_metric_tags: bool) -> Self {
        match event {
            Event::Log(event) => {
                let metadata = event.metadata().clone();
                let value = otel_log_event_to_value(&event);
                VrlTarget::OtelLog(value, metadata)
            }
            Event::Trace(event) => {
                let metadata = event.metadata().clone();
                let value = otel_span_event_to_value(&event);
                VrlTarget::OtelSpan(value, metadata)
            }
            Event::Metric(event) => {
                let value = precompute_otel_metric_value(&event, info);
                VrlTarget::OtelMetric { event, value }
            }
        }
    }

    /// Modifies a schema in the same way that the `into_events` function modifies the event
    pub fn modify_schema_definition_for_into_events(input: Definition) -> Definition {
        merge_array_definitions(input)
    }

    /// Turn the target back into events.
    ///
    /// This returns an iterator of events as one event can be turned into multiple by assigning an
    /// array to `.` in VRL.
    pub fn into_events(self) -> TargetEvents {
        match self {
            VrlTarget::OtelLog(value, metadata) => match value {
                value @ Value::Object(_) => {
                    TargetEvents::One(Event::Log(value_to_otel_log_event(value, metadata)))
                }
                Value::Array(values) => TargetEvents::OtelLogs(TargetIter {
                    iter: values.into_iter(),
                    metadata,
                    _marker: PhantomData,
                }),
                value => {
                    TargetEvents::One(Event::Log(value_to_otel_log_event(value, metadata)))
                }
            },
            VrlTarget::OtelSpan(value, metadata) => match value {
                value @ Value::Object(_) => {
                    TargetEvents::One(Event::Trace(value_to_otel_span_event(value, metadata)))
                }
                Value::Array(values) => TargetEvents::OtelSpans(TargetIter {
                    iter: values.into_iter(),
                    metadata,
                    _marker: PhantomData,
                }),
                value => {
                    TargetEvents::One(Event::Trace(value_to_otel_span_event(value, metadata)))
                }
            },
            VrlTarget::OtelMetric { event, .. } => TargetEvents::One(Event::Metric(event)),
        }
    }

    fn metadata(&self) -> &EventMetadata {
        match self {
            VrlTarget::OtelLog(_, metadata) | VrlTarget::OtelSpan(_, metadata) => metadata,
            VrlTarget::OtelMetric { event, .. } => event.metadata(),
        }
    }

    fn metadata_mut(&mut self) -> &mut EventMetadata {
        match self {
            VrlTarget::OtelLog(_, metadata) | VrlTarget::OtelSpan(_, metadata) => metadata,
            VrlTarget::OtelMetric { event, .. } => event.metadata_mut(),
        }
    }
}

/// If the VRL returns a value that is not an array (see [`merge_array_definitions`]),
/// or an object, that data is moved into the `message` field.
#[cfg(test)]
fn move_field_definitions_into_message(mut definition: Definition) -> Definition {
    let mut message = definition.event_kind().clone();
    message.remove_object();
    message.remove_array();

    if !message.is_never() {
        let message_key = owned_value_path!("body");
        // We need to add the given message type to a field called `body`
        // in the event.
        let message = Kind::object(Collection::from(BTreeMap::from([(
            message_key.to_string().into(),
            message,
        )])));

        definition.event_kind_mut().remove_bytes();
        definition.event_kind_mut().remove_integer();
        definition.event_kind_mut().remove_float();
        definition.event_kind_mut().remove_boolean();
        definition.event_kind_mut().remove_timestamp();
        definition.event_kind_mut().remove_regex();
        definition.event_kind_mut().remove_null();

        *definition.event_kind_mut() = definition.event_kind().union(message);
    }

    definition
}

/// If the transform returns an array, the elements of this array will be separated
/// out into it's individual elements and passed downstream.
///
/// The potential types that the transform can output are any of the arrays
/// elements or any non-array elements that are within the definition. All these
/// definitions need to be merged together.
fn merge_array_definitions(mut definition: Definition) -> Definition {
    if let Some(array) = definition.event_kind().as_array() {
        let array_kinds = array.reduced_kind();

        let kind = definition.event_kind_mut();
        kind.remove_array();
        *kind = kind.union(array_kinds);
    }

    definition
}

impl Target for VrlTarget {
    fn target_insert(&mut self, target_path: &OwnedTargetPath, value: Value) -> Result<(), String> {
        let path = &target_path.path;
        match target_path.prefix {
            PathPrefix::Event => match self {
                VrlTarget::OtelLog(log, _) | VrlTarget::OtelSpan(log, _) => {
                    log.insert(path, value);
                    Ok(())
                }
                VrlTarget::OtelMetric {
                    event,
                    value: metric_value,
                } => {
                    if path.is_root() {
                        return Err(MetricPathError::SetPathError.to_string());
                    }

                    if let Some(paths) = path.to_alternative_components(MAX_OTEL_METRIC_PATH_DEPTH) {
                        match paths.as_slice() {
                            ["name"] => {
                                let v = value.clone().try_bytes().map_err(|e| e.to_string())?;
                                event.metric_mut().name = String::from_utf8_lossy(&v).into_owned();
                            }
                            ["description"] => {
                                let v = value.clone().try_bytes().map_err(|e| e.to_string())?;
                                event.metric_mut().description = String::from_utf8_lossy(&v).into_owned();
                            }
                            ["unit"] => {
                                let v = value.clone().try_bytes().map_err(|e| e.to_string())?;
                                event.metric_mut().unit = String::from_utf8_lossy(&v).into_owned();
                            }
                            ["resource"] => {
                                if let Some(resource) = value_to_otel_resource(&value) {
                                    event.set_resource(resource);
                                }
                            }
                            ["resource", rest @ ..] => {
                                let mut res_val = event.resource_proto()
                                    .map(|r| otel_resource_to_value(&r))
                                    .unwrap_or_else(|| Value::Object(ObjectMap::new()));
                                insert_at_segments(&mut res_val, rest, value.clone());
                                if let Some(resource) = value_to_otel_resource(&res_val) {
                                    event.set_resource(resource);
                                }
                            }
                            ["scope"] => {
                                if let Some(scope) = value_to_otel_scope(&value) {
                                    event.set_scope(scope);
                                }
                            }
                            ["scope", rest @ ..] => {
                                let mut scope_val = event.scope_proto()
                                    .map(|s| otel_scope_to_value(&s))
                                    .unwrap_or_else(|| Value::Object(ObjectMap::new()));
                                insert_at_segments(&mut scope_val, rest, value.clone());
                                if let Some(scope) = value_to_otel_scope(&scope_val) {
                                    event.set_scope(scope);
                                }
                            }
                            // Legacy: .kind ("absolute" / "incremental")
                            ["kind"] => {
                                let v = value.clone().try_bytes().map_err(|e| e.to_string())?;
                                let kind = match String::from_utf8_lossy(&v).as_ref() {
                                    "incremental" => crate::event::MetricKind::Incremental,
                                    _ => crate::event::MetricKind::Absolute,
                                };
                                event.set_kind(kind);
                            }
                            // Legacy: .namespace
                            ["namespace"] => {
                                let v = value.clone().try_bytes().map_err(|e| e.to_string())?;
                                event.set_namespace(String::from_utf8_lossy(&v).into_owned());
                            }
                            // OTel-native: .data.*.data_points[*].attributes."key"
                            // Shorthand: .attributes."key" (sets on all data points)
                            ["attributes", attr_key] => {
                                event.set_data_point_attribute(
                                    attr_key.to_string(),
                                    super::vrl_value_to_any_value(&value),
                                );
                            }
                            ["data", _, "data_points", _, "attributes", attr_key] => {
                                event.set_data_point_attribute(
                                    attr_key.to_string(),
                                    super::vrl_value_to_any_value(&value),
                                );
                            }
                            _ => {
                                return Err(MetricPathError::InvalidPath {
                                    path: &path.to_string(),
                                    expected: VALID_OTEL_METRIC_PATHS_SET,
                                }
                                .to_string());
                            }
                        }
                        metric_value.insert(path, value);
                        return Ok(());
                    }

                    Err(MetricPathError::InvalidPath {
                        path: &path.to_string(),
                        expected: VALID_OTEL_METRIC_PATHS_SET,
                    }
                    .to_string())
                }
            },
            PathPrefix::Metadata => {
                self.metadata_mut()
                    .value_mut()
                    .insert(&target_path.path, value);
                Ok(())
            }
        }
    }

    #[allow(clippy::redundant_closure_for_method_calls)] // false positive
    fn target_get(&self, target_path: &OwnedTargetPath) -> Result<Option<&Value>, String> {
        match target_path.prefix {
            PathPrefix::Event => match self {
                VrlTarget::OtelLog(log, _) | VrlTarget::OtelSpan(log, _) => {
                    Ok(log.get(&target_path.path))
                }
                VrlTarget::OtelMetric { value, .. } => {
                    target_get_otel_metric(&target_path.path, value)
                }
            },
            PathPrefix::Metadata => Ok(self.metadata().value().get(&target_path.path)),
        }
    }

    fn target_get_mut(
        &mut self,
        target_path: &OwnedTargetPath,
    ) -> Result<Option<&mut Value>, String> {
        match target_path.prefix {
            PathPrefix::Event => match self {
                VrlTarget::OtelLog(log, _) | VrlTarget::OtelSpan(log, _) => {
                    Ok(log.get_mut(&target_path.path))
                }
                VrlTarget::OtelMetric { value, .. } => {
                    target_get_mut_otel_metric(&target_path.path, value)
                }
            },
            PathPrefix::Metadata => Ok(self.metadata_mut().value_mut().get_mut(&target_path.path)),
        }
    }

    fn target_remove(
        &mut self,
        target_path: &OwnedTargetPath,
        compact: bool,
    ) -> Result<Option<vrl::value::Value>, String> {
        match target_path.prefix {
            PathPrefix::Event => match self {
                VrlTarget::OtelLog(log, _) | VrlTarget::OtelSpan(log, _) => {
                    Ok(log.remove(&target_path.path, compact))
                }
                VrlTarget::OtelMetric { event, value } => {
                    if target_path.path.is_root() {
                        return Err(MetricPathError::SetPathError.to_string());
                    }
                    let removed = value.remove(&target_path.path, false);
                    // Write-back known mutable fields to the proto event.
                    if let Some(paths) = target_path.path.to_alternative_components(MAX_OTEL_METRIC_PATH_DEPTH) {
                        match paths.as_slice() {
                            ["name"] => { event.metric_mut().name = String::new(); }
                            ["description"] => { event.metric_mut().description = String::new(); }
                            ["unit"] => { event.metric_mut().unit = String::new(); }
                            ["resource"] => { event.set_resource(Default::default()); }
                            ["scope"] => { event.set_scope(Default::default()); }
                            ["attributes", attr_key] => {
                                event.remove_data_point_attribute(attr_key);
                            }
                            _ => {}
                        }
                    }
                    Ok(removed)
                }
            },
            PathPrefix::Metadata => Ok(self
                .metadata_mut()
                .value_mut()
                .remove(&target_path.path, compact)),
        }
    }
}

impl SecretTarget for VrlTarget {
    fn get_secret(&self, key: &str) -> Option<&str> {
        self.metadata().secrets().get_secret(key)
    }

    fn insert_secret(&mut self, key: &str, value: &str) {
        self.metadata_mut().secrets_mut().insert_secret(key, value);
    }

    fn remove_secret(&mut self, key: &str) {
        self.metadata_mut().secrets_mut().remove_secret(key);
    }
}

fn target_get_otel_metric<'a>(
    path: &OwnedValuePath,
    value: &'a Value,
) -> Result<Option<&'a Value>, String> {
    if path.is_root() {
        return Ok(Some(value));
    }

    let value = value.get(path);

    let Some(paths) = path.to_alternative_components(MAX_OTEL_METRIC_PATH_DEPTH) else {
        return Ok(None);
    };

    match paths.as_slice() {
        ["name"] | ["description"] | ["unit"] | ["resource"] | ["resource", ..]
        | ["scope"] | ["scope", ..] | ["data"] | ["attributes"] | ["attributes", ..] | ["kind"]
        | ["namespace"] => Ok(value),
        _ => Err(MetricPathError::InvalidPath {
            path: &path.to_string(),
            expected: VALID_OTEL_METRIC_PATHS_GET,
        }
        .to_string()),
    }
}

fn target_get_mut_otel_metric<'a>(
    path: &OwnedValuePath,
    value: &'a mut Value,
) -> Result<Option<&'a mut Value>, String> {
    if path.is_root() {
        return Ok(Some(value));
    }

    let value = value.get_mut(path);

    let Some(paths) = path.to_alternative_components(MAX_OTEL_METRIC_PATH_DEPTH) else {
        return Ok(None);
    };

    match paths.as_slice() {
        ["name"] | ["description"] | ["unit"] | ["resource"] | ["resource", ..]
        | ["scope"] | ["scope", ..] | ["attributes"] | ["attributes", ..] | ["kind"]
        | ["namespace"] => Ok(value),
        _ => Err(MetricPathError::InvalidPath {
            path: &path.to_string(),
            expected: VALID_OTEL_METRIC_PATHS_SET,
        }
        .to_string()),
    }
}

#[derive(Debug, Snafu)]
enum MetricPathError<'a> {
    #[snafu(display("cannot set root path"))]
    SetPathError,

    #[snafu(display("invalid path {}: expected one of {}", path, expected))]
    InvalidPath { path: &'a str, expected: &'a str },
}

#[cfg(test)]
mod test {
    use lookup::owned_value_path;
    use similar_asserts::assert_eq;
    use vrl::{btreemap, value::kind::Index};

    use super::{super::OtelMetric, *};
    #[test]
    fn test_field_definitions_in_message() {
        let definition =
            Definition::new_with_default_metadata(Kind::bytes());
        assert_eq!(
            Definition::new_with_default_metadata(
                Kind::object(BTreeMap::from([("body".into(), Kind::bytes())]))
            ),
            move_field_definitions_into_message(definition)
        );

        // Test when a body field already exists.
        let definition = Definition::new_with_default_metadata(
            Kind::object(BTreeMap::from([("body".into(), Kind::integer())])).or_bytes()
        );
        assert_eq!(
            Definition::new_with_default_metadata(
                Kind::object(BTreeMap::from([(
                    "body".into(),
                    Kind::bytes().or_integer()
                )]))
            ),
            move_field_definitions_into_message(definition)
        );
    }

    #[test]
    fn test_merged_array_definitions_simple() {
        // Test merging the array definitions where the schema definition
        // is simple, containing only one possible type in the array.
        let object: BTreeMap<vrl::value::kind::Field, Kind> = [
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]
        .into();

        let kind = Kind::array(Collection::from_unknown(Kind::object(object)));

        let definition = Definition::new_with_default_metadata(kind);

        let kind = Kind::object(BTreeMap::from([
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]));

        let wanted = Definition::new_with_default_metadata(kind);
        let merged = merge_array_definitions(definition);

        assert_eq!(wanted, merged);
    }

    #[test]
    fn test_merged_array_definitions_complex() {
        // Test merging the array definitions where the schema definition
        // is fairly complex containing multiple different possible types.
        let object: BTreeMap<vrl::value::kind::Field, Kind> = [
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]
        .into();

        let array: BTreeMap<Index, Kind> = [
            (Index::from(0), Kind::integer()),
            (Index::from(1), Kind::boolean()),
            (
                Index::from(2),
                Kind::object(BTreeMap::from([("peas".into(), Kind::bytes())])),
            ),
        ]
        .into();

        let mut kind = Kind::bytes();
        kind.add_object(object);
        kind.add_array(array);

        let definition = Definition::new_with_default_metadata(kind);

        let mut kind = Kind::bytes();
        kind.add_integer();
        kind.add_boolean();
        kind.add_object(BTreeMap::from([
            ("carrot".into(), Kind::bytes().or_undefined()),
            ("potato".into(), Kind::integer().or_undefined()),
            ("peas".into(), Kind::bytes().or_undefined()),
        ]));

        let wanted = Definition::new_with_default_metadata(kind);
        let merged = merge_array_definitions(definition);

        assert_eq!(wanted, merged);
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn log_insert() {
        let cases = vec![
            (
                BTreeMap::from([("foo".into(), "bar".into())]),
                owned_value_path!(0),
                btreemap! { "baz" => "qux" }.into(),
                btreemap! { "baz" => "qux" },
                Ok(()),
            ),
            (
                BTreeMap::from([("foo".into(), "bar".into())]),
                owned_value_path!("foo"),
                "baz".into(),
                btreemap! { "foo" => "baz" },
                Ok(()),
            ),
            (
                BTreeMap::from([("foo".into(), "bar".into())]),
                owned_value_path!("foo", 2, "bar baz", "a", "b"),
                true.into(),
                btreemap! {
                    "foo" => vec![
                        Value::Null,
                        Value::Null,
                        btreemap! {
                            "bar baz" => btreemap! { "a" => btreemap! { "b" => true } },
                        }.into()
                    ]
                },
                Ok(()),
            ),
            (
                btreemap! { "foo" => vec![0, 1, 2] },
                owned_value_path!("foo", 5),
                "baz".into(),
                btreemap! {
                    "foo" => vec![
                        0.into(),
                        1.into(),
                        2.into(),
                        Value::Null,
                        Value::Null,
                        Value::from("baz"),
                    ],
                },
                Ok(()),
            ),
            (
                BTreeMap::from([("foo".into(), "bar".into())]),
                owned_value_path!("foo", 0),
                "baz".into(),
                btreemap! { "foo" => vec!["baz"] },
                Ok(()),
            ),
            (
                btreemap! { "foo" => Value::Array(vec![]) },
                owned_value_path!("foo", 0),
                "baz".into(),
                btreemap! { "foo" => vec!["baz"] },
                Ok(()),
            ),
            (
                btreemap! { "foo" => Value::Array(vec![0.into()]) },
                owned_value_path!("foo", 0),
                "baz".into(),
                btreemap! { "foo" => vec!["baz"] },
                Ok(()),
            ),
            (
                btreemap! { "foo" => Value::Array(vec![0.into(), 1.into()]) },
                owned_value_path!("foo", 0),
                "baz".into(),
                btreemap! { "foo" => Value::Array(vec!["baz".into(), 1.into()]) },
                Ok(()),
            ),
            (
                btreemap! { "foo" => Value::Array(vec![0.into(), 1.into()]) },
                owned_value_path!("foo", 1),
                "baz".into(),
                btreemap! { "foo" => Value::Array(vec![0.into(), "baz".into()]) },
                Ok(()),
            ),
        ];

        for (object, path, value, expect, result) in cases {
            let object: ObjectMap = object;
            let info = ProgramInfo {
                fallible: false,
                abortable: false,
                target_queries: vec![],
                target_assignments: vec![],
            };
            let mut target = VrlTarget::new(Event::Log(OtelLog::from(Value::Object(object))), &info, false);
            let expect = OtelLog::from(Value::Object(expect));
            let value: Value = value;
            let path = OwnedTargetPath::event(path);

            assert_eq!(
                Target::target_insert(&mut target, &path, value.clone()),
                result
            );
            assert_eq!(
                Target::target_get(&target, &path).map(Option::<&Value>::cloned),
                Ok(Some(value))
            );
            assert_eq!(
                match target.into_events() {
                    TargetEvents::One(event) => vec![event],
                    TargetEvents::OtelLogs(events) => events.collect::<Vec<_>>(),
                    TargetEvents::OtelSpans(events) => events.collect::<Vec<_>>(),
                }
                .first()
                .cloned()
                .unwrap(),
                Event::Log(expect)
            );
        }
    }

    // -- OTel-native VrlTarget tests --

    fn make_info_with_queries(paths: &[&str]) -> ProgramInfo {
        ProgramInfo {
            fallible: false,
            abortable: false,
            target_queries: paths
                .iter()
                .map(|p| {
                    OwnedTargetPath::event(
                        OwnedValuePath::try_from(p.to_string()).unwrap(),
                    )
                })
                .collect(),
            target_assignments: vec![],
        }
    }

    fn make_empty_info() -> ProgramInfo {
        ProgramInfo {
            fallible: false,
            abortable: false,
            target_queries: vec![],
            target_assignments: vec![],
        }
    }

    #[test]
    fn otel_log_vrl_target_get_severity_text() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let event = OtelLog::new(LogRecord {
            severity_text: "ERROR".to_string(),
            ..Default::default()
        });
        let info = make_empty_info();
        let target = VrlTarget::new(Event::Log(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("severity_text"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("ERROR".into())));
    }

    #[test]
    fn otel_log_vrl_target_get_attribute() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let event = OtelLog::new(LogRecord {
            attributes: vec![OtelKeyValue {
                key: "host.name".to_string(),
                value: Some(OtelAnyValue {
                    value: Some(OtelValueKind::StringValue("myhost".to_string())),
                }),
            }],
            ..Default::default()
        });
        let info = make_empty_info();
        let target = VrlTarget::new(Event::Log(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("attributes", "host.name"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("myhost".into())));
    }

    #[test]
    fn otel_log_vrl_target_insert_attribute() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let event = OtelLog::new(LogRecord::default());
        let info = make_empty_info();
        let mut target = VrlTarget::new(Event::Log(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("attributes", "host.name"));
        Target::target_insert(&mut target, &path, Value::Bytes("myhost".into())).unwrap();

        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("myhost".into())));
    }

    #[test]
    fn otel_log_vrl_target_roundtrip_into_events() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let event = OtelLog::new(LogRecord {
            severity_text: "INFO".to_string(),
            severity_number: 9,
            time_unix_nano: 1234567890,
            attributes: vec![OtelKeyValue {
                key: "service".to_string(),
                value: Some(OtelAnyValue {
                    value: Some(OtelValueKind::StringValue("web".to_string())),
                }),
            }],
            ..Default::default()
        });
        let info = make_empty_info();
        let target = VrlTarget::new(Event::Log(event), &info, false);

        let events: Vec<Event> = match target.into_events() {
            TargetEvents::One(e) => vec![e],
            _ => panic!("expected one event"),
        };
        assert_eq!(events.len(), 1);

        let restored = match &events[0] {
            Event::Log(e) => e,
            _ => panic!("expected OtelLog"),
        };
        assert_eq!(restored.severity_text(), "INFO");
        assert_eq!(restored.severity_number(), 9);
        assert_eq!(restored.time_unix_nano(), 1234567890);
        assert_eq!(restored.attributes().len(), 1);
        assert!(restored.attributes().get("service").is_some());
    }

    #[test]
    fn otel_log_vrl_target_mutate_and_roundtrip() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let event = OtelLog::new(LogRecord {
            severity_text: "DEBUG".to_string(),
            ..Default::default()
        });
        let info = make_empty_info();
        let mut target = VrlTarget::new(Event::Log(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("severity_text"));
        Target::target_insert(&mut target, &path, Value::Bytes("WARN".into())).unwrap();

        let events: Vec<Event> = match target.into_events() {
            TargetEvents::One(e) => vec![e],
            _ => panic!("expected one event"),
        };

        let restored = match &events[0] {
            Event::Log(e) => e,
            _ => panic!("expected OtelLog"),
        };
        assert_eq!(restored.severity_text(), "WARN");
    }

    #[test]
    fn otel_span_vrl_target_get_name() {
        use opentelemetry_proto::tonic::trace::v1::Span;
        let event = OtelSpan::new(Span {
            name: "my-span".to_string(),
            trace_id: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            ..Default::default()
        });
        let info = make_empty_info();
        let target = VrlTarget::new(Event::Trace(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("name"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("my-span".into())));

        let path = OwnedTargetPath::event(owned_value_path!("trace_id"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("0102030405060708090a0b0c0d0e0f10".into())));
    }

    #[test]
    fn otel_span_vrl_target_roundtrip() {
        use opentelemetry_proto::tonic::trace::v1::Span;
        let event = OtelSpan::new(Span {
            name: "test-span".to_string(),
            kind: 2,
            start_time_unix_nano: 1000,
            end_time_unix_nano: 2000,
            ..Default::default()
        });
        let info = make_empty_info();
        let target = VrlTarget::new(Event::Trace(event), &info, false);

        let events: Vec<Event> = match target.into_events() {
            TargetEvents::One(e) => vec![e],
            _ => panic!("expected one event"),
        };

        let restored = match &events[0] {
            Event::Trace(e) => e,
            _ => panic!("expected OtelSpan"),
        };
        assert_eq!(restored.name(), "test-span");
        assert_eq!(restored.kind(), 2);
        assert_eq!(restored.start_time_unix_nano(), 1000);
        assert_eq!(restored.end_time_unix_nano(), 2000);
    }

    #[test]
    fn otel_metric_vrl_target_get_name() {
        use opentelemetry_proto::tonic::metrics::v1::Metric as OtelMetricProto;
        let event = OtelMetric::new(OtelMetricProto {
            name: "http.duration".to_string(),
            description: "request duration".to_string(),
            unit: "ms".to_string(),
            ..Default::default()
        });
        let info = make_info_with_queries(&[".name", ".description", ".unit"]);
        let target = VrlTarget::new(Event::Metric(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("name"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("http.duration".into())));

        let path = OwnedTargetPath::event(owned_value_path!("unit"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("ms".into())));
    }

    #[test]
    fn otel_metric_vrl_target_set_name() {
        use opentelemetry_proto::tonic::metrics::v1::Metric as OtelMetricProto;
        let event = OtelMetric::new(OtelMetricProto {
            name: "old.name".to_string(),
            ..Default::default()
        });
        let info = make_info_with_queries(&[".name"]);
        let mut target = VrlTarget::new(Event::Metric(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("name"));
        Target::target_insert(&mut target, &path, Value::Bytes("new.name".into())).unwrap();

        let events: Vec<Event> = match target.into_events() {
            TargetEvents::One(e) => vec![e],
            _ => panic!("expected one event"),
        };

        let restored = match &events[0] {
            Event::Metric(e) => e,
            _ => panic!("expected OtelMetric"),
        };
        assert_eq!(restored.name(), "new.name");
    }

    #[test]
    fn otel_log_resource_and_scope_roundtrip() {
        use opentelemetry_proto::tonic::logs::v1::LogRecord;
        let mut event = OtelLog::new(LogRecord::default());
        event.set_resource(OtelResource {
            attributes: vec![OtelKeyValue {
                key: "service.name".to_string(),
                value: Some(OtelAnyValue {
                    value: Some(OtelValueKind::StringValue("my-svc".to_string())),
                }),
            }],
            dropped_attributes_count: 0,
        });
        event.set_scope(OtelScope {
            name: "my-lib".to_string(),
            version: "1.0".to_string(),
            ..Default::default()
        });

        let info = make_empty_info();
        let target = VrlTarget::new(Event::Log(event), &info, false);

        let path = OwnedTargetPath::event(owned_value_path!("resource", "attributes", "service.name"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("my-svc".into())));

        let path = OwnedTargetPath::event(owned_value_path!("scope", "name"));
        let result = Target::target_get(&target, &path).unwrap();
        assert_eq!(result, Some(&Value::Bytes("my-lib".into())));

        let events: Vec<Event> = match target.into_events() {
            TargetEvents::One(e) => vec![e],
            _ => panic!("expected one event"),
        };

        let restored = match &events[0] {
            Event::Log(e) => e,
            _ => panic!("expected OtelLog"),
        };
        assert!(restored.resource().is_some());
        assert_eq!(
            restored.resource_attribute("service.name").and_then(|v| {
                match &v.value {
                    Some(OtelValueKind::StringValue(s)) => Some(s.as_str()),
                    _ => None,
                }
            }),
            Some("my-svc")
        );
        assert_eq!(restored.scope().unwrap().name, "my-lib");
        assert_eq!(restored.scope().unwrap().version, "1.0");
    }

    #[test]
    fn otel_any_value_conversion_roundtrip() {
        let cases = vec![
            OtelAnyValue { value: Some(OtelValueKind::StringValue("hello".to_string())) },
            OtelAnyValue { value: Some(OtelValueKind::BoolValue(true)) },
            OtelAnyValue { value: Some(OtelValueKind::IntValue(42)) },
            OtelAnyValue { value: Some(OtelValueKind::DoubleValue(3.14)) },
            OtelAnyValue { value: Some(OtelValueKind::BytesValue(vec![1, 2, 3])) },
            OtelAnyValue { value: None },
        ];

        for original in &cases {
            let vrl_val = otel_any_value_to_vrl(original);
            let restored = vrl_value_to_otel_any_value(&vrl_val);
            match (&original.value, &restored.value) {
                (Some(OtelValueKind::BytesValue(a)), Some(OtelValueKind::StringValue(b))) => {
                    // bytes -> string lossy conversion is expected
                    assert_eq!(String::from_utf8_lossy(a).as_ref(), b.as_str());
                }
                (None, None) => {}
                _ => assert_eq!(original, &restored, "roundtrip failed for {original:?}"),
            }
        }
    }

    #[test]
    fn hex_encode_decode_roundtrip() {
        let original = vec![0x01, 0x23, 0xab, 0xcd, 0xef, 0x00, 0xff];
        let encoded = hex_encode_bytes(&original);
        assert_eq!(encoded, Value::Bytes("0123abcdef00ff".into()));
        let decoded = hex_decode_value(&encoded);
        assert_eq!(decoded, original);
    }
}
