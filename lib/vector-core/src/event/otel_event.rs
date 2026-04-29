use std::collections::{BTreeMap, BTreeSet};
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, InstrumentationScope, KeyValue, any_value::Value as OtelValueKind,
};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use opentelemetry_proto::tonic::metrics::v1::Metric as OtelMetricProto;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use serde::Serialize;
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq,
    byte_size_of::ByteSizeOf,
    finalization::{EventFinalizers, Finalizable},
    internal_event::TaggedEventsSent,
    json_size::JsonSize,
    request_metadata::GetEventCountTags,
};
use vrl::value::{KeyString, ObjectMap, Value};

use super::{
    BatchNotifier, EstimatedJsonEncodedSizeOf, EventFinalizer, EventMetadata,
};

/// Convert a JSON value to an OTel `AnyValue`.
pub fn json_to_any_value(value: serde_json::Value) -> AnyValue {
    let kind = match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(OtelValueKind::BoolValue(b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(OtelValueKind::IntValue(i))
            } else {
                Some(OtelValueKind::DoubleValue(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Some(OtelValueKind::StringValue(s)),
        serde_json::Value::Array(arr) => {
            let values = arr.into_iter().map(json_to_any_value).collect();
            Some(OtelValueKind::ArrayValue(
                opentelemetry_proto::tonic::common::v1::ArrayValue { values },
            ))
        }
        serde_json::Value::Object(map) => {
            let values = map
                .into_iter()
                .map(|(k, v)| KeyValue {
                    key: k,
                    value: Some(json_to_any_value(v)),
                })
                .collect();
            Some(OtelValueKind::KvlistValue(
                opentelemetry_proto::tonic::common::v1::KeyValueList { values },
            ))
        }
    };
    AnyValue { value: kind }
}

fn json_to_vrl_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else {
                Value::Float(ordered_float::NotNan::new(n.as_f64().unwrap_or(0.0)).unwrap_or_default())
            }
        }
        serde_json::Value::String(s) => Value::Bytes(bytes::Bytes::from(s)),
        serde_json::Value::Array(arr) => {
            Value::Array(arr.into_iter().map(json_to_vrl_value).collect())
        }
        serde_json::Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (KeyString::from(k), json_to_vrl_value(v))).collect())
        }
    }
}

/// Create a string `AnyValue`.
pub fn string_value(s: impl Into<String>) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::StringValue(s.into())),
    }
}

/// Create an integer `AnyValue`.
pub fn int_value(i: i64) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::IntValue(i)),
    }
}

/// Tracing `Visit` implementation that accumulates into an `ObjectMap`.
/// Used by `OtelLog::from_tracing_event` to batch all field insertions into
/// a single `from_value_map` conversion.
#[derive(Default)]
struct OtelLogTracingBuilder {
    map: ObjectMap,
}

impl OtelLogTracingBuilder {
    /// Map the tracing field name to the canonical OtelLog key.
    /// The tracing framework uses "message" for the log body, but
    /// the proto-canonical key is "body".
    fn canonical_key(field: &tracing::field::Field) -> vrl::prelude::KeyString {
        let name = field.name();
        if name == "message" { "body".into() } else { name.into() }
    }
}

impl tracing::field::Visit for OtelLogTracingBuilder {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.map
            .insert(Self::canonical_key(field), Value::Bytes(value.to_string().into()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.map
            .insert(Self::canonical_key(field), Value::Bytes(format!("{value:?}").into()));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.map.insert(Self::canonical_key(field), Value::Integer(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match i64::try_from(value) {
            Ok(v) => self.map.insert(Self::canonical_key(field), Value::Integer(v)),
            Err(_) => self
                .map
                .insert(Self::canonical_key(field), Value::Bytes(value.to_string().into())),
        };
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.map.insert(Self::canonical_key(field), Value::Boolean(value));
    }
}

pub(super) fn hex_encode(bytes: &[u8]) -> Value {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    Value::Bytes(s.into())
}

/// Decode a hex string Value back to bytes. Accepts `Value::Bytes` holding
/// an even-length ASCII hex string. Returns `None` if the input is not a
/// well-formed hex byte sequence — in that case the caller should fall back
/// to storing the original Value as an attribute, so corrupt data does not
/// silently disappear.
fn hex_decode(value: &Value) -> Option<Vec<u8>> {
    let bytes = match value {
        Value::Bytes(b) => b,
        _ => return None,
    };
    let s = std::str::from_utf8(bytes).ok()?;
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Try to interpret a VRL Value::Object as an OTLP JSON AnyValue pattern.
///
/// When data roundtrips through OTLP JSON serialization and JSON parsing,
/// AnyValue wrappers like `{"stringValue":"hello"}` become VRL Objects.
/// This function recognizes these patterns and converts them back to the
/// proper AnyValue proto representation.
pub fn try_parse_otlp_any_value(value: &Value) -> Option<AnyValue> {
    let map = match value {
        Value::Object(m) if m.len() == 1 => m,
        _ => return None,
    };

    let (key, val) = map.iter().next()?;
    let key_str = key.as_ref();

    let kind = match key_str {
        "stringValue" => {
            let s = match val {
                Value::Bytes(b) => String::from_utf8(b.to_vec()).ok()?,
                _ => return None,
            };
            Some(OtelValueKind::StringValue(s))
        }
        "intValue" => {
            let i = match val {
                Value::Integer(i) => *i,
                Value::Bytes(b) => std::str::from_utf8(b).ok()?.parse::<i64>().ok()?,
                _ => return None,
            };
            Some(OtelValueKind::IntValue(i))
        }
        "doubleValue" => {
            let d = match val {
                Value::Float(f) => f.into_inner(),
                Value::Integer(i) => *i as f64,
                _ => return None,
            };
            Some(OtelValueKind::DoubleValue(d))
        }
        "boolValue" => {
            let b = match val {
                Value::Boolean(b) => *b,
                _ => return None,
            };
            Some(OtelValueKind::BoolValue(b))
        }
        "bytesValue" => {
            let bytes = match val {
                Value::Bytes(b) => {
                    // May be hex-encoded
                    hex_decode_bytes(b).unwrap_or_else(|| b.to_vec())
                }
                _ => return None,
            };
            Some(OtelValueKind::BytesValue(bytes))
        }
        "arrayValue" => {
            // {"arrayValue": {"values": [...]}}
            let arr = match val {
                Value::Object(obj) => {
                    match obj.get(&KeyString::from("values")) {
                        Some(Value::Array(arr)) => {
                            arr.iter().map(|v| {
                                try_parse_otlp_any_value(v)
                                    .unwrap_or_else(|| vrl_value_to_any_value(v))
                            }).collect()
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            };
            Some(OtelValueKind::ArrayValue(
                opentelemetry_proto::tonic::common::v1::ArrayValue { values: arr },
            ))
        }
        "kvlistValue" => {
            // {"kvlistValue": {"values": [{"key":"k","value":{...}}]}}
            let kvl = match val {
                Value::Object(obj) => {
                    match obj.get(&KeyString::from("values")) {
                        Some(Value::Array(arr)) => {
                            parse_otlp_key_value_array(arr)?
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            };
            Some(OtelValueKind::KvlistValue(
                opentelemetry_proto::tonic::common::v1::KeyValueList { values: kvl },
            ))
        }
        _ => return None,
    };

    Some(AnyValue { value: kind })
}

/// Parse an OTLP JSON attributes array: [{"key":"k","value":{...}}, ...]
fn parse_otlp_key_value_array(arr: &[Value]) -> Option<Vec<KeyValue>> {
    let mut result = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = match item {
            Value::Object(m) => m,
            _ => return None,
        };
        let key = match obj.get(&KeyString::from("key")) {
            Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).ok()?,
            _ => return None,
        };
        let value = obj.get(&KeyString::from("value")).and_then(|v| {
            try_parse_otlp_any_value(v)
        });
        result.push(KeyValue { key, value });
    }
    Some(result)
}

/// Helper: try to decode hex-encoded bytes.
fn hex_decode_bytes(b: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(b).ok()?;
    if s.len() % 2 != 0 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    hex_decode(&Value::Bytes(bytes::Bytes::copy_from_slice(b)))
}

pub fn vrl_value_to_any_value(value: &Value) -> AnyValue {
    let kind = match value {
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => Some(OtelValueKind::StringValue(s.to_owned())),
            Err(_) => Some(OtelValueKind::BytesValue(b.to_vec())),
        },
        Value::Integer(i) => Some(OtelValueKind::IntValue(*i)),
        Value::Float(f) => Some(OtelValueKind::DoubleValue(f.into_inner())),
        Value::Boolean(b) => Some(OtelValueKind::BoolValue(*b)),
        Value::Timestamp(ts) => Some(OtelValueKind::StringValue(
            ts.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        )),
        Value::Regex(r) => Some(OtelValueKind::StringValue(r.to_string())),
        Value::Null => None,
        Value::Object(map) => {
            let values = map
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.to_string(),
                    value: Some(vrl_value_to_any_value(v)),
                })
                .collect();
            Some(OtelValueKind::KvlistValue(
                opentelemetry_proto::tonic::common::v1::KeyValueList { values },
            ))
        }
        Value::Array(arr) => {
            let values = arr.iter().map(vrl_value_to_any_value).collect();
            Some(OtelValueKind::ArrayValue(
                opentelemetry_proto::tonic::common::v1::ArrayValue { values },
            ))
        }
    };
    AnyValue { value: kind }
}

pub(crate) fn any_value_to_vrl(av: &AnyValue) -> Value {
    match &av.value {
        Some(OtelValueKind::StringValue(s)) => Value::Bytes(s.clone().into()),
        Some(OtelValueKind::BoolValue(b)) => Value::Boolean(*b),
        Some(OtelValueKind::IntValue(i)) => Value::Integer(*i),
        Some(OtelValueKind::DoubleValue(d)) => {
            Value::Float(ordered_float::NotNan::new(*d).unwrap_or_default())
        }
        Some(OtelValueKind::BytesValue(b)) => Value::Bytes(bytes::Bytes::copy_from_slice(b)),
        Some(OtelValueKind::ArrayValue(arr)) => {
            Value::Array(arr.values.iter().map(any_value_to_vrl).collect())
        }
        Some(OtelValueKind::KvlistValue(kvl)) => Value::Object(kvlist_to_object_map(&kvl.values)),
        None => Value::Null,
    }
}

pub(crate) fn kvlist_to_object_map(kvs: &[KeyValue]) -> ObjectMap {
    kvs.iter()
        .map(|kv| {
            let v = kv
                .value
                .as_ref()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null);
            (kv.key.clone().into(), v)
        })
        .collect()
}

fn restore_resource(map: &mut ObjectMap) -> (Option<Resource>, OtelAttributes) {
    match map.remove("resource") {
        Some(Value::Object(mut res_map)) => {
            let dropped_count = res_map.remove("dropped_attributes_count")
                .and_then(|v| v.as_integer())
                .unwrap_or(0) as u32;

            // Try OTLP JSON format first: {"attributes":[{"key":"k","value":{...}}]}
            let attrs = if let Some(Value::Array(arr)) = res_map.remove("attributes") {
                if let Some(kvs) = parse_otlp_key_value_array(&arr) {
                    let mut otel_attrs = OtelAttributes::new();
                    for kv in kvs {
                        otel_attrs.insert(kv.key, kv.value.unwrap_or(AnyValue { value: None }));
                    }
                    otel_attrs
                } else {
                    // Not valid OTLP format, put it back and use flat format
                    res_map.insert("attributes".into(), Value::Array(arr));
                    OtelAttributes::from_object_map(&res_map)
                }
            } else {
                // Flat format: {"key": "value", ...}
                OtelAttributes::from_object_map(&res_map)
            };

            let resource = Resource { attributes: Vec::new(), dropped_attributes_count: dropped_count };
            (Some(resource), attrs)
        }
        Some(other) => { map.insert("resource".into(), other); (None, OtelAttributes::new()) }
        None => (None, OtelAttributes::new()),
    }
}

fn restore_scope(map: &mut ObjectMap) -> (Option<InstrumentationScope>, OtelAttributes) {
    match map.remove("scope") {
        Some(Value::Object(mut scope_map)) => {
            let name = match scope_map.remove("name") {
                Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap_or_default(),
                _ => String::new(),
            };
            let version = match scope_map.remove("version") {
                Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap_or_default(),
                _ => String::new(),
            };
            let attrs = match scope_map.remove("attributes") {
                Some(Value::Array(arr)) => {
                    // Try OTLP JSON format: [{"key":"k","value":{...}}]
                    if let Some(kvs) = parse_otlp_key_value_array(&arr) {
                        let mut otel_attrs = OtelAttributes::new();
                        for kv in kvs {
                            otel_attrs.insert(kv.key, kv.value.unwrap_or(AnyValue { value: None }));
                        }
                        otel_attrs
                    } else {
                        OtelAttributes::new()
                    }
                }
                Some(Value::Object(attrs_map)) => OtelAttributes::from_object_map(&attrs_map),
                _ => OtelAttributes::new(),
            };
            if name.is_empty() && version.is_empty() && attrs.is_empty() {
                (None, OtelAttributes::new())
            } else {
                (Some(InstrumentationScope { name, version, attributes: Vec::new(), dropped_attributes_count: 0 }), attrs)
            }
        }
        Some(other) => { map.insert("scope".into(), other); (None, OtelAttributes::new()) }
        None => (None, OtelAttributes::new()),
    }
}

/// Convert an OTel `any_value::Value` to a string for use as a metric tag.
fn otel_value_to_tag_string(v: &OtelValueKind) -> String {
    match v {
        OtelValueKind::StringValue(s) => s.clone(),
        OtelValueKind::BoolValue(b) => b.to_string(),
        OtelValueKind::IntValue(i) => i.to_string(),
        OtelValueKind::DoubleValue(f) => f.to_string(),
        OtelValueKind::BytesValue(b) => String::from_utf8_lossy(b).into_owned(),
        OtelValueKind::ArrayValue(_) => "<array>".to_string(),
        OtelValueKind::KvlistValue(_) => "<kvlist>".to_string(),
    }
}

/// Insert an OTel attribute as a metric tag, handling array values as multi-value tags.
/// Null values (AnyValue with value: None) are treated as bare tags.
fn insert_otel_attr_as_tag_from_any_value(
    tags: &mut super::MetricTags,
    key: &str,
    av: &AnyValue,
) {
    use super::metric::TagValue;
    match &av.value {
        Some(OtelValueKind::ArrayValue(arr)) => {
            let values: Vec<TagValue> = arr
                .values
                .iter()
                .map(|item| match &item.value {
                    Some(v) => TagValue::Value(otel_value_to_tag_string(v)),
                    None => TagValue::Bare,
                })
                .collect();
            tags.set_multi_value(key.to_string(), values);
        }
        Some(v) => {
            tags.insert(key.to_string(), otel_value_to_tag_string(v));
        }
        None => {
            tags.replace(key.to_string(), TagValue::Bare);
        }
    }
}

fn nanos_to_timestamp(nanos: u64) -> Option<Value> {
    let secs = (nanos / 1_000_000_000) as i64;
    let nsecs = (nanos % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsecs).map(Value::Timestamp)
}

fn navigate_value(v: &Value, remaining: &[String]) -> Option<Value> {
    let mut current = v;
    for seg in remaining {
        match current {
            Value::Object(map) => {
                current = map.get(seg.as_str())?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

fn insert_value_at(v: &mut Value, remaining: &[String], new_val: Value) -> Option<Value> {
    if remaining.len() == 1 {
        match v {
            Value::Object(map) => map.insert(remaining[0].as_str().into(), new_val),
            _ => {
                let mut map = ObjectMap::new();
                map.insert(remaining[0].as_str().into(), new_val);
                *v = Value::Object(map);
                None
            }
        }
    } else {
        match v {
            Value::Object(map) => {
                let entry = map
                    .entry(remaining[0].as_str().into())
                    .or_insert_with(|| Value::Object(ObjectMap::new()));
                insert_value_at(entry, &remaining[1..], new_val)
            }
            _ => {
                let mut map = ObjectMap::new();
                let mut inner = Value::Object(ObjectMap::new());
                insert_value_at(&mut inner, &remaining[1..], new_val);
                map.insert(remaining[0].as_str().into(), inner);
                *v = Value::Object(map);
                None
            }
        }
    }
}

fn remove_value_at(v: &mut Value, remaining: &[String], prune: bool) -> Option<Value> {
    if remaining.len() == 1 {
        match v {
            Value::Object(map) => map.remove(remaining[0].as_str()),
            _ => None,
        }
    } else {
        match v {
            Value::Object(map) => {
                let inner = map.get_mut(remaining[0].as_str())?;
                let result = remove_value_at(inner, &remaining[1..], prune);
                if prune {
                    if let Value::Object(inner_map) = inner {
                        if inner_map.is_empty() {
                            map.remove(remaining[0].as_str());
                        }
                    }
                }
                result
            }
            _ => None,
        }
    }
}

/// Coerce a VRL `Value` to a `Timestamp` if it's a string that can be
/// parsed as RFC 3339. This is needed because OTLP `AnyValue` has no native
/// timestamp type, so timestamps round-trip as strings.
fn coerce_to_timestamp(v: Value) -> Value {
    match &v {
        Value::Timestamp(_) => v,
        Value::Bytes(b) => {
            if let Ok(s) = std::str::from_utf8(b) {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                    return Value::Timestamp(dt.with_timezone(&chrono::Utc));
                }
            }
            v
        }
        _ => v,
    }
}

// -- OtelAttributes --

/// Return a full `Resource` proto with attributes reconstituted from
/// the internal `OtelAttributes`. Use at proto serialization boundaries.
pub fn resource_to_proto(resource: Option<&Resource>, attrs: &OtelAttributes) -> Option<Resource> {
    let has_resource = resource.is_some();
    let has_attrs = !attrs.is_empty();
    if !has_resource && !has_attrs {
        return None;
    }
    let mut r = resource.cloned().unwrap_or(Resource {
        attributes: Vec::new(),
        dropped_attributes_count: 0,
    });
    r.attributes = attrs.to_key_values();
    Some(r)
}

/// Return a full `InstrumentationScope` proto with attributes reconstituted.
pub fn scope_to_proto(scope: Option<&InstrumentationScope>, attrs: &OtelAttributes) -> Option<InstrumentationScope> {
    let has_scope = scope.is_some();
    let has_attrs = !attrs.is_empty();
    if !has_scope && !has_attrs {
        return None;
    }
    let mut s = scope.cloned().unwrap_or_default();
    s.attributes = attrs.to_key_values();
    Some(s)
}

fn append_canonical_resource_scope(
    map: &mut ObjectMap,
    resource: Option<&Resource>,
    resource_attrs: &OtelAttributes,
    scope: Option<&InstrumentationScope>,
    scope_attrs: &OtelAttributes,
) {
    {
        let mut res_map = resource_attrs.to_object_map();
        if let Some(ref res) = resource {
            if res.dropped_attributes_count != 0 {
                res_map.insert(
                    "dropped_attributes_count".into(),
                    Value::Integer(res.dropped_attributes_count as i64),
                );
            }
        }
        if !res_map.is_empty() {
            map.insert("resource".into(), Value::Object(res_map));
        }
    }
    {
        let mut scope_map = ObjectMap::new();
        if let Some(ref s) = scope {
            if !s.name.is_empty() {
                scope_map.insert("name".into(), Value::Bytes(s.name.clone().into()));
            }
            if !s.version.is_empty() {
                scope_map.insert("version".into(), Value::Bytes(s.version.clone().into()));
            }
        }
        if !scope_attrs.is_empty() {
            scope_map.insert(
                "attributes".into(),
                Value::Object(scope_attrs.to_object_map()),
            );
        }
        if !scope_map.is_empty() {
            map.insert("scope".into(), Value::Object(scope_map));
        }
    }
}

fn remove_resource_subpath(
    resource_attrs: &mut OtelAttributes,
    remaining: &[String],
    prune: bool,
) -> Option<Value> {
    if remaining.len() == 1 {
        resource_attrs.remove(remaining[0].as_str())
            .map(|av| any_value_to_vrl(&av))
    } else {
        let key = remaining[0].as_str();
        let av = resource_attrs.get(key)?;
        let mut v = any_value_to_vrl(av);
        let result = remove_value_at(&mut v, &remaining[1..], prune);
        resource_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
        result
    }
}

fn remove_scope_subpath(
    scope: Option<&mut InstrumentationScope>,
    scope_attrs: &mut OtelAttributes,
    remaining: &[String],
    prune: bool,
) -> Option<Value> {
    let scope = scope?;
    match remaining[0].as_str() {
        "name" if remaining.len() == 1 => {
            if scope.name.is_empty() { return None; }
            let old = Some(Value::Bytes(scope.name.clone().into()));
            scope.name = String::new();
            old
        }
        "version" if remaining.len() == 1 => {
            if scope.version.is_empty() { return None; }
            let old = Some(Value::Bytes(scope.version.clone().into()));
            scope.version = String::new();
            old
        }
        "attributes" => {
            if remaining.len() == 1 {
                if scope_attrs.is_empty() { return None; }
                let old = Some(Value::Object(scope_attrs.to_object_map()));
                *scope_attrs = OtelAttributes::new();
                old
            } else if remaining.len() == 2 {
                scope_attrs.remove(remaining[1].as_str())
                    .map(|av| any_value_to_vrl(&av))
            } else {
                let key = remaining[1].as_str();
                let av = scope_attrs.get(key)?;
                let mut v = any_value_to_vrl(av);
                let result = remove_value_at(&mut v, &remaining[2..], prune);
                scope_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
                result
            }
        }
        _ => None,
    }
}

fn remove_attrs_subpath(
    attrs: &mut OtelAttributes,
    first: &str,
    remaining: &[String],
    prune: bool,
) -> Option<Value> {
    let av = attrs.get(first)?;
    let mut v = any_value_to_vrl(av);
    let result = remove_value_at(&mut v, remaining, prune);
    if result.is_some() {
        if prune {
            if let Value::Object(ref map) = v {
                if map.is_empty() {
                    attrs.remove(first);
                    return result;
                }
            }
        }
        attrs.insert(first.to_string(), vrl_value_to_any_value(&v));
    }
    result
}

/// BTreeMap-backed attribute container for O(log n) lookup.
/// Converts to/from `Vec<KeyValue>` at proto serialization boundaries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OtelAttributes {
    inner: BTreeMap<String, AnyValue>,
}

impl OtelAttributes {
    pub fn new() -> Self {
        Self { inner: BTreeMap::new() }
    }

    pub fn get(&self, key: &str) -> Option<&AnyValue> {
        self.inner.get(key)
    }

    pub fn insert(&mut self, key: String, value: AnyValue) -> Option<AnyValue> {
        self.inner.insert(key, value)
    }

    pub fn remove(&mut self, key: &str) -> Option<AnyValue> {
        self.inner.remove(key)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &AnyValue)> {
        self.inner.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.inner.keys()
    }

    /// Convert from proto `Vec<KeyValue>` (at source ingestion boundary).
    /// Duplicate keys are merged into an ArrayValue.
    pub fn from_key_values(kvs: Vec<KeyValue>) -> Self {
        let mut inner = BTreeMap::new();
        for kv in kvs {
            let val = kv.value.unwrap_or(AnyValue { value: None });
            match inner.entry(kv.key) {
                std::collections::btree_map::Entry::Vacant(e) => { e.insert(val); }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let existing = e.get_mut();
                    match &mut existing.value {
                        Some(OtelValueKind::ArrayValue(arr)) => {
                            arr.values.push(val);
                        }
                        _ => {
                            let old = std::mem::take(existing);
                            *existing = AnyValue {
                                value: Some(OtelValueKind::ArrayValue(
                                    opentelemetry_proto::tonic::common::v1::ArrayValue {
                                        values: vec![old, val],
                                    }
                                )),
                            };
                        }
                    }
                }
            }
        }
        Self { inner }
    }

    /// Convert to proto `Vec<KeyValue>` (at sink egress boundary).
    pub fn to_key_values(&self) -> Vec<KeyValue> {
        self.inner.iter()
            .map(|(k, v)| {
                let value = if v.value.is_none() { None } else { Some(v.clone()) };
                KeyValue { key: k.clone(), value }
            })
            .collect()
    }

    /// Convert from VRL `ObjectMap` (at deserialization boundary).
    pub fn from_object_map(map: &ObjectMap) -> Self {
        let inner = map.iter()
            .map(|(k, v)| (k.to_string(), vrl_value_to_any_value(v)))
            .collect();
        Self { inner }
    }

    /// Convert to VRL `ObjectMap` for canonical value representation.
    pub fn to_object_map(&self) -> ObjectMap {
        self.inner.iter()
            .map(|(k, v)| (KeyString::from(k.clone()), any_value_to_vrl(v)))
            .collect()
    }
}

impl From<Vec<KeyValue>> for OtelAttributes {
    fn from(kvs: Vec<KeyValue>) -> Self {
        Self::from_key_values(kvs)
    }
}

// -- OtelLog --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelLog {
    pub(crate) record: LogRecord,
    pub(crate) record_attrs: OtelAttributes,
    pub(crate) resource: Option<Resource>,
    pub(crate) resource_attrs: OtelAttributes,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) scope_attrs: OtelAttributes,
    pub(crate) metadata: EventMetadata,
}

impl OtelLog {
    pub fn new(mut record: LogRecord) -> Self {
        let record_attrs = OtelAttributes::from_key_values(std::mem::take(&mut record.attributes));
        Self {
            record,
            record_attrs,
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata: EventMetadata::default(),
        }
    }

    /// Create an `OtelLog` from raw bytes, setting `record.body` to a string value.
    pub fn from_bytes(bytes: bytes::Bytes) -> Self {
        let body_value = match std::str::from_utf8(&bytes) {
            Ok(s) => OtelValueKind::StringValue(s.to_owned()),
            Err(_) => OtelValueKind::BytesValue(bytes.to_vec()),
        };
        Self {
            record: LogRecord {
                body: Some(AnyValue {
                    value: Some(body_value),
                }),
                ..Default::default()
            },
            record_attrs: OtelAttributes::new(),
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata: EventMetadata::default(),
        }
    }

    /// Create an `OtelLog` from a JSON value.
    /// Objects are routed through `from_value_map` so canonical keys (`body`,
    /// `time_unix_nano`, `resource`, etc.) are restored to their proto slots.
    /// Scalars/arrays become the record body directly.
    pub fn from_json_value(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Object(map) => {
                let vrl_map: ObjectMap = map
                    .into_iter()
                    .map(|(k, v)| (KeyString::from(k), json_to_vrl_value(v)))
                    .collect();
                Self::from_value_map(Value::Object(vrl_map), EventMetadata::default())
            }
            other => {
                let body = json_to_any_value(other);
                Self {
                    record: LogRecord {
                        body: Some(body),
                        ..Default::default()
                    },
                    record_attrs: OtelAttributes::new(),
                    resource: None,
                    resource_attrs: OtelAttributes::new(),
                    scope: None,
                    scope_attrs: OtelAttributes::new(),
                    metadata: EventMetadata::default(),
                }
            }
        }
    }

    pub fn from_parts(
        mut record: LogRecord,
        mut resource: Option<Resource>,
        mut scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        let record_attrs = OtelAttributes::from_key_values(std::mem::take(&mut record.attributes));
        let resource_attrs = resource.as_mut()
            .map(|r| OtelAttributes::from_key_values(std::mem::take(&mut r.attributes)))
            .unwrap_or_default();
        let scope_attrs = scope.as_mut()
            .map(|s| OtelAttributes::from_key_values(std::mem::take(&mut s.attributes)))
            .unwrap_or_default();
        Self {
            record,
            record_attrs,
            resource,
            resource_attrs,
            scope,
            scope_attrs,
            metadata,
        }
    }

    /// Build an `OtelLog` from a `tracing::Event` — the same semantics as
    /// Converts a `tracing::Event` directly into an `OtelLog`.
    /// Accumulates fields via the `tracing::field::Visit`
    /// trait into a single `ObjectMap`, then converts to `OtelLog` once
    /// to amortize the legacy-layout round-trip.
    pub fn from_tracing_event(event: &tracing::Event<'_>) -> Self {
        let mut builder = OtelLogTracingBuilder::default();
        event.record(&mut builder);

        let meta = event.metadata();
        let now_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0) as u64;
        builder.map.insert(
            "time_unix_nano".into(),
            Value::Integer(now_nanos as i64),
        );
        let kind_value = if meta.is_event() {
            Value::Bytes("event".to_string().into())
        } else if meta.is_span() {
            Value::Bytes("span".to_string().into())
        } else {
            Value::Null
        };
        let mut metadata_map = ObjectMap::new();
        metadata_map.insert("kind".into(), kind_value);
        metadata_map.insert("level".into(), Value::Bytes(meta.level().to_string().into()));
        metadata_map.insert(
            "module_path".into(),
            meta.module_path()
                .map_or(Value::Null, |mp| Value::Bytes(mp.to_string().into())),
        );
        metadata_map.insert(
            "target".into(),
            Value::Bytes(meta.target().to_string().into()),
        );
        builder.map.insert("metadata".into(), Value::Object(metadata_map));

        OtelLog::from_value_map(Value::Object(builder.map), EventMetadata::default())
    }

    pub fn into_parts(
        mut self,
    ) -> (
        LogRecord,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        self.record.attributes = self.record_attrs.to_key_values();
        let resource = self.resource.map(|mut r| {
            r.attributes = self.resource_attrs.to_key_values();
            r
        });
        let scope = self.scope.map(|mut s| {
            s.attributes = self.scope_attrs.to_key_values();
            s
        });
        (self.record, resource, scope, self.metadata)
    }

    pub fn record(&self) -> &LogRecord {
        &self.record
    }

    /// Return a full `LogRecord` with `attributes` populated from the
    /// internal `OtelAttributes` map.  Use at proto serialization boundaries
    /// (gRPC sink, buffer codec, OTLP codec) where the downstream expects
    /// a complete proto message.
    pub fn record_to_proto(&self) -> LogRecord {
        let mut r = self.record.clone();
        r.attributes = self.record_attrs.to_key_values();
        r
    }

    pub fn record_mut(&mut self) -> &mut LogRecord {
        &mut self.record
    }

    /// Return a reference to the `OtelAttributes` for record-level attributes.
    pub fn record_attributes(&self) -> &OtelAttributes {
        &self.record_attrs
    }

    pub fn resource(&self) -> Option<&Resource> {
        self.resource.as_ref()
    }

    pub fn resource_proto(&self) -> Option<Resource> {
        resource_to_proto(self.resource.as_ref(), &self.resource_attrs)
    }

    pub fn scope_proto(&self) -> Option<InstrumentationScope> {
        scope_to_proto(self.scope.as_ref(), &self.scope_attrs)
    }

    pub fn set_resource(&mut self, mut resource: Resource) {
        self.resource_attrs = OtelAttributes::from_key_values(std::mem::take(&mut resource.attributes));
        self.resource = Some(resource);
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, mut scope: InstrumentationScope) {
        self.scope_attrs = OtelAttributes::from_key_values(std::mem::take(&mut scope.attributes));
        self.scope = Some(scope);
    }

    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        &mut self.metadata
    }

    pub fn body(&self) -> Option<&AnyValue> {
        self.record.body.as_ref()
    }

    /// Return the body as a human-readable string, suitable for text-oriented
    /// serializers (text, raw_message, etc.).
    pub fn body_string(&self) -> String {
        match self.record.body.as_ref().and_then(|av| av.value.as_ref()) {
            Some(OtelValueKind::StringValue(s)) => s.clone(),
            Some(OtelValueKind::BoolValue(b)) => b.to_string(),
            Some(OtelValueKind::IntValue(i)) => i.to_string(),
            Some(OtelValueKind::DoubleValue(d)) => d.to_string(),
            Some(OtelValueKind::BytesValue(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(OtelValueKind::ArrayValue(a)) => format!("{a:?}"),
            Some(OtelValueKind::KvlistValue(kv)) => format!("{kv:?}"),
            None => String::new(),
        }
    }

    pub fn set_body(&mut self, value: AnyValue) {
        self.record.body = Some(value);
    }

    pub fn time_unix_nano(&self) -> u64 {
        self.record.time_unix_nano
    }

    pub fn observed_time_unix_nano(&self) -> u64 {
        self.record.observed_time_unix_nano
    }

    pub fn severity_number(&self) -> i32 {
        self.record.severity_number
    }

    pub fn severity_text(&self) -> &str {
        &self.record.severity_text
    }

    pub fn trace_id(&self) -> &[u8] {
        &self.record.trace_id
    }

    pub fn span_id(&self) -> &[u8] {
        &self.record.span_id
    }

    /// Merge another OtelLog's body into this one (concatenate string bodies).
    /// Used for partial line merging (e.g. Docker logs).
    /// Also merges metadata.
    pub fn merge_body(&mut self, incoming: &OtelLog) {
        if let (Some(self_body), Some(inc_body)) = (self.body(), incoming.body()) {
            if let (Some(OtelValueKind::StringValue(self_s)), Some(OtelValueKind::StringValue(inc_s)))
                = (&self_body.value, &inc_body.value)
            {
                let merged = format!("{}{}", self_s, inc_s);
                self.set_body(AnyValue {
                    value: Some(OtelValueKind::StringValue(merged)),
                });
            }
        } else if self.body().is_none() {
            if let Some(inc_body) = incoming.body() {
                self.set_body(inc_body.clone());
            }
        }
        self.metadata.merge(incoming.metadata.clone());
    }

    pub fn attribute(&self, key: &str) -> Option<&AnyValue> {
        self.record_attrs.get(key)
    }

    /// Returns `true` if the body is a KvList that contains `key`, or if `key`
    /// exists as a record attribute.  This is the appropriate check before
    /// writing a header value so that body fields always take precedence.
    pub fn has_field(&self, key: &str) -> bool {
        if self.attribute(key).is_some() {
            return true;
        }
        if let Some(body) = self.body() {
            if let Some(OtelValueKind::KvlistValue(kvl)) = &body.value {
                return kvl.values.iter().any(|kv| kv.key == key);
            }
        }
        false
    }

    pub fn set_attribute(&mut self, key: String, value: AnyValue) {
        self.record_attrs.insert(key, value);
    }

    pub fn remove_attribute(&mut self, key: &str) -> Option<AnyValue> {
        self.record_attrs.remove(key)
    }

    pub fn attributes(&self) -> &OtelAttributes {
        &self.record_attrs
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource_attrs.get(key)
    }

    pub fn resource_attrs(&self) -> &OtelAttributes {
        &self.resource_attrs
    }

    pub fn scope_attrs(&self) -> &OtelAttributes {
        &self.scope_attrs
    }

    /// Ensure the resource object exists, creating it if absent.
    fn ensure_resource(&mut self) {
        if self.resource.is_none() {
            self.resource = Some(Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            });
        }
    }

    /// Set a resource attribute (e.g. `host.name`, `source_type`).
    pub fn set_resource_attribute(&mut self, key: String, value: AnyValue) {
        self.ensure_resource();
        self.resource_attrs.insert(key, value);
    }

    /// Set the observed_time_unix_nano (ingest timestamp) from a chrono DateTime.
    pub fn set_observed_timestamp(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.record.observed_time_unix_nano =
            now.timestamp_nanos_opt().unwrap_or(0) as u64;
    }

    /// Set source metadata: source_type (resource attribute) and observed_time_unix_nano.
    pub fn set_source_metadata(
        &mut self,
        source_name: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        self.set_resource_attribute("source_type".to_string(), string_value(source_name));
        self.set_observed_timestamp(now);
    }

    /// Set source metadata for Vector namespace: populates both OtelLog
    /// fields and `%vector.*` metadata entries for backward compatibility.
    pub fn set_source_metadata_vector_ns(
        &mut self,
        source_name: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        self.set_source_metadata(source_name, now);
        self.metadata
            .value_mut()
            .insert(lookup::path!("vector", "source_type"), source_name.to_owned());
        self.metadata
            .value_mut()
            .insert(lookup::path!("vector", "ingest_timestamp"), Value::Timestamp(now));
    }

    pub fn add_finalizer(&mut self, finalizer: EventFinalizer) {
        self.metadata.add_finalizer(finalizer);
    }

    #[must_use]
    pub fn with_batch_notifier(mut self, batch: &BatchNotifier) -> Self {
        self.metadata = self.metadata.with_batch_notifier(batch);
        self
    }

    #[must_use]
    pub fn with_batch_notifier_option(mut self, batch: &Option<BatchNotifier>) -> Self {
        self.metadata = self.metadata.with_batch_notifier_option(batch);
        self
    }

    // -----------------------------------------------------------------------
    // Field access methods
    //
    // Single-segment and multi-segment field paths use direct proto
    // accessors. Array-index paths fall back to `to_value_canonical()`.
    // -----------------------------------------------------------------------

    /// Get a field value by its semantic meaning (looks up schema definition).
    pub fn get_by_meaning(&self, meaning: impl AsRef<str>) -> Option<Value> {
        self.metadata
            .dropped_field(&meaning)
            .cloned()
            .or_else(|| {
                self.metadata
                    .schema_definition()
                    .meaning_path(meaning.as_ref())
                    .and_then(|path| self.get(path))
            })
    }

    /// Find the path for a field by its semantic meaning.
    pub fn find_key_by_meaning(&self, meaning: impl AsRef<str>) -> Option<&vrl::path::OwnedTargetPath> {
        self.metadata.schema_definition().meaning_path(meaning.as_ref())
    }

    /// Get a field value by path.
    /// Fast path for well-known single-segment proto fields; falls back to legacy layout.
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.get_single_segment(first.as_ref()),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.get_field_path(&fields)
                                } else {
                                    let value = self.to_value_canonical();
                                    value.get(path.value_path()).cloned()
                                }
                            }
                            _ => {
                                let value = self.to_value_canonical();
                                value.get(path.value_path()).cloned()
                            }
                        }
                    }
                    _ => {
                        let value = self.to_value_canonical();
                        value.get(path.value_path()).cloned()
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value().get(path.value_path()).cloned()
            }
        }
    }

    fn get_single_segment(&self, field: &str) -> Option<Value> {
        match field {
            "body" => {
                self.body().map(any_value_to_vrl)
            }
            "severity_text" if !self.record.severity_text.is_empty() => {
                Some(Value::Bytes(self.record.severity_text.clone().into()))
            }
            "severity_number" if self.record.severity_number != 0 => {
                Some(Value::Integer(self.record.severity_number as i64))
            }
            "trace_id" if !self.record.trace_id.is_empty() => {
                Some(hex_encode(&self.record.trace_id))
            }
            "span_id" if !self.record.span_id.is_empty() => {
                Some(hex_encode(&self.record.span_id))
            }
            "time_unix_nano" if self.record.time_unix_nano != 0 => {
                Some(Value::Integer(self.record.time_unix_nano as i64))
            }
            "observed_time_unix_nano" if self.record.observed_time_unix_nano != 0 => {
                Some(Value::Integer(self.record.observed_time_unix_nano as i64))
            }
            other => {
                self.record_attrs.get(other)
                    .map(any_value_to_vrl)
            }
        }
    }

    fn get_field_path(&self, fields: &[String]) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => {
                let remaining = &fields[1..];
                if remaining.len() == 1 {
                    let key = remaining[0].as_str();
                    if key == "dropped_attributes_count" {
                        let res = self.resource.as_ref()?;
                        if res.dropped_attributes_count != 0 {
                            return Some(Value::Integer(res.dropped_attributes_count as i64));
                        }
                        return None;
                    }
                    self.resource_attrs.get(key).map(any_value_to_vrl)
                } else {
                    let key = remaining[0].as_str();
                    let av = self.resource_attrs.get(key)?;
                    let v = any_value_to_vrl(av);
                    navigate_value(&v, &remaining[1..])
                }
            }
            "scope" => {
                let scope = self.scope.as_ref()?;
                let remaining = &fields[1..];
                match remaining[0].as_str() {
                    "name" if remaining.len() == 1 => {
                        if scope.name.is_empty() { None }
                        else { Some(Value::Bytes(scope.name.clone().into())) }
                    }
                    "version" if remaining.len() == 1 => {
                        if scope.version.is_empty() { None }
                        else { Some(Value::Bytes(scope.version.clone().into())) }
                    }
                    "attributes" => {
                        if remaining.len() == 1 {
                            if self.scope_attrs.is_empty() { None }
                            else { Some(Value::Object(self.scope_attrs.to_object_map())) }
                        } else {
                            let av = self.scope_attrs.get(&remaining[1])?;
                            if remaining.len() == 2 {
                                Some(any_value_to_vrl(av))
                            } else {
                                let v = any_value_to_vrl(av);
                                navigate_value(&v, &remaining[2..])
                            }
                        }
                    }
                    _ => None,
                }
            }
            first => {
                let av = self.record_attrs.get(first)?;
                let v = any_value_to_vrl(av);
                navigate_value(&v, &fields[1..])
            }
        }
    }

    /// Insert a field value by path.
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let value = value.into();
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.insert_single_segment(first.as_ref(), value),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.insert_field_path(&fields, value)
                                } else {
                                    let mut val = self.to_value_canonical();
                                    let old = val.insert(path.value_path(), value);
                                    self.apply_value_map(val);
                                    old
                                }
                            }
                            _ => {
                                let mut val = self.to_value_canonical();
                                let old = val.insert(path.value_path(), value);
                                self.apply_value_map(val);
                                old
                            }
                        }
                    }
                    _ => {
                        let mut val = self.to_value_canonical();
                        let old = val.insert(path.value_path(), value);
                        self.apply_value_map(val);
                        old
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().insert(path.value_path(), value)
            }
        }
    }

    fn insert_single_segment(&mut self, field: &str, value: Value) -> Option<Value> {
        match field {
            "body" => {
                let old = self.body().map(any_value_to_vrl);
                self.record_mut().body = Some(vrl_value_to_any_value(&value));
                old
            }
            "severity_text" => {
                let old = if self.record.severity_text.is_empty() { None }
                    else { Some(Value::Bytes(self.record.severity_text.clone().into())) };
                self.record_mut().severity_text = value.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| value.to_string_lossy().into_owned());
                old
            }
            "severity_number" => {
                let old = if self.record.severity_number == 0 { None }
                    else { Some(Value::Integer(self.record.severity_number as i64)) };
                if let Some(n) = value.as_integer() {
                    self.record_mut().severity_number = n as i32;
                }
                old
            }
            "trace_id" => {
                let old = if self.record.trace_id.is_empty() { None }
                    else { Some(hex_encode(&self.record.trace_id)) };
                if let Some(decoded) = hex_decode(&value) {
                    self.record_mut().trace_id = decoded;
                } else {
                    // Malformed hex: store as attribute so data isn't lost
                    self.record_attrs.insert(
                        "trace_id".to_string(),
                        vrl_value_to_any_value(&value),
                    );
                }
                old
            }
            "span_id" => {
                let old = if self.record.span_id.is_empty() { None }
                    else { Some(hex_encode(&self.record.span_id)) };
                if let Some(decoded) = hex_decode(&value) {
                    self.record_mut().span_id = decoded;
                } else {
                    self.record_attrs.insert(
                        "span_id".to_string(),
                        vrl_value_to_any_value(&value),
                    );
                }
                old
            }
            "time_unix_nano" => {
                let old = if self.record.time_unix_nano == 0 { None }
                    else { Some(Value::Integer(self.record.time_unix_nano as i64)) };
                match &value {
                    Value::Integer(n) => {
                        self.record_mut().time_unix_nano = *n as u64;
                    }
                    Value::Timestamp(ts) => {
                        self.record_mut().time_unix_nano =
                            ts.timestamp_nanos_opt().unwrap_or(0) as u64;
                    }
                    _ => {
                        self.record_attrs.insert(
                            "time_unix_nano".to_string(),
                            vrl_value_to_any_value(&value),
                        );
                    }
                }
                old
            }
            "observed_time_unix_nano" => {
                let old = if self.record.observed_time_unix_nano == 0 { None }
                    else { Some(Value::Integer(self.record.observed_time_unix_nano as i64)) };
                match &value {
                    Value::Integer(n) => {
                        self.record_mut().observed_time_unix_nano = *n as u64;
                    }
                    Value::Timestamp(ts) => {
                        self.record_mut().observed_time_unix_nano =
                            ts.timestamp_nanos_opt().unwrap_or(0) as u64;
                    }
                    _ => {}
                }
                old
            }
            other => {
                // Generic: upsert into record attributes
                let old = self.record_attrs.get(other)
                    .map(any_value_to_vrl);
                self.record_attrs.insert(
                    other.to_string(),
                    vrl_value_to_any_value(&value),
                );
                old
            }
        }
    }

    fn insert_field_path(&mut self, fields: &[String], value: Value) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => {
                self.ensure_resource();
                let remaining = &fields[1..];
                if remaining.len() == 1 {
                    let key = remaining[0].as_str();
                    let old = self.resource_attrs.get(key).map(any_value_to_vrl);
                    self.resource_attrs.insert(key.to_string(), vrl_value_to_any_value(&value));
                    old
                } else {
                    let key = remaining[0].as_str();
                    let mut v = self.resource_attrs.get(key)
                        .map(any_value_to_vrl)
                        .unwrap_or(Value::Object(ObjectMap::new()));
                    let old = insert_value_at(&mut v, &remaining[1..], value);
                    self.resource_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
                    old
                }
            }
            "scope" => {
                let scope = self.scope.get_or_insert_with(InstrumentationScope::default);
                let remaining = &fields[1..];
                match remaining[0].as_str() {
                    "name" if remaining.len() == 1 => {
                        let old = if scope.name.is_empty() { None }
                            else { Some(Value::Bytes(scope.name.clone().into())) };
                        scope.name = value.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| value.to_string_lossy().into_owned());
                        old
                    }
                    "version" if remaining.len() == 1 => {
                        let old = if scope.version.is_empty() { None }
                            else { Some(Value::Bytes(scope.version.clone().into())) };
                        scope.version = value.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| value.to_string_lossy().into_owned());
                        old
                    }
                    "attributes" => {
                        if remaining.len() == 1 {
                            let old = if self.scope_attrs.is_empty() { None }
                                else { Some(Value::Object(self.scope_attrs.to_object_map())) };
                            if let Value::Object(map) = &value {
                                let mut new_attrs = OtelAttributes::new();
                                for (k, v) in map.iter() {
                                    new_attrs.insert(k.to_string(), vrl_value_to_any_value(v));
                                }
                                self.scope_attrs = new_attrs;
                            }
                            old
                        } else {
                            let key = remaining[1].as_str();
                            if remaining.len() == 2 {
                                let old = self.scope_attrs.get(key).map(any_value_to_vrl);
                                self.scope_attrs.insert(key.to_string(), vrl_value_to_any_value(&value));
                                old
                            } else {
                                let mut v = self.scope_attrs.get(key)
                                    .map(any_value_to_vrl)
                                    .unwrap_or(Value::Object(ObjectMap::new()));
                                let old = insert_value_at(&mut v, &remaining[2..], value);
                                self.scope_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
                                old
                            }
                        }
                    }
                    _ => None,
                }
            }
            first => {
                let mut v = self.record_attrs.get(first)
                    .map(any_value_to_vrl)
                    .unwrap_or(Value::Object(ObjectMap::new()));
                let old = insert_value_at(&mut v, &fields[1..], value);
                self.record_attrs.insert(
                    first.to_string(),
                    vrl_value_to_any_value(&v),
                );
                old
            }
        }
    }

    /// Remove a field value by path.
    pub fn remove<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
    ) -> Option<Value> {
        self.remove_prune(path, false)
    }

    /// Remove at `path`. If `prune` is true, empty parent objects along
    /// the path are also removed.
    pub fn remove_prune<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        prune: bool,
    ) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.remove_single_segment(first.as_ref()),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.remove_field_path(&fields, prune)
                                } else {
                                    let mut val = self.to_value_canonical();
                                    let old = val.remove(path.value_path(), prune);
                                    self.apply_value_map(val);
                                    old
                                }
                            }
                            _ => {
                                let mut val = self.to_value_canonical();
                                let old = val.remove(path.value_path(), prune);
                                self.apply_value_map(val);
                                old
                            }
                        }
                    }
                    _ => {
                        let mut val = self.to_value_canonical();
                        let old = val.remove(path.value_path(), prune);
                        self.apply_value_map(val);
                        old
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().remove(path.value_path(), prune)
            }
        }
    }

    fn remove_single_segment(&mut self, field: &str) -> Option<Value> {
        match field {
            "body" => {
                let old = self.body().map(any_value_to_vrl);
                self.record_mut().body = None;
                old
            }
            "severity_text" => {
                if self.record.severity_text.is_empty() { return None; }
                let old = Some(Value::Bytes(self.record.severity_text.clone().into()));
                self.record_mut().severity_text = String::new();
                old
            }
            "severity_number" => {
                if self.record.severity_number == 0 { return None; }
                let old = Some(Value::Integer(self.record.severity_number as i64));
                self.record_mut().severity_number = 0;
                old
            }
            "trace_id" => {
                if self.record.trace_id.is_empty() { return None; }
                let old = Some(hex_encode(&self.record.trace_id));
                self.record_mut().trace_id.clear();
                old
            }
            "span_id" => {
                if self.record.span_id.is_empty() { return None; }
                let old = Some(hex_encode(&self.record.span_id));
                self.record_mut().span_id.clear();
                old
            }
            "time_unix_nano" => {
                if self.record.time_unix_nano == 0 { return None; }
                let old = Some(Value::Integer(self.record.time_unix_nano as i64));
                self.record_mut().time_unix_nano = 0;
                old
            }
            "observed_time_unix_nano" => {
                if self.record.observed_time_unix_nano == 0 { return None; }
                let old = Some(Value::Integer(self.record.observed_time_unix_nano as i64));
                self.record_mut().observed_time_unix_nano = 0;
                old
            }
            other => {
                self.record_attrs.remove(other)
                    .map(|av| any_value_to_vrl(&av))
            }
        }
    }

    fn remove_field_path(&mut self, fields: &[String], prune: bool) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => remove_resource_subpath(
                &mut self.resource_attrs, &fields[1..], prune,
            ),
            "scope" => remove_scope_subpath(
                self.scope.as_mut(), &mut self.scope_attrs, &fields[1..], prune,
            ),
            first => remove_attrs_subpath(
                &mut self.record_attrs, first, &fields[1..], prune,
            ),
        }
    }

    /// Insert at `path` only if `path` is Some. Convenience wrapper around
    /// `insert` that only inserts when the path is `Some`.
    pub fn maybe_insert<'a>(
        &mut self,
        path: Option<impl lookup::lookup_v2::TargetPath<'a>>,
        value: impl Into<Value>,
    ) {
        if let Some(path) = path {
            self.insert(path, value);
        }
    }

    /// Merge fields from `incoming` into this log, concatenating byte
    /// values for specified fields. Merges by concatenating string values for specified fields.
    pub fn merge(&mut self, mut incoming: OtelLog, fields: &[impl AsRef<str>]) {
        for field in fields {
            let field_path = vrl::event_path!(field.as_ref());
            let Some(incoming_val) = incoming.remove(field_path) else {
                continue;
            };
            match self.get(field_path) {
                None => {
                    self.insert(field_path, incoming_val);
                }
                Some(mut current_val) => {
                    current_val.merge(incoming_val);
                    self.insert(field_path, current_val);
                }
            }
        }
        self.metadata.merge(incoming.metadata);
    }

    /// Build a Value tree with the legacy layout — no intermediate conversion.
    /// This ensures callers see the same field names and types.
    pub fn to_value_canonical(&self) -> Value {
        let mut map = ObjectMap::new();

        if let Some(body) = self.body() {
            map.insert("body".into(), any_value_to_vrl(body));
        }
        if !self.record.severity_text.is_empty() {
            map.insert("severity_text".into(), Value::Bytes(self.record.severity_text.clone().into()));
        }
        if self.record.severity_number != 0 {
            map.insert("severity_number".into(), Value::Integer(self.record.severity_number as i64));
        }
        if self.record.time_unix_nano != 0 {
            map.insert("time_unix_nano".into(), Value::Integer(self.record.time_unix_nano as i64));
        }
        if self.record.observed_time_unix_nano != 0 {
            map.insert("observed_time_unix_nano".into(), Value::Integer(self.record.observed_time_unix_nano as i64));
        }
        if !self.record.trace_id.is_empty() {
            map.insert("trace_id".into(), hex_encode(&self.record.trace_id));
        }
        if !self.record.span_id.is_empty() {
            map.insert("span_id".into(), hex_encode(&self.record.span_id));
        }
        if self.record.flags != 0 {
            map.insert("flags".into(), Value::Integer(i64::from(self.record.flags)));
        }
        if self.record.dropped_attributes_count != 0 {
            map.insert("dropped_attributes_count".into(), Value::Integer(i64::from(self.record.dropped_attributes_count)));
        }
        if !self.record_attrs.is_empty() {
            for (k, v) in self.record_attrs.iter() {
                let key = KeyString::from(k.clone());
                if !map.contains_key(&key) {
                    map.insert(key, any_value_to_vrl(v));
                }
            }
        }
        append_canonical_resource_scope(
            &mut map,
            self.resource.as_ref(),
            &self.resource_attrs,
            self.scope.as_ref(),
            &self.scope_attrs,
        );

        Value::Object(map)
    }

    /// Construct an OtelLog from a legacy-layout Value + metadata.
    /// Routes fields into OTel structure: body, timestamp, source_type/host
    /// → resource attrs, everything else → record.attributes. Clears scope.
    pub fn from_value_map(value: Value, metadata: EventMetadata) -> Self {
        let mut out = Self {
            record: LogRecord::default(),
            record_attrs: OtelAttributes::new(),
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata,
        };
        out.apply_value_map(value);
        out
    }

    /// Extract a nanoseconds-timestamp field from the map, accepting both
    /// snake_case and camelCase names, and both integer and string-encoded values.
    fn extract_nanos_field(
        map: &mut ObjectMap,
        snake_name: &str,
        camel_name: &str,
    ) -> u64 {
        let val = map.remove(snake_name)
            .or_else(|| map.remove(camel_name));
        match val {
            Some(Value::Integer(n)) => n as u64,
            Some(Value::Bytes(b)) => {
                // OTLP JSON encodes nano timestamps as strings
                std::str::from_utf8(&b)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_else(|| {
                        map.insert(snake_name.into(), Value::Bytes(b));
                        0
                    })
            }
            Some(other) => { map.insert(snake_name.into(), other); 0 }
            None => 0,
        }
    }

    /// Write back a Value tree to proto fields.
    ///
    /// Accepts canonical keys (`body`/`message`, `time_unix_nano` as Integer)
    /// as well as OTLP JSON camelCase keys (`timeUnixNano`, `severityText`, etc.).
    /// Proto fields are extracted into their native slots;
    /// `resource`/`scope` sub-objects are restored;
    /// the remainder becomes `record.attributes`.
    fn apply_value_map(&mut self, value: Value) {
        let mut map = match value {
            Value::Object(m) => m,
            other => {
                self.record = LogRecord {
                    body: Some(vrl_value_to_any_value(&other)),
                    ..Default::default()
                };
                self.record_attrs = OtelAttributes::new();
                self.resource = None;
                self.resource_attrs = OtelAttributes::new();
                self.scope = None;
                self.scope_attrs = OtelAttributes::new();
                return;
            }
        };

        // Handle body: try OTLP AnyValue pattern first (e.g. {"stringValue":"hello"})
        let body = map.remove("body")
            .map(|v| {
                try_parse_otlp_any_value(&v)
                    .unwrap_or_else(|| vrl_value_to_any_value(&v))
            });

        // time_unix_nano: accept snake_case or camelCase, integer or string-encoded
        let time_unix_nano = Self::extract_nanos_field(&mut map, "time_unix_nano", "timeUnixNano");

        // observed_time_unix_nano: accept snake_case or camelCase
        let observed_time_unix_nano = Self::extract_nanos_field(
            &mut map,
            "observed_time_unix_nano",
            "observedTimeUnixNano",
        );

        // severity_text: accept snake_case or camelCase
        let severity_text = map.remove("severity_text")
            .or_else(|| map.remove("severityText"))
            .map(|v| match v {
                Value::Bytes(b) => String::from_utf8(b.to_vec()).unwrap_or_default(),
                other => { map.insert("severity_text".into(), other); String::new() }
            })
            .unwrap_or_default();

        // severity_number: accept snake_case or camelCase
        let severity_number = map.remove("severity_number")
            .or_else(|| map.remove("severityNumber"))
            .map(|v| match v {
                Value::Integer(i) => i as i32,
                other => { map.insert("severity_number".into(), other); 0 }
            })
            .unwrap_or(0);

        // trace_id: accept snake_case or camelCase
        let trace_id = map.remove("trace_id")
            .or_else(|| map.remove("traceId"))
            .map(|v| match hex_decode(&v) {
                Some(bytes) => bytes,
                None => { map.insert("trace_id".into(), v); Vec::new() }
            })
            .unwrap_or_default();

        // span_id: accept snake_case or camelCase
        let span_id = map.remove("span_id")
            .or_else(|| map.remove("spanId"))
            .map(|v| match hex_decode(&v) {
                Some(bytes) => bytes,
                None => { map.insert("span_id".into(), v); Vec::new() }
            })
            .unwrap_or_default();

        let flags = match map.remove("flags") {
            Some(Value::Integer(i)) => i as u32,
            Some(other) => { map.insert("flags".into(), other); 0 }
            None => 0,
        };
        let dropped_attributes_count = match map.remove("dropped_attributes_count") {
            Some(Value::Integer(i)) => i as u32,
            Some(other) => { map.insert("dropped_attributes_count".into(), other); 0 }
            None => 0,
        };

        let (resource, resource_attrs) = restore_resource(&mut map);
        self.resource = resource;
        self.resource_attrs = resource_attrs;
        let (scope, scope_attrs) = restore_scope(&mut map);
        self.scope = scope;
        self.scope_attrs = scope_attrs;

        // Handle OTLP JSON "attributes" array format:
        // [{"key":"k","value":{"stringValue":"v"}}, ...]
        let mut extra_attrs = OtelAttributes::new();
        if let Some(Value::Array(arr)) = map.remove("attributes") {
            if let Some(kvs) = parse_otlp_key_value_array(&arr) {
                for kv in kvs {
                    let av = kv.value.unwrap_or(AnyValue { value: None });
                    extra_attrs.insert(kv.key, av);
                }
            } else {
                // Not a valid OTLP attributes array, put it back
                map.insert("attributes".into(), Value::Array(arr));
            }
        }

        self.record_attrs = OtelAttributes {
            inner: map.into_iter()
                .map(|(k, v)| (k.to_string(), vrl_value_to_any_value(&v)))
                .collect(),
        };

        // Merge any attributes parsed from OTLP JSON format
        for (k, v) in extra_attrs.inner {
            self.record_attrs.insert(k, v);
        }

        self.record = LogRecord {
            body,
            time_unix_nano,
            observed_time_unix_nano,
            severity_text,
            severity_number,
            trace_id,
            span_id,
            flags,
            dropped_attributes_count,
            attributes: Vec::new(),
        };
    }

    /// Get the timestamp from the event.
    ///
    /// Prefers `time_unix_nano` (event time), falls back to
    /// `observed_time_unix_nano` (ingest time).
    pub fn get_timestamp(&self) -> Option<Value> {
        if let Some(ts) = self.get_by_meaning("timestamp") {
            return Some(coerce_to_timestamp(ts));
        }
        if self.record.time_unix_nano != 0 {
            let nanos = self.record.time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nsecs).map(Value::Timestamp)
        } else if self.record.observed_time_unix_nano != 0 {
            let nanos = self.record.observed_time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nsecs).map(Value::Timestamp)
        } else {
            None
        }
    }

    /// Remove the timestamp from the event.
    pub fn remove_timestamp(&mut self) -> Option<Value> {
        let ts = self.get_timestamp();
        self.record.time_unix_nano = 0;
        ts
    }

    /// Set the event timestamp (`time_unix_nano`) from a chrono DateTime.
    /// Returns the previous timestamp value, if any.
    pub fn set_timestamp(&mut self, ts: chrono::DateTime<chrono::Utc>) -> Option<Value> {
        let old = self.get_timestamp();
        self.record_mut().time_unix_nano =
            ts.timestamp_nanos_opt().unwrap_or(0) as u64;
        old
    }

    /// Set the event timestamp only if one is not already present.
    pub fn try_set_timestamp(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        if self.record.time_unix_nano == 0 {
            self.record_mut().time_unix_nano =
                ts.timestamp_nanos_opt().unwrap_or(0) as u64;
        }
    }

    /// Check if a field exists.
    pub fn contains<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> bool {
        self.get(path).is_some()
    }

    /// Get the log body value.
    pub fn get_body(&self) -> Option<Value> {
        self.body().map(any_value_to_vrl)
    }

    /// Get the "source_type" from resource attributes.
    pub fn get_source_type(&self) -> Option<Value> {
        self.resource_attribute("source_type")
            .map(any_value_to_vrl)
    }

    /// Set the source_type as a resource attribute.
    pub fn set_source_type(&mut self, value: impl Into<Value>) {
        self.set_resource_attribute(
            "source_type".to_string(),
            vrl_value_to_any_value(&value.into()),
        );
    }

    /// Set the source_type only if not already present.
    pub fn try_set_source_type(&mut self, value: impl Into<Value>) {
        if self.resource_attribute("source_type").is_none() {
            self.set_source_type(value);
        }
    }

    /// Get the host value from resource attributes.
    pub fn get_host(&self) -> Option<Value> {
        self.resource_attribute("host.name")
            .map(any_value_to_vrl)
    }

    /// Set the host as a resource attribute (`host.name`).
    pub fn set_host(&mut self, value: impl Into<Value>) {
        self.set_resource_attribute(
            "host.name".to_string(),
            vrl_value_to_any_value(&value.into()),
        );
    }

    /// Set the host only if not already present.
    pub fn try_set_host(&mut self, value: impl Into<Value>) {
        if self.resource_attribute("host.name").is_none() {
            self.set_host(value);
        }
    }

    /// Parse a path and get a value.
    pub fn parse_path_and_get_value(
        &self,
        path: &str,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let target_path = vrl::path::parse_target_path(path)?;
        Ok(self.get(&target_path))
    }

    /// Iterate all event fields (flattened, dotted keys). Owned values.
    /// Iterator over all event fields, returning owned values
    /// since OtelLog builds the field tree dynamically from proto.
    pub fn all_event_fields(&self) -> Option<Vec<(vrl::value::KeyString, Value)>> {
        let fields = self.convert_to_fields();
        if fields.is_empty() { None } else { Some(fields) }
    }

    /// Like `all_event_fields` but skips individual array elements.
    pub fn all_event_fields_skip_array_elements(&self) -> Option<Vec<(vrl::value::KeyString, Value)>> {
        match self.to_value_canonical() {
            Value::Object(map) => {
                let fields: Vec<_> = super::util::log::all_fields_skip_array_elements(&map)
                    .map(|(k, v)| (k, v.clone()))
                    .collect();
                if fields.is_empty() { None } else { Some(fields) }
            }
            _ => None,
        }
    }

    /// Iterate all metadata fields (flattened, with `%` prefix). Owned values.
    pub fn all_metadata_fields(&self) -> Option<Vec<(vrl::value::KeyString, Value)>> {
        match self.metadata.value() {
            Value::Object(metadata_map) => {
                let fields: Vec<_> = super::util::log::all_metadata_fields(metadata_map)
                    .map(|(k, v)| (k, v.clone()))
                    .collect();
                if fields.is_empty() { None } else { Some(fields) }
            }
            _ => None,
        }
    }

    /// Convert to fields — recursively flatten nested objects with dotted keys.
    pub fn convert_to_fields(&self) -> Vec<(vrl::value::KeyString, Value)> {
        match self.to_value_canonical() {
            Value::Object(map) => super::util::log::all_fields(&map)
                .map(|(k, v)| (k, v.clone()))
                .collect(),
            _ => vec![],
        }
    }

    /// Rename a key.
    pub fn rename_key<'a>(
        &mut self,
        from: impl lookup::lookup_v2::TargetPath<'a>,
        to: impl lookup::lookup_v2::TargetPath<'a>,
    ) {
        if let Some(val) = self.remove(from) {
            self.insert(to, val);
        }
    }

    /// Get the timestamp path (proto-canonical: `time_unix_nano`).
    pub fn timestamp_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("time_unix_nano")))
    }

    /// Get the body path.
    pub fn body_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("body")))
    }

    /// Returns the path to the source_type resource attribute, if present.
    pub fn source_type_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        if self.resource_attribute("source_type").is_some() {
            Some(vrl::path::OwnedTargetPath::event(
                lookup::owned_value_path!("resource", "source_type"),
            ))
        } else {
            None
        }
    }

    /// Try insert - only inserts if the path doesn't exist.
    pub fn try_insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a> + Clone,
        value: impl Into<Value>,
    ) {
        if self.get(path.clone()).is_none() {
            self.insert(path, value);
        }
    }

    /// Get the underlying value.
    /// In Vector namespace: returns body only.
    /// In Legacy namespace: returns canonical proto layout.
    pub fn value(&self) -> Value {
        self.body()
            .map(any_value_to_vrl)
            .unwrap_or(Value::Null)
    }

    /// Get all top-level keys from the event.
    pub fn keys(&self) -> Option<std::vec::IntoIter<vrl::value::KeyString>> {
        match self.to_value_canonical() {
            Value::Object(map) => Some(map.into_keys().collect::<Vec<_>>().into_iter()),
            _ => None,
        }
    }

    /// Check if the log has no body and no attributes.
    pub fn is_empty_object(&self) -> bool {
        self.record.body.is_none() && self.record_attrs.is_empty()
    }

    /// Convert to fields unquoted — recursively flatten nested objects with unquoted dotted keys.
    pub fn convert_to_fields_unquoted(&self) -> Vec<(vrl::value::KeyString, Value)> {
        match self.to_value_canonical() {
            Value::Object(map) => super::util::log::all_fields_unquoted(&map)
                .map(|(k, v)| (k, v.clone()))
                .collect(),
            _ => vec![],
        }
    }

    /// Get a snapshot of the value (mutations won't persist — use insert/remove).
    pub fn value_mut(&mut self) -> Value {
        self.to_value_canonical()
    }

    /// Get as an object map.
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_value_canonical() {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn from_str_legacy(msg: impl Into<String>) -> Self {
        let mut log = Self::from(msg.into());
        log.record_mut().time_unix_nano = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .max(0) as u64;
        log
    }

    pub fn from_map(map: ObjectMap, metadata: EventMetadata) -> Self {
        Self::from_value_map(Value::Object(map), metadata)
    }

    pub fn new_with_metadata(metadata: EventMetadata) -> Self {
        Self {
            record: LogRecord::default(),
            record_attrs: OtelAttributes::new(),
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata,
        }
    }

    pub fn parse_path_and_insert(
        &mut self,
        path: impl AsRef<str>,
        value: impl Into<Value>,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let target_path = vrl::path::parse_target_path(path.as_ref())?;
        Ok(self.insert(&target_path, value))
    }

}

// -- OtelSpan --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelSpan {
    pub(crate) span: Span,
    pub(crate) span_attrs: OtelAttributes,
    pub(crate) resource: Option<Resource>,
    pub(crate) resource_attrs: OtelAttributes,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) scope_attrs: OtelAttributes,
    pub(crate) metadata: EventMetadata,
}

impl OtelSpan {
    pub fn new(mut span: Span) -> Self {
        let span_attrs = OtelAttributes::from_key_values(std::mem::take(&mut span.attributes));
        Self {
            span,
            span_attrs,
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata: EventMetadata::default(),
        }
    }

    /// Create an OtelSpan from an OtelLog (for trace signal detection in OTLP decoder).
    ///
    /// The OtelLog's fields become span attributes. Resource and scope are preserved.
    pub fn from_otel_log(log: OtelLog) -> Self {
        let map = log.as_map().unwrap_or_default();
        let mut span_attrs = OtelAttributes::new();
        for (k, v) in map {
            span_attrs.insert(k.to_string(), vrl_value_to_any_value(&v));
        }
        Self {
            span: Span::default(),
            span_attrs,
            resource: log.resource,
            resource_attrs: log.resource_attrs,
            scope: log.scope,
            scope_attrs: log.scope_attrs,
            metadata: log.metadata,
        }
    }

    /// Construct an `OtelSpan` from a legacy-layout Value + metadata.
    ///
    /// Routes native span fields (`name`, `trace_id`, `span_id`,
    /// `parent_span_id`, `start_time`/`end_time`, `kind`, `status`) into
    /// their proto slots; everything else becomes `span.attributes`. See
    /// `apply_value_map` for the full routing contract.
    pub fn from_value_map(value: Value, metadata: EventMetadata) -> Self {
        let mut out = Self {
            span: Span::default(),
            span_attrs: OtelAttributes::new(),
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata,
        };
        out.apply_value_map(value);
        out
    }

    pub fn from_parts(
        mut span: Span,
        mut resource: Option<Resource>,
        mut scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        let span_attrs = OtelAttributes::from_key_values(std::mem::take(&mut span.attributes));
        let resource_attrs = resource.as_mut()
            .map(|r| OtelAttributes::from_key_values(std::mem::take(&mut r.attributes)))
            .unwrap_or_default();
        let scope_attrs = scope.as_mut()
            .map(|s| OtelAttributes::from_key_values(std::mem::take(&mut s.attributes)))
            .unwrap_or_default();
        Self {
            span,
            span_attrs,
            resource,
            resource_attrs,
            scope,
            scope_attrs,
            metadata,
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        Span,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        let mut span = self.span;
        span.attributes = self.span_attrs.to_key_values();
        let resource = self.resource.map(|mut r| {
            r.attributes = self.resource_attrs.to_key_values();
            r
        });
        let scope = self.scope.map(|mut s| {
            s.attributes = self.scope_attrs.to_key_values();
            s
        });
        (span, resource, scope, self.metadata)
    }

    /// Return a full `Span` proto with attributes reconstituted from the
    /// internal `OtelAttributes` map. Use at proto serialization boundaries
    /// (OTLP codec, buffer encoding, gRPC sink).
    pub fn span_to_proto(&self) -> Span {
        let mut span = self.span.clone();
        span.attributes = self.span_attrs.to_key_values();
        span
    }

    pub fn span(&self) -> &Span {
        &self.span
    }

    pub fn span_mut(&mut self) -> &mut Span {
        &mut self.span
    }

    pub fn resource(&self) -> Option<&Resource> {
        self.resource.as_ref()
    }

    pub fn resource_proto(&self) -> Option<Resource> {
        resource_to_proto(self.resource.as_ref(), &self.resource_attrs)
    }

    pub fn scope_proto(&self) -> Option<InstrumentationScope> {
        scope_to_proto(self.scope.as_ref(), &self.scope_attrs)
    }

    pub fn set_resource(&mut self, mut resource: Resource) {
        self.resource_attrs = OtelAttributes::from_key_values(std::mem::take(&mut resource.attributes));
        self.resource = Some(resource);
    }

    pub fn resource_attrs(&self) -> &OtelAttributes {
        &self.resource_attrs
    }

    pub fn scope_attrs(&self) -> &OtelAttributes {
        &self.scope_attrs
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, mut scope: InstrumentationScope) {
        self.scope_attrs = OtelAttributes::from_key_values(std::mem::take(&mut scope.attributes));
        self.scope = Some(scope);
    }

    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        &mut self.metadata
    }

    pub fn name(&self) -> &str {
        &self.span.name
    }

    pub fn trace_id(&self) -> &[u8] {
        &self.span.trace_id
    }

    pub fn span_id(&self) -> &[u8] {
        &self.span.span_id
    }

    pub fn parent_span_id(&self) -> &[u8] {
        &self.span.parent_span_id
    }

    pub fn start_time_unix_nano(&self) -> u64 {
        self.span.start_time_unix_nano
    }

    pub fn end_time_unix_nano(&self) -> u64 {
        self.span.end_time_unix_nano
    }

    pub fn kind(&self) -> i32 {
        self.span.kind
    }

    pub fn status(&self) -> Option<&opentelemetry_proto::tonic::trace::v1::Status> {
        self.span.status.as_ref()
    }

    pub fn attribute(&self, key: &str) -> Option<&AnyValue> {
        self.span_attrs.get(key)
    }

    pub fn set_attribute(&mut self, key: String, value: AnyValue) {
        self.span_attrs.insert(key, value);
    }

    pub fn remove_attribute(&mut self, key: &str) -> Option<AnyValue> {
        self.span_attrs.remove(key)
    }

    pub fn attributes(&self) -> &OtelAttributes {
        &self.span_attrs
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource_attrs.get(key)
    }

    pub fn set_resource_attribute(&mut self, key: String, value: AnyValue) {
        if self.resource.is_none() {
            self.resource = Some(Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            });
        }
        self.resource_attrs.insert(key, value);
    }

    pub fn add_finalizer(&mut self, finalizer: EventFinalizer) {
        self.metadata.add_finalizer(finalizer);
    }

    #[must_use]
    pub fn with_batch_notifier(mut self, batch: &BatchNotifier) -> Self {
        self.metadata = self.metadata.with_batch_notifier(batch);
        self
    }

    #[must_use]
    pub fn with_batch_notifier_option(mut self, batch: &Option<BatchNotifier>) -> Self {
        self.metadata = self.metadata.with_batch_notifier_option(batch);
        self
    }

    // -----------------------------------------------------------------------
    // Field access methods (same pattern as OtelLog — see comment above)
    // -----------------------------------------------------------------------

    pub fn to_value_canonical(&self) -> Value {
        let mut map = ObjectMap::new();

        if !self.span.name.is_empty() {
            map.insert("name".into(), Value::Bytes(self.span.name.clone().into()));
        }
        if !self.span.trace_id.is_empty() {
            map.insert("trace_id".into(), hex_encode(&self.span.trace_id));
        }
        if !self.span.span_id.is_empty() {
            map.insert("span_id".into(), hex_encode(&self.span.span_id));
        }
        if !self.span.parent_span_id.is_empty() {
            map.insert("parent_span_id".into(), hex_encode(&self.span.parent_span_id));
        }
        if self.span.start_time_unix_nano != 0 {
            map.insert("start_time_unix_nano".into(), Value::Integer(self.span.start_time_unix_nano as i64));
        }
        if self.span.end_time_unix_nano != 0 {
            map.insert("end_time_unix_nano".into(), Value::Integer(self.span.end_time_unix_nano as i64));
        }
        if self.span.kind != 0 {
            map.insert("kind".into(), Value::Integer(self.span.kind as i64));
        }
        if self.span.flags != 0 {
            map.insert("flags".into(), Value::Integer(i64::from(self.span.flags)));
        }
        if self.span.dropped_attributes_count != 0 {
            map.insert("dropped_attributes_count".into(), Value::Integer(i64::from(self.span.dropped_attributes_count)));
        }
        if let Some(status) = &self.span.status {
            let mut status_map = ObjectMap::new();
            if !status.message.is_empty() {
                status_map.insert("message".into(), Value::Bytes(status.message.clone().into()));
            }
            status_map.insert("code".into(), Value::Integer(status.code as i64));
            map.insert("status".into(), Value::Object(status_map));
        }
        if !self.span_attrs.is_empty() {
            for (k, v) in self.span_attrs.iter() {
                let key = KeyString::from(k.clone());
                if !map.contains_key(&key) {
                    map.insert(key, any_value_to_vrl(v));
                }
            }
        }
        append_canonical_resource_scope(
            &mut map,
            self.resource.as_ref(),
            &self.resource_attrs,
            self.scope.as_ref(),
            &self.scope_attrs,
        );

        Value::Object(map)
    }

    /// Write back a Value tree to proto fields.
    ///
    /// Handles canonical layout (from `to_value_canonical`): proto fields
    /// are extracted into `Span` slots, `resource`/`scope` sub-objects are
    /// restored, remainder becomes `span.attributes`. Also handles legacy
    /// "start_time"/"end_time" (Timestamp) for old disk buffer compat.
    fn apply_value_map(&mut self, value: Value) {
        use opentelemetry_proto::tonic::trace::v1::Status;

        let mut map = match value {
            Value::Object(m) => m,
            _ => ObjectMap::new(),
        };

        let name = match map.remove("name") {
            Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap_or_default(),
            Some(other) => { map.insert("name".into(), other); String::new() }
            None => String::new(),
        };

        let take_id = |map: &mut ObjectMap, key: &str| -> Vec<u8> {
            match map.remove(key) {
                Some(v) => match hex_decode(&v) {
                    Some(bytes) => bytes,
                    None => { map.insert(key.into(), v); Vec::new() }
                },
                None => Vec::new(),
            }
        };
        let trace_id = take_id(&mut map, "trace_id");
        let span_id = take_id(&mut map, "span_id");
        let parent_span_id = take_id(&mut map, "parent_span_id");

        let take_integer = |map: &mut ObjectMap, key: &str| -> u64 {
            match map.remove(key) {
                Some(Value::Integer(n)) => n as u64,
                Some(other) => { map.insert(key.into(), other); 0 }
                None => 0,
            }
        };
        let start_time_unix_nano = take_integer(&mut map, "start_time_unix_nano");
        let end_time_unix_nano = take_integer(&mut map, "end_time_unix_nano");

        let kind = match map.remove("kind") {
            Some(Value::Integer(i)) => i as i32,
            Some(other) => { map.insert("kind".into(), other); 0 }
            None => 0,
        };

        let flags = match map.remove("flags") {
            Some(Value::Integer(i)) => i as u32,
            Some(other) => { map.insert("flags".into(), other); 0 }
            None => 0,
        };
        let dropped_attributes_count = match map.remove("dropped_attributes_count") {
            Some(Value::Integer(i)) => i as u32,
            Some(other) => { map.insert("dropped_attributes_count".into(), other); 0 }
            None => 0,
        };

        let status = match map.remove("status") {
            Some(Value::Object(mut status_map)) => {
                let message = match status_map.remove("message") {
                    Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap_or_default(),
                    _ => String::new(),
                };
                let code = match status_map.remove("code") {
                    Some(Value::Integer(i)) => i as i32,
                    _ => 0,
                };
                if message.is_empty() && code == 0 && status_map.is_empty() {
                    None
                } else {
                    Some(Status { message, code })
                }
            }
            Some(other) => { map.insert("status".into(), other); None }
            None => None,
        };

        let (resource, resource_attrs) = restore_resource(&mut map);
        self.resource = resource;
        self.resource_attrs = resource_attrs;
        let (scope, scope_attrs) = restore_scope(&mut map);
        self.scope = scope;
        self.scope_attrs = scope_attrs;

        let mut span_attrs = OtelAttributes::new();
        for (k, v) in map {
            span_attrs.insert(k.to_string(), vrl_value_to_any_value(&v));
        }

        self.span = Span {
            name,
            trace_id,
            span_id,
            parent_span_id,
            start_time_unix_nano,
            end_time_unix_nano,
            kind,
            flags,
            dropped_attributes_count,
            status,
            attributes: Vec::new(),
            trace_state: String::new(),
            events: Vec::new(),
            dropped_events_count: 0,
            links: Vec::new(),
            dropped_links_count: 0,
        };
        self.span_attrs = span_attrs;
    }

    /// Get a field value by path.
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.span_get_single_segment(first.as_ref()),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.span_get_field_path(&fields)
                                } else {
                                    let value = self.to_value_canonical();
                                    value.get(path.value_path()).cloned()
                                }
                            }
                            _ => {
                                let value = self.to_value_canonical();
                                value.get(path.value_path()).cloned()
                            }
                        }
                    }
                    _ => {
                        let value = self.to_value_canonical();
                        value.get(path.value_path()).cloned()
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value().get(path.value_path()).cloned()
            }
        }
    }

    fn span_get_single_segment(&self, field: &str) -> Option<Value> {
        match field {
            "name" if !self.span.name.is_empty() => {
                Some(Value::Bytes(self.span.name.clone().into()))
            }
            "trace_id" if !self.span.trace_id.is_empty() => {
                Some(hex_encode(&self.span.trace_id))
            }
            "span_id" if !self.span.span_id.is_empty() => {
                Some(hex_encode(&self.span.span_id))
            }
            "parent_span_id" if !self.span.parent_span_id.is_empty() => {
                Some(hex_encode(&self.span.parent_span_id))
            }
            "start_time" if self.span.start_time_unix_nano != 0 => {
                nanos_to_timestamp(self.span.start_time_unix_nano)
            }
            "end_time" if self.span.end_time_unix_nano != 0 => {
                nanos_to_timestamp(self.span.end_time_unix_nano)
            }
            "kind" if self.span.kind != 0 => {
                Some(Value::Integer(self.span.kind as i64))
            }
            "status" => {
                let status = self.span.status.as_ref()?;
                let mut status_map = ObjectMap::new();
                if !status.message.is_empty() {
                    status_map.insert("message".into(), Value::Bytes(status.message.clone().into()));
                }
                status_map.insert("code".into(), Value::Integer(status.code as i64));
                Some(Value::Object(status_map))
            }
            other => {
                self.span_attrs.get(other)
                    .map(any_value_to_vrl)
            }
        }
    }

    fn span_get_field_path(&self, fields: &[String]) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => {
                let remaining = &fields[1..];
                if remaining.len() == 1 {
                    let key = remaining[0].as_str();
                    if key == "dropped_attributes_count" {
                        let res = self.resource.as_ref()?;
                        if res.dropped_attributes_count != 0 {
                            return Some(Value::Integer(res.dropped_attributes_count as i64));
                        }
                        return None;
                    }
                    self.resource_attrs.get(key).map(any_value_to_vrl)
                } else {
                    let key = remaining[0].as_str();
                    let av = self.resource_attrs.get(key)?;
                    let v = any_value_to_vrl(av);
                    navigate_value(&v, &remaining[1..])
                }
            }
            "scope" => {
                let scope = self.scope.as_ref()?;
                let remaining = &fields[1..];
                match remaining[0].as_str() {
                    "name" if remaining.len() == 1 => {
                        if scope.name.is_empty() { None }
                        else { Some(Value::Bytes(scope.name.clone().into())) }
                    }
                    "version" if remaining.len() == 1 => {
                        if scope.version.is_empty() { None }
                        else { Some(Value::Bytes(scope.version.clone().into())) }
                    }
                    "attributes" => {
                        if remaining.len() == 1 {
                            if self.scope_attrs.is_empty() { None }
                            else { Some(Value::Object(self.scope_attrs.to_object_map())) }
                        } else {
                            let av = self.scope_attrs.get(&remaining[1])?;
                            if remaining.len() == 2 {
                                Some(any_value_to_vrl(av))
                            } else {
                                let v = any_value_to_vrl(av);
                                navigate_value(&v, &remaining[2..])
                            }
                        }
                    }
                    _ => None,
                }
            }
            "status" => {
                let status = self.span.status.as_ref()?;
                let remaining = &fields[1..];
                match remaining[0].as_str() {
                    "message" if remaining.len() == 1 => {
                        if status.message.is_empty() { None }
                        else { Some(Value::Bytes(status.message.clone().into())) }
                    }
                    "code" if remaining.len() == 1 => {
                        Some(Value::Integer(status.code as i64))
                    }
                    _ => None,
                }
            }
            first => {
                if let Some(av) = self.span_attrs.get(first) {
                    let v = any_value_to_vrl(av);
                    if let Some(result) = navigate_value(&v, &fields[1..]) {
                        return Some(result);
                    }
                }
                None
            }
        }
    }

    /// Insert a field value by path.
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let value = value.into();
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.span_insert_single_segment(first.as_ref(), value),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.span_insert_field_path(&fields, value)
                                } else {
                                    let mut val = self.to_value_canonical();
                                    let old = val.insert(path.value_path(), value);
                                    self.apply_value_map(val);
                                    old
                                }
                            }
                            _ => {
                                let mut val = self.to_value_canonical();
                                let old = val.insert(path.value_path(), value);
                                self.apply_value_map(val);
                                old
                            }
                        }
                    }
                    _ => {
                        let mut val = self.to_value_canonical();
                        let old = val.insert(path.value_path(), value);
                        self.apply_value_map(val);
                        old
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().insert(path.value_path(), value)
            }
        }
    }

    fn span_insert_single_segment(&mut self, field: &str, value: Value) -> Option<Value> {
        use opentelemetry_proto::tonic::trace::v1::Status;

        match field {
            "name" => {
                let old = if self.span.name.is_empty() { None }
                    else { Some(Value::Bytes(self.span.name.clone().into())) };
                self.span.name = value.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| value.to_string_lossy().into_owned());
                old
            }
            "trace_id" => {
                let old = if self.span.trace_id.is_empty() { None }
                    else { Some(hex_encode(&self.span.trace_id)) };
                if let Some(decoded) = hex_decode(&value) {
                    self.span.trace_id = decoded;
                } else {
                    self.span_attrs.insert("trace_id".to_string(), vrl_value_to_any_value(&value));
                }
                old
            }
            "span_id" => {
                let old = if self.span.span_id.is_empty() { None }
                    else { Some(hex_encode(&self.span.span_id)) };
                if let Some(decoded) = hex_decode(&value) {
                    self.span.span_id = decoded;
                } else {
                    self.span_attrs.insert("span_id".to_string(), vrl_value_to_any_value(&value));
                }
                old
            }
            "parent_span_id" => {
                let old = if self.span.parent_span_id.is_empty() { None }
                    else { Some(hex_encode(&self.span.parent_span_id)) };
                if let Some(decoded) = hex_decode(&value) {
                    self.span.parent_span_id = decoded;
                } else {
                    self.span_attrs.insert("parent_span_id".to_string(), vrl_value_to_any_value(&value));
                }
                old
            }
            "start_time" => {
                let old = if self.span.start_time_unix_nano != 0 {
                    nanos_to_timestamp(self.span.start_time_unix_nano)
                } else { None };
                if let Some(ts) = value.as_timestamp() {
                    if let Some(nanos) = ts.timestamp_nanos_opt() {
                        if nanos >= 0 {
                            self.span.start_time_unix_nano = nanos as u64;
                            return old;
                        }
                    }
                }
                self.span_attrs.insert("start_time".to_string(), vrl_value_to_any_value(&value));
                old
            }
            "end_time" => {
                let old = if self.span.end_time_unix_nano != 0 {
                    nanos_to_timestamp(self.span.end_time_unix_nano)
                } else { None };
                if let Some(ts) = value.as_timestamp() {
                    if let Some(nanos) = ts.timestamp_nanos_opt() {
                        if nanos >= 0 {
                            self.span.end_time_unix_nano = nanos as u64;
                            return old;
                        }
                    }
                }
                self.span_attrs.insert("end_time".to_string(), vrl_value_to_any_value(&value));
                old
            }
            "kind" => {
                let old = if self.span.kind == 0 { None }
                    else { Some(Value::Integer(self.span.kind as i64)) };
                if let Some(n) = value.as_integer() {
                    self.span.kind = n as i32;
                }
                old
            }
            "status" => {
                let old = self.span.status.as_ref().map(|st| {
                    let mut m = ObjectMap::new();
                    if !st.message.is_empty() {
                        m.insert("message".into(), Value::Bytes(st.message.clone().into()));
                    }
                    m.insert("code".into(), Value::Integer(st.code as i64));
                    Value::Object(m)
                });
                if let Value::Object(map) = &value {
                    let message = map.get("message")
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_default();
                    let code = map.get("code")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(0) as i32;
                    self.span.status = Some(Status { message, code });
                }
                old
            }
            other => {
                let old = self.span_attrs.get(other).map(any_value_to_vrl);
                self.span_attrs.insert(other.to_string(), vrl_value_to_any_value(&value));
                old
            }
        }
    }

    fn span_insert_field_path(&mut self, fields: &[String], value: Value) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => {
                let remaining = &fields[1..];
                if self.resource.is_none() {
                    self.resource = Some(Resource {
                        attributes: Vec::new(),
                        dropped_attributes_count: 0,
                    });
                }
                if remaining.len() == 1 {
                    let key = remaining[0].as_str();
                    let old = self.resource_attrs.get(key).map(any_value_to_vrl);
                    self.resource_attrs.insert(key.to_string(), vrl_value_to_any_value(&value));
                    old
                } else {
                    let key = remaining[0].as_str();
                    let mut v = self.resource_attrs.get(key)
                        .map(any_value_to_vrl)
                        .unwrap_or(Value::Object(ObjectMap::new()));
                    let old = insert_value_at(&mut v, &remaining[1..], value);
                    self.resource_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
                    old
                }
            }
            "scope" => {
                let remaining = &fields[1..];
                let scope = self.scope.get_or_insert_with(InstrumentationScope::default);
                match remaining[0].as_str() {
                    "name" if remaining.len() == 1 => {
                        let old = if scope.name.is_empty() { None }
                            else { Some(Value::Bytes(scope.name.clone().into())) };
                        scope.name = value.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| value.to_string_lossy().into_owned());
                        old
                    }
                    "version" if remaining.len() == 1 => {
                        let old = if scope.version.is_empty() { None }
                            else { Some(Value::Bytes(scope.version.clone().into())) };
                        scope.version = value.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| value.to_string_lossy().into_owned());
                        old
                    }
                    "attributes" if remaining.len() == 1 => {
                        let old = if self.scope_attrs.is_empty() { None }
                            else { Some(Value::Object(self.scope_attrs.to_object_map())) };
                        if let Value::Object(map) = &value {
                            self.scope_attrs = OtelAttributes::from_object_map(map);
                        }
                        old
                    }
                    "attributes" => {
                        let key = remaining[1].as_str();
                        if remaining.len() == 2 {
                            let old = self.scope_attrs.get(key).map(any_value_to_vrl);
                            self.scope_attrs.insert(key.to_string(), vrl_value_to_any_value(&value));
                            old
                        } else {
                            let mut v = self.scope_attrs.get(key)
                                .map(any_value_to_vrl)
                                .unwrap_or(Value::Object(ObjectMap::new()));
                            let old = insert_value_at(&mut v, &remaining[2..], value);
                            self.scope_attrs.insert(key.to_string(), vrl_value_to_any_value(&v));
                            old
                        }
                    }
                    _ => None,
                }
            }
            "status" => {
                use opentelemetry_proto::tonic::trace::v1::Status;
                let remaining = &fields[1..];
                let status = self.span.status.get_or_insert_with(|| Status {
                    message: String::new(),
                    code: 0,
                });
                match remaining[0].as_str() {
                    "message" if remaining.len() == 1 => {
                        let old = if status.message.is_empty() { None }
                            else { Some(Value::Bytes(status.message.clone().into())) };
                        status.message = value.as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| value.to_string_lossy().into_owned());
                        old
                    }
                    "code" if remaining.len() == 1 => {
                        let old = Some(Value::Integer(status.code as i64));
                        if let Some(n) = value.as_integer() {
                            status.code = n as i32;
                        }
                        old
                    }
                    _ => None,
                }
            }
            first => {
                let mut v = self.span_attrs.get(first)
                    .map(any_value_to_vrl)
                    .unwrap_or(Value::Object(ObjectMap::new()));
                let old = insert_value_at(&mut v, &fields[1..], value);
                self.span_attrs.insert(first.to_string(), vrl_value_to_any_value(&v));
                old
            }
        }
    }

    pub fn remove<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
    ) -> Option<Value> {
        self.remove_prune(path, false)
    }

    pub fn remove_prune<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        prune: bool,
    ) -> Option<Value> {
        use lookup::lookup_v2::ValuePath;
        use lookup::path::BorrowedSegment;

        match path.prefix() {
            lookup::PathPrefix::Event => {
                let vp = path.value_path();
                let mut iter = vp.segment_iter();
                match iter.next() {
                    Some(BorrowedSegment::Field(ref first)) => {
                        match iter.next() {
                            None => self.span_remove_single_segment(first.as_ref()),
                            Some(BorrowedSegment::Field(ref second)) => {
                                let mut fields = vec![first.to_string(), second.to_string()];
                                let mut all_fields = true;
                                for seg in iter {
                                    match seg {
                                        BorrowedSegment::Field(f) => fields.push(f.to_string()),
                                        _ => { all_fields = false; break; }
                                    }
                                }
                                if all_fields {
                                    self.span_remove_field_path(&fields, prune)
                                } else {
                                    let mut val = self.to_value_canonical();
                                    let old = val.remove(path.value_path(), prune);
                                    self.apply_value_map(val);
                                    old
                                }
                            }
                            _ => {
                                let mut val = self.to_value_canonical();
                                let old = val.remove(path.value_path(), prune);
                                self.apply_value_map(val);
                                old
                            }
                        }
                    }
                    _ => {
                        let mut val = self.to_value_canonical();
                        let old = val.remove(path.value_path(), prune);
                        self.apply_value_map(val);
                        old
                    }
                }
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().remove(path.value_path(), prune)
            }
        }
    }

    fn span_remove_single_segment(&mut self, field: &str) -> Option<Value> {
        match field {
            "name" => {
                if self.span.name.is_empty() { return None; }
                let old = Some(Value::Bytes(self.span.name.clone().into()));
                self.span.name = String::new();
                old
            }
            "trace_id" => {
                if self.span.trace_id.is_empty() { return None; }
                let old = Some(hex_encode(&self.span.trace_id));
                self.span.trace_id.clear();
                old
            }
            "span_id" => {
                if self.span.span_id.is_empty() { return None; }
                let old = Some(hex_encode(&self.span.span_id));
                self.span.span_id.clear();
                old
            }
            "parent_span_id" => {
                if self.span.parent_span_id.is_empty() { return None; }
                let old = Some(hex_encode(&self.span.parent_span_id));
                self.span.parent_span_id.clear();
                old
            }
            "start_time" | "start_time_unix_nano" => {
                if self.span.start_time_unix_nano == 0 { return None; }
                let old = if field == "start_time" {
                    nanos_to_timestamp(self.span.start_time_unix_nano)
                } else {
                    Some(Value::Integer(self.span.start_time_unix_nano as i64))
                };
                self.span.start_time_unix_nano = 0;
                old
            }
            "end_time" | "end_time_unix_nano" => {
                if self.span.end_time_unix_nano == 0 { return None; }
                let old = if field == "end_time" {
                    nanos_to_timestamp(self.span.end_time_unix_nano)
                } else {
                    Some(Value::Integer(self.span.end_time_unix_nano as i64))
                };
                self.span.end_time_unix_nano = 0;
                old
            }
            "kind" => {
                if self.span.kind == 0 { return None; }
                let old = Some(Value::Integer(self.span.kind as i64));
                self.span.kind = 0;
                old
            }
            "flags" => {
                if self.span.flags == 0 { return None; }
                let old = Some(Value::Integer(i64::from(self.span.flags)));
                self.span.flags = 0;
                old
            }
            "dropped_attributes_count" => {
                if self.span.dropped_attributes_count == 0 { return None; }
                let old = Some(Value::Integer(i64::from(self.span.dropped_attributes_count)));
                self.span.dropped_attributes_count = 0;
                old
            }
            "status" => {
                let status = self.span.status.take()?;
                let mut m = ObjectMap::new();
                if !status.message.is_empty() {
                    m.insert("message".into(), Value::Bytes(status.message.into()));
                }
                m.insert("code".into(), Value::Integer(status.code as i64));
                Some(Value::Object(m))
            }
            other => {
                self.span_attrs.remove(other)
                    .map(|av| any_value_to_vrl(&av))
            }
        }
    }

    fn span_remove_field_path(&mut self, fields: &[String], prune: bool) -> Option<Value> {
        debug_assert!(fields.len() >= 2);
        match fields[0].as_str() {
            "resource" => remove_resource_subpath(
                &mut self.resource_attrs, &fields[1..], prune,
            ),
            "scope" => remove_scope_subpath(
                self.scope.as_mut(), &mut self.scope_attrs, &fields[1..], prune,
            ),
            "status" => {
                let status = self.span.status.as_mut()?;
                let remaining = &fields[1..];
                match remaining[0].as_str() {
                    "message" if remaining.len() == 1 => {
                        if status.message.is_empty() { return None; }
                        let old = Some(Value::Bytes(status.message.clone().into()));
                        status.message = String::new();
                        old
                    }
                    "code" if remaining.len() == 1 => {
                        let old = Some(Value::Integer(status.code as i64));
                        status.code = 0;
                        old
                    }
                    _ => None,
                }
            }
            first => remove_attrs_subpath(
                &mut self.span_attrs, first, &fields[1..], prune,
            ),
        }
    }

    /// Check if a field exists.
    pub fn contains<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> bool {
        self.get(path).is_some()
    }

    /// Get as an object map.
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_value_canonical() {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Parse a path and get a value.
    pub fn parse_path_and_get_value(
        &self,
        path: &str,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let target_path = vrl::path::parse_target_path(path)?;
        Ok(self.get(&target_path))
    }

}

// -- OtelMetric --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelMetric {
    pub(crate) metric: OtelMetricProto,
    pub(crate) dp_attrs: Vec<OtelAttributes>,
    pub(crate) resource: Option<Resource>,
    pub(crate) resource_attrs: OtelAttributes,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) scope_attrs: OtelAttributes,
    pub(crate) metadata: EventMetadata,
}

fn extract_dp_attrs(metric: &mut OtelMetricProto) -> Vec<OtelAttributes> {
    use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
    match metric.data.as_mut() {
        Some(MetricData::Sum(s)) => s.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Gauge(g)) => g.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Histogram(h)) => h.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Summary(s)) => s.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::ExponentialHistogram(e)) => e.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        None => vec![],
    }
}

fn populate_dp_attrs(metric: &mut OtelMetricProto, dp_attrs: &[OtelAttributes]) {
    use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
    macro_rules! write_back {
        ($data_points:expr) => {
            for (dp, attrs) in $data_points.iter_mut().zip(dp_attrs.iter()) {
                dp.attributes = attrs.to_key_values();
            }
        };
    }
    if let Some(data) = metric.data.as_mut() {
        match data {
            MetricData::Sum(s) => write_back!(s.data_points),
            MetricData::Gauge(g) => write_back!(g.data_points),
            MetricData::Histogram(h) => write_back!(h.data_points),
            MetricData::Summary(s) => write_back!(s.data_points),
            MetricData::ExponentialHistogram(e) => write_back!(e.data_points),
        }
    }
}

impl OtelMetric {
    pub fn new(mut metric: OtelMetricProto) -> Self {
        let dp_attrs = extract_dp_attrs(&mut metric);
        Self {
            metric,
            dp_attrs,
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata: EventMetadata::default(),
        }
    }

    /// Convenience constructor for a counter metric.
    /// Builds the OTLP proto directly without going through legacy Metric.
    pub fn new_counter(name: impl Into<String>, kind: super::MetricKind, value: f64) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, number_data_point::Value as NDPValue, NumberDataPoint, Sum,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(NDPValue::AsDouble(value)),
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
                is_monotonic: true,
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for a gauge metric.
    /// Builds the OTLP proto directly without going through legacy Metric.
    pub fn new_gauge(name: impl Into<String>, value: f64) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            metric::Data, number_data_point::Value as NDPValue, Gauge, NumberDataPoint,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    value: Some(NDPValue::AsDouble(value)),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for an aggregated histogram metric.
    pub fn new_histogram(
        name: impl Into<String>,
        kind: super::MetricKind,
        buckets: &[super::metric::Bucket],
        count: u64,
        sum: f64,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, Histogram, HistogramDataPoint,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let n = buckets.len();
        let mut explicit_bounds = Vec::with_capacity(n);
        let mut bucket_counts = Vec::with_capacity(n);
        for b in buckets.iter() {
            bucket_counts.push(b.count);
            explicit_bounds.push(b.upper_limit);
        }
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    count,
                    sum: Some(sum),
                    bucket_counts,
                    explicit_bounds,
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for an aggregated summary metric.
    pub fn new_summary(
        name: impl Into<String>,
        quantiles: &[super::metric::Quantile],
        count: u64,
        sum: f64,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            metric::Data, Summary, SummaryDataPoint,
            summary_data_point::ValueAtQuantile,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Summary(Summary {
                data_points: vec![SummaryDataPoint {
                    count,
                    sum,
                    quantile_values: quantiles
                        .iter()
                        .map(|q| ValueAtQuantile {
                            quantile: q.quantile,
                            value: q.value,
                        })
                        .collect(),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for a set metric.
    /// OTel has no native Set type; represented as a Gauge whose value
    /// is the cardinality (number of unique values).
    pub fn new_set(name: impl Into<String>, cardinality: usize) -> Self {
        Self::new_gauge(name, cardinality as f64)
    }

    /// Convenience constructor for a distribution metric from samples.
    /// Represented as an OTLP Histogram with vector.metric_type=distribution
    /// and vector.statistic attribute indicating histogram vs summary.
    pub fn new_distribution_from_samples(
        name: impl Into<String>,
        kind: super::MetricKind,
        samples: &[super::metric::Sample],
        statistic: super::StatisticKind,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, Histogram, HistogramDataPoint,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let count = samples.iter().map(|s| s.rate).sum::<u32>() as u64;
        let sum: f64 = samples.iter().map(|s| s.value * s.rate as f64).sum();
        let explicit_bounds: Vec<f64> = samples.iter().map(|s| s.value).collect();
        let bucket_counts: Vec<u64> = samples.iter().map(|s| s.rate as u64).collect();

        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    count,
                    sum: Some(sum),
                    bucket_counts,
                    explicit_bounds,
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
            })),
            ..Default::default()
        };
        let mut m = Self::new(proto);
        m.set_data_point_attribute(
            "vector.metric_type".to_string(),
            string_value("distribution"),
        );
        m.set_data_point_attribute(
            "vector.statistic".to_string(),
            string_value(match statistic {
                super::StatisticKind::Histogram => "histogram",
                super::StatisticKind::Summary => "summary",
            }),
        );
        m
    }

    /// Convenience constructor for a distribution metric (empty, no samples).
    /// Use `new_distribution_from_samples` when you have sample data.
    pub fn new_distribution(name: impl Into<String>, kind: super::MetricKind) -> Self {
        Self::new_distribution_from_samples(name, kind, &[], super::StatisticKind::Histogram)
    }

    /// Convenience constructor for a set metric with its values.
    /// Represented as an OTLP Gauge with vector.metric_type=set,
    /// vector.set_values attribute, and cardinality as the numeric value.
    pub fn new_set_from_values(
        name: impl Into<String>,
        kind: super::MetricKind,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let values: Vec<String> = values.into_iter().map(Into::into).collect();
        let cardinality = values.len() as f64;
        let mut m = Self::new_gauge(name, cardinality);
        m.set_data_point_attribute(
            "vector.metric_type".to_string(),
            string_value("set"),
        );
        m.set_data_point_attribute(
            "vector.metric_kind".to_string(),
            string_value(match kind {
                super::MetricKind::Incremental => "incremental",
                super::MetricKind::Absolute => "absolute",
            }),
        );
        let set_values: Vec<AnyValue> = values.iter().map(|v| string_value(v)).collect();
        m.set_data_point_attribute(
            "vector.set_values".to_string(),
            AnyValue {
                value: Some(OtelValueKind::ArrayValue(
                    opentelemetry_proto::tonic::common::v1::ArrayValue { values: set_values },
                )),
            },
        );
        m
    }

    /// Convenience constructor for a delta/signed gauge (e.g. statsd +/-).
    /// Represented as an OTLP Gauge with vector.metric_kind=incremental attribute.
    pub fn new_gauge_delta(name: impl Into<String>, value: f64) -> Self {
        let mut m = Self::new_gauge(name, value);
        m.set_data_point_attribute(
            "vector.metric_kind".to_string(),
            string_value("incremental"),
        );
        m
    }

    /// Construct an OtelMetric directly from metric parts without the legacy Metric struct.
    pub fn from_metric_parts(
        series: super::metric::MetricSeries,
        data: super::metric::MetricData,
        metadata: super::EventMetadata,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric, number_data_point::Value as NDPValue,
        };
        use super::{MetricKind, MetricValue};

        let metric_data = data.value;
        let time_nanos = data
            .time
            .timestamp
            .and_then(|ts| ts.timestamp_nanos_opt())
            .unwrap_or(0) as u64;

        let attributes: Vec<KeyValue> = series
            .tags
            .as_ref()
            .map(|tags| {
                tags.iter_sets()
                    .map(|(k, tag_set)| {
                        use opentelemetry_proto::tonic::common::v1::{ArrayValue, any_value};
                        let raw_vals: Vec<Option<&str>> = tag_set.into_iter().collect();
                        let value = if raw_vals.len() == 1 {
                            match raw_vals[0] {
                                Some(v) => Some(string_value(v)),
                                None => Some(AnyValue { value: None }),
                            }
                        } else {
                            Some(AnyValue {
                                value: Some(any_value::Value::ArrayValue(ArrayValue {
                                    values: raw_vals
                                        .iter()
                                        .map(|v| match v {
                                            Some(s) => string_value(*s),
                                            None => AnyValue { value: None },
                                        })
                                        .collect(),
                                })),
                            })
                        };
                        KeyValue {
                            key: k.to_string(),
                            value,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let temporality = match data.kind {
            MetricKind::Incremental => {
                otel_metrics::AggregationTemporality::Delta as i32
            }
            MetricKind::Absolute => {
                otel_metrics::AggregationTemporality::Cumulative as i32
            }
        };

        let start_nanos = data
            .time
            .interval_ms
            .and_then(|interval| {
                data.time.timestamp.and_then(|ts| ts.timestamp_nanos_opt()).map(|t| {
                    (t as u64).saturating_sub(u64::from(interval.get()) * 1_000_000)
                })
            })
            .unwrap_or(0);

        let data = match metric_data {
            MetricValue::Counter { value } => metric::Data::Sum(otel_metrics::Sum {
                data_points: vec![otel_metrics::NumberDataPoint {
                    attributes: attributes.clone(),
                    start_time_unix_nano: start_nanos,
                    time_unix_nano: time_nanos,
                    exemplars: vec![],
                    flags: 0,
                    value: Some(NDPValue::AsDouble(value)),
                }],
                aggregation_temporality: temporality,
                is_monotonic: true,
            }),
            MetricValue::Gauge { value } => {
                let mut dp_attrs = attributes.clone();
                if data.kind == MetricKind::Incremental {
                    dp_attrs.push(KeyValue {
                        key: "vector.metric_kind".to_string(),
                        value: Some(string_value("incremental")),
                    });
                }
                metric::Data::Gauge(otel_metrics::Gauge {
                    data_points: vec![otel_metrics::NumberDataPoint {
                        attributes: dp_attrs,
                        start_time_unix_nano: start_nanos,
                        time_unix_nano: time_nanos,
                        exemplars: vec![],
                        flags: 0,
                        value: Some(NDPValue::AsDouble(value)),
                    }],
                })
            }
            MetricValue::AggregatedHistogram { buckets, count, sum } => {
                let mut explicit_bounds =
                    Vec::with_capacity(buckets.len().saturating_sub(1));
                let mut bucket_counts = Vec::with_capacity(buckets.len());
                for b in &buckets {
                    bucket_counts.push(b.count);
                    explicit_bounds.push(b.upper_limit);
                }
                if explicit_bounds.last() == Some(&f64::INFINITY) {
                    explicit_bounds.pop();
                }
                metric::Data::Histogram(otel_metrics::Histogram {
                    data_points: vec![otel_metrics::HistogramDataPoint {
                        attributes: attributes.clone(),
                        start_time_unix_nano: start_nanos,
                        time_unix_nano: time_nanos,
                        count,
                        sum: Some(sum),
                        bucket_counts,
                        explicit_bounds,
                        exemplars: vec![],
                        flags: 0,
                        min: None,
                        max: None,
                    }],
                    aggregation_temporality: temporality,
                })
            }
            MetricValue::AggregatedSummary { quantiles, count, sum } => {
                metric::Data::Summary(otel_metrics::Summary {
                    data_points: vec![otel_metrics::SummaryDataPoint {
                        attributes: attributes.clone(),
                        start_time_unix_nano: start_nanos,
                        time_unix_nano: time_nanos,
                        count,
                        sum,
                        quantile_values: quantiles
                            .iter()
                            .map(|q| {
                                otel_metrics::summary_data_point::ValueAtQuantile {
                                    quantile: q.quantile,
                                    value: q.value,
                                }
                            })
                            .collect(),
                        flags: 0,
                    }],
                })
            }
            MetricValue::Set { values } => {
                let mut dp_attrs = attributes.clone();
                dp_attrs.push(KeyValue {
                    key: "vector.metric_type".to_string(),
                    value: Some(string_value("set")),
                });
                dp_attrs.push(KeyValue {
                    key: "vector.metric_kind".to_string(),
                    value: Some(string_value(match data.kind {
                        MetricKind::Incremental => "incremental",
                        MetricKind::Absolute => "absolute",
                    })),
                });
                let set_values: Vec<AnyValue> = values.iter().map(|v| string_value(v)).collect();
                dp_attrs.push(KeyValue {
                    key: "vector.set_values".to_string(),
                    value: Some(AnyValue {
                        value: Some(OtelValueKind::ArrayValue(
                            opentelemetry_proto::tonic::common::v1::ArrayValue { values: set_values },
                        )),
                    }),
                });
                metric::Data::Gauge(otel_metrics::Gauge {
                    data_points: vec![otel_metrics::NumberDataPoint {
                        attributes: dp_attrs,
                        start_time_unix_nano: start_nanos,
                        time_unix_nano: time_nanos,
                        exemplars: vec![],
                        flags: 0,
                        value: Some(NDPValue::AsDouble(values.len() as f64)),
                    }],
                })
            }
            MetricValue::Distribution { samples, statistic } => {
                let count = samples.iter().map(|s| s.rate).sum::<u32>() as u64;
                let sum: f64 = samples.iter().map(|s| s.value * s.rate as f64).sum();
                let mut dp_attrs = attributes.clone();
                dp_attrs.push(KeyValue {
                    key: "vector.metric_type".to_string(),
                    value: Some(string_value("distribution")),
                });
                dp_attrs.push(KeyValue {
                    key: "vector.statistic".to_string(),
                    value: Some(string_value(match statistic {
                        super::StatisticKind::Histogram => "histogram",
                        super::StatisticKind::Summary => "summary",
                    })),
                });
                let explicit_bounds: Vec<f64> = samples.iter().map(|s| s.value).collect();
                let bucket_counts: Vec<u64> = samples.iter().map(|s| s.rate as u64).collect();
                metric::Data::Histogram(otel_metrics::Histogram {
                    data_points: vec![otel_metrics::HistogramDataPoint {
                        attributes: dp_attrs,
                        start_time_unix_nano: start_nanos,
                        time_unix_nano: time_nanos,
                        count,
                        sum: Some(sum),
                        bucket_counts,
                        explicit_bounds,
                        exemplars: vec![],
                        flags: 0,
                        min: None,
                        max: None,
                    }],
                    aggregation_temporality: temporality,
                })
            }
        };

        let name = series.name.name;
        let namespace = series.name.namespace.clone();

        let mut resource_attrs = Vec::new();
        if let Some(ref ns) = namespace {
            resource_attrs.push(KeyValue {
                key: "metric.namespace".to_string(),
                value: Some(string_value(ns)),
            });
        }

        let mut otel_metric = OtelMetricProto {
            name,
            description: String::new(),
            unit: String::new(),
            metadata: vec![],
            data: Some(data),
        };

        let dp_attrs = extract_dp_attrs(&mut otel_metric);

        let ra = OtelAttributes::from_key_values(resource_attrs);
        let resource = if ra.is_empty() {
            None
        } else {
            Some(Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            })
        };

        Self {
            metric: otel_metric,
            dp_attrs,
            resource,
            resource_attrs: ra,
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata,
        }
    }

    pub fn from_parts(
        mut metric: OtelMetricProto,
        mut resource: Option<Resource>,
        mut scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        let dp_attrs = extract_dp_attrs(&mut metric);
        let resource_attrs = resource.as_mut()
            .map(|r| OtelAttributes::from_key_values(std::mem::take(&mut r.attributes)))
            .unwrap_or_default();
        let scope_attrs = scope.as_mut()
            .map(|s| OtelAttributes::from_key_values(std::mem::take(&mut s.attributes)))
            .unwrap_or_default();
        Self {
            metric,
            dp_attrs,
            resource,
            resource_attrs,
            scope,
            scope_attrs,
            metadata,
        }
    }

    pub fn into_parts(
        mut self,
    ) -> (
        OtelMetricProto,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        populate_dp_attrs(&mut self.metric, &self.dp_attrs);
        let resource = self.resource.map(|mut r| {
            r.attributes = self.resource_attrs.to_key_values();
            r
        });
        let scope = self.scope.map(|mut s| {
            s.attributes = self.scope_attrs.to_key_values();
            s
        });
        (self.metric, resource, scope, self.metadata)
    }

    pub fn metric_proto(&self) -> OtelMetricProto {
        let mut m = self.metric.clone();
        populate_dp_attrs(&mut m, &self.dp_attrs);
        m
    }

    pub fn metric(&self) -> &OtelMetricProto {
        &self.metric
    }

    pub fn metric_mut(&mut self) -> &mut OtelMetricProto {
        &mut self.metric
    }

    pub fn resource(&self) -> Option<&Resource> {
        self.resource.as_ref()
    }

    pub fn resource_proto(&self) -> Option<Resource> {
        resource_to_proto(self.resource.as_ref(), &self.resource_attrs)
    }

    pub fn scope_proto(&self) -> Option<InstrumentationScope> {
        scope_to_proto(self.scope.as_ref(), &self.scope_attrs)
    }

    pub fn set_resource(&mut self, mut resource: Resource) {
        self.resource_attrs = OtelAttributes::from_key_values(std::mem::take(&mut resource.attributes));
        self.resource = Some(resource);
    }

    pub fn resource_attrs(&self) -> &OtelAttributes {
        &self.resource_attrs
    }

    pub fn scope_attrs(&self) -> &OtelAttributes {
        &self.scope_attrs
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, mut scope: InstrumentationScope) {
        self.scope_attrs = OtelAttributes::from_key_values(std::mem::take(&mut scope.attributes));
        self.scope = Some(scope);
    }

    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        &mut self.metadata
    }

    pub fn name(&self) -> &str {
        &self.metric.name
    }

    pub fn description(&self) -> &str {
        &self.metric.description
    }

    pub fn unit(&self) -> &str {
        &self.metric.unit
    }

    pub fn first_dp_attrs(&self) -> Option<&OtelAttributes> {
        self.dp_attrs.first()
    }

    pub fn set_data_point_attribute(&mut self, key: String, value: AnyValue) {
        for attrs in &mut self.dp_attrs {
            attrs.insert(key.clone(), value.clone());
        }
    }

    pub fn reduce_tags_to_single(&mut self) {
        for attrs in &mut self.dp_attrs {
            let updates: Vec<(String, AnyValue)> = attrs.iter()
                .filter_map(|(key, val)| {
                    if let Some(OtelValueKind::ArrayValue(arr)) = &val.value {
                        let last = arr.values.iter().rev().find_map(|v| match &v.value {
                            Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                            _ => None,
                        });
                        last.map(|s| (key.clone(), AnyValue {
                            value: Some(OtelValueKind::StringValue(s)),
                        }))
                    } else {
                        None
                    }
                })
                .collect();
            for (key, val) in updates {
                attrs.insert(key, val);
            }
        }
    }

    pub fn remove_data_point_attribute(&mut self, key: &str) -> Option<AnyValue> {
        let mut removed = None;
        for attrs in &mut self.dp_attrs {
            removed = removed.or(attrs.remove(key));
        }
        removed
    }

    /// Replace a tag: remove existing attribute then set new value.
    pub fn replace_tag(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        self.remove_data_point_attribute(&key);
        self.set_data_point_attribute(key, AnyValue {
            value: Some(OtelValueKind::StringValue(value.into())),
        });
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource_attrs.get(key)
    }

    pub fn set_resource_attribute(&mut self, key: String, value: AnyValue) {
        if self.resource.is_none() {
            self.resource = Some(Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            });
        }
        self.resource_attrs.insert(key, value);
    }

    // -----------------------------------------------------------------------
    // Metric accessors
    //
    // `value()`, `kind()`, `tag_value()` read proto directly via
    // `extract_metric_data()`. `timestamp()` and `namespace()` also read
    // proto directly. `tags()` is a broken stub — see its doc comment.
    // -----------------------------------------------------------------------

    /// Get the metric timestamp from the first data point.
    pub fn timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.to_legacy_metric_ref_timestamp()
    }

    /// Get the interval between start_time and end_time in milliseconds.
    pub fn interval_ms(&self) -> Option<std::num::NonZeroU32> {
        self.reconstruct_interval_ms()
    }

    fn to_legacy_metric_ref_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        use opentelemetry_proto::tonic::metrics::v1::metric;
        let data = self.metric.data.as_ref()?;
        let nanos = match data {
            metric::Data::Gauge(g) => g.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Sum(s) => s.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Histogram(h) => h.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::ExponentialHistogram(h) => h.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Summary(s) => s.data_points.first().map(|dp| dp.time_unix_nano),
        }?;
        if nanos == 0 { return None; }
        let secs = (nanos / 1_000_000_000) as i64;
        let nsecs = (nanos % 1_000_000_000) as u32;
        chrono::DateTime::from_timestamp(secs, nsecs)
    }

    /// Set the timestamp on all data points.
    ///
    /// Note: `None` sets `time_unix_nano` to 0, which `timestamp()` reads back
    /// as `None`. A `Some(ts)` at the Unix epoch (1970-01-01T00:00:00Z) also
    /// produces nanos == 0 and will round-trip as `None`.
    pub fn set_timestamp(&mut self, ts: Option<chrono::DateTime<chrono::Utc>>) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let nanos = ts.map(|t| t.timestamp_nanos_opt().unwrap_or(0) as u64).unwrap_or(0);
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => { for dp in &mut s.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Gauge(g) => { for dp in &mut g.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Histogram(h) => { for dp in &mut h.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Summary(s) => { for dp in &mut s.data_points { dp.time_unix_nano = nanos; } }
                MetricData::ExponentialHistogram(e) => { for dp in &mut e.data_points { dp.time_unix_nano = nanos; } }
            }
        }
    }

    pub fn set_kind(&mut self, kind: super::MetricKind) {
        use opentelemetry_proto::tonic::metrics::v1::{metric::Data as MetricData, AggregationTemporality};
        let temp = match kind {
            super::MetricKind::Incremental => AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => AggregationTemporality::Cumulative as i32,
        };
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => s.aggregation_temporality = temp,
                MetricData::Histogram(h) => h.aggregation_temporality = temp,
                MetricData::ExponentialHistogram(e) => e.aggregation_temporality = temp,
                MetricData::Gauge(_) | MetricData::Summary(_) => {}
            }
        }
    }

    pub fn set_namespace(&mut self, namespace: impl Into<String>) {
        self.set_resource_attribute("metric.namespace".to_string(), string_value(&namespace.into()));
    }

    /// Builder-style: set the metric namespace (stored as `metric.namespace` resource attribute).
    pub fn with_namespace(mut self, namespace: Option<impl Into<String>>) -> Self {
        if let Some(ns) = namespace {
            if self.resource.is_none() {
                self.resource = Some(Resource {
                    attributes: Vec::new(),
                    dropped_attributes_count: 0,
                });
            }
            self.resource_attrs.insert("metric.namespace".to_string(), string_value(&ns.into()));
        }
        self
    }

    /// Builder-style: set the timestamp on all data points.
    pub fn with_timestamp(mut self, ts: Option<chrono::DateTime<chrono::Utc>>) -> Self {
        self.set_timestamp(ts);
        self
    }

    /// Builder-style: set tags as data point attributes.
    ///
    /// Preserves multi-value tags: each key becomes a single `KeyValue` whose
    /// value is a `StringValue` (single) or an `ArrayValue` of strings/nulls
    /// (multi). Mirrors the tag encoding used by `from_metric_parts`.
    pub fn with_tags(mut self, tags: Option<super::metric::MetricTags>) -> Self {
        use opentelemetry_proto::tonic::common::v1::{ArrayValue, any_value};
        let Some(tags) = tags else { return self };
        for (key, tag_set) in tags.iter_sets() {
            let raw_vals: Vec<Option<&str>> = tag_set.into_iter().collect();
            let value = if raw_vals.len() == 1 {
                match raw_vals[0] {
                    Some(v) => string_value(v),
                    None => AnyValue { value: None },
                }
            } else {
                AnyValue {
                    value: Some(any_value::Value::ArrayValue(ArrayValue {
                        values: raw_vals
                            .iter()
                            .map(|v| match v {
                                Some(s) => string_value(*s),
                                None => AnyValue { value: None },
                            })
                            .collect(),
                    })),
                }
            };
            self.remove_data_point_attribute(key);
            self.set_data_point_attribute(key.to_string(), value);
        }
        self
    }

    pub fn with_metadata(mut self, metadata: EventMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build MetricTags from proto data point, resource, and scope attributes.
    ///
    /// Returns an owned `MetricTags` (not a reference) because the tags are
    /// assembled from multiple proto fields. Returns `None` if there are no
    /// tags at all.
    pub fn tags(&self) -> Option<super::metric::MetricTags> {
        let mut tags = super::MetricTags::default();

        // Resource attributes (prefixed with "resource.")
        for (key, val) in self.resource_attrs.iter() {
            if key == "metric.namespace" {
                continue;
            }
            if let Some(ref v) = val.value {
                tags.insert(
                    format!("resource.{}", key),
                    otel_value_to_tag_string(v),
                );
            }
        }

        // Scope attributes
        if let Some(ref scope) = self.scope {
            if !scope.name.is_empty() {
                tags.insert("scope.name".to_string(), scope.name.clone());
            }
            if !scope.version.is_empty() {
                tags.insert("scope.version".to_string(), scope.version.clone());
            }
        }

        // Data point attributes
        if let Some(dp) = self.dp_attrs.first() {
            for (key, val) in dp.iter() {
                if key.starts_with("vector.") {
                    continue;
                }
                if val.value.is_some() {
                    insert_otel_attr_as_tag_from_any_value(&mut tags, key, val);
                } else {
                    tags.replace(key.clone(), super::metric::TagValue::Bare);
                }
            }
        }

        if tags.is_empty() { None } else { Some(tags) }
    }

    /// Get the metric namespace from the `metric.namespace` resource attribute.
    pub fn namespace(&self) -> Option<&str> {
        self.resource_attrs.get("metric.namespace")
            .and_then(|av| match &av.value {
                Some(OtelValueKind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
    }

    /// Get the metric value directly from proto.
    pub fn value(&self) -> super::MetricValue {
        self.extract_metric_data().1
    }

    /// Get the metric kind directly from proto.
    pub fn kind(&self) -> super::MetricKind {
        self.extract_metric_data().0
    }

    pub fn tag_value(&self, key: &str) -> Option<String> {
        if let Some(dp) = self.dp_attrs.first() {
            if let Some(av) = dp.get(key) {
                if let Some(ref v) = av.value {
                    return Some(otel_value_to_tag_string(v));
                }
            }
        }
        // Check resource attributes (prefixed with "resource." in legacy)
        if let Some(stripped) = key.strip_prefix("resource.") {
            if let Some(av) = self.resource_attrs.get(stripped) {
                if let Some(ref v) = av.value {
                    return Some(otel_value_to_tag_string(v));
                }
            }
        }
        // Check scope attributes
        if let Some(ref scope) = self.scope {
            match key {
                "scope.name" if !scope.name.is_empty() => return Some(scope.name.clone()),
                "scope.version" if !scope.version.is_empty() => return Some(scope.version.clone()),
                _ => {}
            }
        }
        None
    }

    /// Check whether a tag with the given name matches the given value.
    pub fn tag_matches(&self, name: &str, value: &str) -> bool {
        self.tag_value(name)
            .filter(|v| v == value)
            .is_some()
    }

    /// Extract (MetricKind, MetricValue, timestamp, data-point attributes) from proto.
    ///
    /// Single source of truth for metric data interpretation — used by `value()`,
    /// `kind()`, and `into_metric_parts()`.
    fn extract_metric_data(
        &self,
    ) -> (
        super::MetricKind,
        super::MetricValue,
        Option<chrono::DateTime<chrono::Utc>>,
    ) {
        use chrono::Utc;
        use opentelemetry_proto::tonic::metrics::v1::{
            metric, number_data_point::Value as NDPValue, AggregationTemporality,
        };
        use super::{MetricKind, MetricValue};
        use super::metric::{Bucket, Quantile};

        let nanos_to_ts = |nanos: u64| -> Option<chrono::DateTime<Utc>> {
            if nanos == 0 {
                None
            } else {
                let secs = (nanos / 1_000_000_000) as i64;
                let nsecs = (nanos % 1_000_000_000) as u32;
                chrono::DateTime::from_timestamp(secs, nsecs)
            }
        };

        let dp0 = self.dp_attrs.first();

        let dp_has_str = |key: &str, val: &str| -> bool {
            dp0.and_then(|a| a.get(key))
                .and_then(|av| av.value.as_ref())
                .map(|v| v == &OtelValueKind::StringValue(val.into()))
                .unwrap_or(false)
        };

        let dp_get_str = |key: &str| -> Option<String> {
            dp0.and_then(|a| a.get(key))
                .and_then(|av| match av.value.as_ref() {
                    Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                    _ => None,
                })
        };

        match self.metric.data.as_ref() {
            Some(metric::Data::Sum(sum)) => {
                let dp = sum.data_points.first();
                let val = dp.and_then(|p| p.value.as_ref()).map(|v| match v {
                    NDPValue::AsDouble(f) => *f,
                    NDPValue::AsInt(i) => *i as f64,
                }).unwrap_or(0.0);
                let kind = if sum.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    MetricKind::Incremental
                } else {
                    MetricKind::Absolute
                };
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                let metric_value = if sum.is_monotonic {
                    MetricValue::Counter { value: val }
                } else {
                    MetricValue::Gauge { value: val }
                };
                (kind, metric_value, ts)
            }
            Some(metric::Data::Gauge(gauge)) => {
                let dp = gauge.data_points.first();
                let is_set = dp_has_str("vector.metric_type", "set");
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                if is_set {
                    let values = dp0
                        .and_then(|a| a.get("vector.set_values"))
                        .and_then(|av| match &av.value {
                            Some(OtelValueKind::ArrayValue(arr)) => {
                                let vals: BTreeSet<String> = arr
                                    .values
                                    .iter()
                                    .filter_map(|v| match &v.value {
                                        Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                Some(vals)
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let kind = match dp_get_str("vector.metric_kind").as_deref() {
                        Some("incremental") => MetricKind::Incremental,
                        _ => MetricKind::Absolute,
                    };
                    (kind, MetricValue::Set { values }, ts)
                } else {
                    let val = dp.and_then(|p| p.value.as_ref()).map(|v| match v {
                        NDPValue::AsDouble(f) => *f,
                        NDPValue::AsInt(i) => *i as f64,
                    }).unwrap_or(0.0);
                    let kind = match dp_get_str("vector.metric_kind").as_deref() {
                        Some("incremental") => MetricKind::Incremental,
                        _ => MetricKind::Absolute,
                    };
                    (kind, MetricValue::Gauge { value: val }, ts)
                }
            }
            Some(metric::Data::Histogram(hist)) => {
                let dp = hist.data_points.first();
                let kind = if hist.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    MetricKind::Incremental
                } else {
                    MetricKind::Absolute
                };
                let is_distribution = dp_has_str("vector.metric_type", "distribution");
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                if is_distribution {
                    let statistic = match dp_get_str("vector.statistic").as_deref() {
                        Some("summary") => super::StatisticKind::Summary,
                        _ => super::StatisticKind::Histogram,
                    };
                    let samples = dp
                        .map(|p| {
                            p.explicit_bounds
                                .iter()
                                .zip(p.bucket_counts.iter())
                                .map(|(&value, &rate)| super::metric::Sample {
                                    value,
                                    rate: rate as u32,
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (kind, MetricValue::Distribution { samples, statistic }, ts)
                } else {
                    let (buckets, count, sum_val) = dp
                        .map(|p| {
                            let buckets: Vec<Bucket> = p.bucket_counts.iter().enumerate()
                                .map(|(i, &c)| Bucket {
                                    count: c,
                                    upper_limit: p.explicit_bounds.get(i).copied().unwrap_or(f64::INFINITY),
                                })
                                .collect();
                            (buckets, p.count, p.sum.unwrap_or(0.0))
                        })
                        .unwrap_or_default();
                    (kind, MetricValue::AggregatedHistogram { buckets, count, sum: sum_val }, ts)
                }
            }
            Some(metric::Data::Summary(summary)) => {
                let dp = summary.data_points.first();
                let (quantiles, count, sum_val) = dp
                    .map(|p| {
                        let quantiles: Vec<Quantile> = p.quantile_values.iter()
                            .map(|q| Quantile { quantile: q.quantile, value: q.value })
                            .collect();
                        (quantiles, p.count, p.sum)
                    })
                    .unwrap_or_default();
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                (MetricKind::Absolute, MetricValue::AggregatedSummary { quantiles, count, sum: sum_val }, ts)
            }
            Some(metric::Data::ExponentialHistogram(exp)) => {
                let dp = exp.data_points.first();
                let kind = if exp.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    MetricKind::Incremental
                } else {
                    MetricKind::Absolute
                };
                let (buckets, count, sum_val) = dp
                    .map(|p| {
                        let scale = p.scale;
                        let base = 2f64.powf(2f64.powi(-scale));
                        let mut buckets = Vec::new();
                        if let Some(ref neg) = p.negative {
                            for (i, &c) in neg.bucket_counts.iter().enumerate() {
                                let idx = neg.offset + i as i32;
                                buckets.push(Bucket { count: c, upper_limit: -base.powi(idx) });
                            }
                        }
                        if p.zero_count > 0 {
                            buckets.push(Bucket { count: p.zero_count, upper_limit: 0.0 });
                        }
                        if let Some(ref pos) = p.positive {
                            for (i, &c) in pos.bucket_counts.iter().enumerate() {
                                let idx = pos.offset + i as i32;
                                buckets.push(Bucket { count: c, upper_limit: base.powi(idx + 1) });
                            }
                        }
                        (buckets, p.count, p.sum.unwrap_or(0.0))
                    })
                    .unwrap_or_default();
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                (kind, MetricValue::AggregatedHistogram { buckets, count, sum: sum_val }, ts)
            }
            None => (MetricKind::Absolute, MetricValue::Gauge { value: 0.0 }, None),
        }
    }

    /// Decompose this OtelMetric into legacy metric parts without creating
    /// an intermediate Metric. Used by aggregate and other transforms that
    /// store MetricSeries/MetricData separately.
    /// Build a `MetricSeries` key for this metric (name + namespace + tags).
    /// This is the grouping key used by aggregate/normalization.
    pub fn metric_series(&self) -> super::metric::MetricSeries {
        use super::metric::{MetricName, MetricSeries};
        MetricSeries {
            name: MetricName {
                name: self.metric.name.clone(),
                namespace: self.namespace().map(|s| s.to_string()),
            },
            tags: self.tags(),
        }
    }

    pub fn into_metric_parts(self) -> (super::metric::MetricSeries, super::metric::MetricData, super::EventMetadata) {
        use super::metric::{MetricData, MetricName, MetricSeries, MetricTime};

        let name = self.metric.name.clone();
        let namespace = self.namespace().map(|s| s.to_string());
        let metric_tags = self.tags();
        let (kind, value, timestamp) = self.extract_metric_data();
        let interval_ms = self.reconstruct_interval_ms();

        let series = MetricSeries {
            name: MetricName { name, namespace },
            tags: metric_tags,
        };
        let data = MetricData {
            time: MetricTime { timestamp, interval_ms },
            kind,
            value,
        };
        (series, data, self.metadata)
    }

    fn reconstruct_interval_ms(&self) -> Option<std::num::NonZeroU32> {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let dp_times = match self.metric.data.as_ref()? {
            MetricData::Sum(s) => s.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Gauge(g) => g.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Histogram(h) => h.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Summary(s) => s.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::ExponentialHistogram(e) => e.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
        };
        dp_times.and_then(|(start, end)| {
            if start > 0 && end > start {
                let diff_ms = (end - start) / 1_000_000;
                std::num::NonZeroU32::new(diff_ms as u32)
            } else {
                None
            }
        })
    }

    pub fn add_finalizer(&mut self, finalizer: EventFinalizer) {
        self.metadata.add_finalizer(finalizer);
    }

    #[must_use]
    pub fn with_batch_notifier(mut self, batch: &BatchNotifier) -> Self {
        self.metadata = self.metadata.with_batch_notifier(batch);
        self
    }

    #[must_use]
    pub fn with_batch_notifier_option(mut self, batch: &Option<BatchNotifier>) -> Self {
        self.metadata = self.metadata.with_batch_notifier_option(batch);
        self
    }

    /// Add the data from `other` to this metric.
    ///
    /// Both metrics must have the same data type (Sum+Sum, Gauge+Gauge, etc.).
    /// For Histogram, bucket layouts (explicit_bounds) must match.
    /// Returns `false` if the types are incompatible.
    #[must_use]
    pub fn add(&mut self, other: &Self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match (self.metric.data.as_mut(), other.metric.data.as_ref()) {
            (Some(MD::Sum(s)), Some(MD::Sum(o))) => {
                for (dp, odp) in s.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v += ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v += ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Gauge(g)), Some(MD::Gauge(o))) => {
                for (dp, odp) in g.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v += ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v += ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Histogram(h)), Some(MD::Histogram(oh))) => {
                for (dp, odp) in h.data_points.iter_mut().zip(oh.data_points.iter()) {
                    if dp.explicit_bounds != odp.explicit_bounds
                        || dp.bucket_counts.len() != odp.bucket_counts.len()
                    {
                        return false;
                    }
                    for (bc, obc) in dp.bucket_counts.iter_mut().zip(odp.bucket_counts.iter()) {
                        *bc += obc;
                    }
                    dp.count += odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) + odp.sum.unwrap_or(0.0));
                }
                true
            }
            (Some(MD::Summary(_)), Some(MD::Summary(_))) => {
                // Summaries (quantile sketches) cannot be meaningfully added
                false
            }
            (Some(MD::ExponentialHistogram(eh)), Some(MD::ExponentialHistogram(oeh))) => {
                for (dp, odp) in eh.data_points.iter_mut().zip(oeh.data_points.iter()) {
                    if dp.scale != odp.scale {
                        return false;
                    }
                    dp.count += odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) + odp.sum.unwrap_or(0.0));
                    dp.zero_count += odp.zero_count;
                    if let (Some(pos), Some(opos)) = (&mut dp.positive, &odp.positive) {
                        if pos.offset == opos.offset && pos.bucket_counts.len() == opos.bucket_counts.len() {
                            for (bc, obc) in pos.bucket_counts.iter_mut().zip(opos.bucket_counts.iter()) {
                                *bc += obc;
                            }
                        } else {
                            return false;
                        }
                    }
                    if let (Some(neg), Some(oneg)) = (&mut dp.negative, &odp.negative) {
                        if neg.offset == oneg.offset && neg.bucket_counts.len() == oneg.bucket_counts.len() {
                            for (bc, obc) in neg.bucket_counts.iter_mut().zip(oneg.bucket_counts.iter()) {
                                *bc += obc;
                            }
                        } else {
                            return false;
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Subtract the data of `other` from this metric.
    ///
    /// Both metrics must have the same data type. For counters (Sum),
    /// this is monotonic: returns `false` if subtraction would go negative.
    #[must_use]
    pub fn subtract(&mut self, other: &Self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match (self.metric.data.as_mut(), other.metric.data.as_ref()) {
            (Some(MD::Sum(s)), Some(MD::Sum(o))) => {
                for (dp, odp) in s.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => {
                            if *v < *ov { return false; }
                            *v -= ov;
                        }
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => {
                            if *v < *ov { return false; }
                            *v -= ov;
                        }
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Gauge(g)), Some(MD::Gauge(o))) => {
                for (dp, odp) in g.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v -= ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v -= ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Histogram(h)), Some(MD::Histogram(oh))) => {
                for (dp, odp) in h.data_points.iter_mut().zip(oh.data_points.iter()) {
                    if dp.explicit_bounds != odp.explicit_bounds
                        || dp.bucket_counts.len() != odp.bucket_counts.len()
                        || dp.count < odp.count
                    {
                        return false;
                    }
                    for (bc, obc) in dp.bucket_counts.iter_mut().zip(odp.bucket_counts.iter()) {
                        if *bc < *obc { return false; }
                        *bc -= obc;
                    }
                    dp.count -= odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) - odp.sum.unwrap_or(0.0));
                }
                true
            }
            _ => false,
        }
    }

    /// Zero out all data point values in this metric.
    pub fn zero(&mut self) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_mut() {
            Some(MD::Sum(s)) => {
                for dp in &mut s.data_points {
                    match &mut dp.value {
                        Some(NDPValue::AsDouble(v)) => *v = 0.0,
                        Some(NDPValue::AsInt(v)) => *v = 0,
                        _ => {}
                    }
                }
            }
            Some(MD::Gauge(g)) => {
                for dp in &mut g.data_points {
                    match &mut dp.value {
                        Some(NDPValue::AsDouble(v)) => *v = 0.0,
                        Some(NDPValue::AsInt(v)) => *v = 0,
                        _ => {}
                    }
                }
            }
            Some(MD::Histogram(h)) => {
                for dp in &mut h.data_points {
                    for bc in &mut dp.bucket_counts { *bc = 0; }
                    dp.count = 0;
                    dp.sum = Some(0.0);
                }
            }
            Some(MD::Summary(s)) => {
                for dp in &mut s.data_points {
                    for qv in &mut dp.quantile_values { qv.value = 0.0; }
                    dp.count = 0;
                    dp.sum = 0.0;
                }
            }
            Some(MD::ExponentialHistogram(eh)) => {
                for dp in &mut eh.data_points {
                    dp.count = 0;
                    dp.sum = Some(0.0);
                    dp.zero_count = 0;
                    if let Some(ref mut pos) = dp.positive {
                        for bc in &mut pos.bucket_counts { *bc = 0; }
                    }
                    if let Some(ref mut neg) = dp.negative {
                        for bc in &mut neg.bucket_counts { *bc = 0; }
                    }
                }
            }
            None => {}
        }
    }

    /// Set the first data point value (Sum or Gauge only).
    pub fn set_first_value(&mut self, val: f64) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_mut() {
            Some(MD::Sum(s)) => {
                if let Some(dp) = s.data_points.first_mut() {
                    dp.value = Some(NDPValue::AsDouble(val));
                }
            }
            Some(MD::Gauge(g)) => {
                if let Some(dp) = g.data_points.first_mut() {
                    dp.value = Some(NDPValue::AsDouble(val));
                }
            }
            _ => {}
        }
    }

    /// Get the first data point value as f64, if this is a Sum or Gauge.
    pub fn first_value_as_f64(&self) -> Option<f64> {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.data_points.first().and_then(|dp| match &dp.value {
                Some(NDPValue::AsDouble(v)) => Some(*v),
                Some(NDPValue::AsInt(v)) => Some(*v as f64),
                _ => None,
            }),
            Some(MD::Gauge(g)) => g.data_points.first().and_then(|dp| match &dp.value {
                Some(NDPValue::AsDouble(v)) => Some(*v),
                Some(NDPValue::AsInt(v)) => Some(*v as f64),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Check if this metric is a delta (incremental) type.
    /// Only Sum, Histogram, and ExponentialHistogram have AggregationTemporality.
    /// Gauge and Summary are point-in-time and neither delta nor cumulative.
    pub fn is_delta(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::{AggregationTemporality, metric::Data as MD};
        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.aggregation_temporality == AggregationTemporality::Delta as i32,
            Some(MD::Histogram(h)) => h.aggregation_temporality == AggregationTemporality::Delta as i32,
            Some(MD::ExponentialHistogram(eh)) => eh.aggregation_temporality == AggregationTemporality::Delta as i32,
            _ => false,
        }
    }

    /// Check if this metric is cumulative. Gauge and Summary have no temporality
    /// and return `false` (per OTel spec and otelcol-contrib behavior).
    pub fn is_cumulative(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::{AggregationTemporality, metric::Data as MD};
        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            Some(MD::Histogram(h)) => h.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            Some(MD::ExponentialHistogram(eh)) => eh.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            _ => false,
        }
    }

    /// Check if this metric type carries an aggregation temporality field.
    /// Sum, Histogram, and ExponentialHistogram have temporality.
    /// Gauge and Summary do not.
    pub fn has_temporality(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        matches!(self.metric.data.as_ref(), Some(MD::Sum(_) | MD::Histogram(_) | MD::ExponentialHistogram(_)))
    }

    /// Check if this metric is a Gauge type.
    pub fn is_gauge(&self) -> bool {
        matches!(self.metric.data.as_ref(), Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(_)))
    }

    /// Check if this metric is a Sum type.
    pub fn is_sum(&self) -> bool {
        matches!(self.metric.data.as_ref(), Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Sum(_)))
    }

    /// Check if this metric is a Set (stored as Gauge with vector.metric_type=set attribute).
    pub fn is_set(&self) -> bool {
        self.dp_attrs.first()
            .and_then(|attrs| attrs.get("vector.metric_type"))
            .and_then(|av| av.value.as_ref())
            .is_some_and(|v| matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == "set"))
    }

    /// Check if this metric is a Distribution (stored as Histogram with vector.metric_type=distribution attribute).
    pub fn is_distribution(&self) -> bool {
        self.dp_attrs.first()
            .and_then(|attrs| attrs.get("vector.metric_type"))
            .and_then(|av| av.value.as_ref())
            .is_some_and(|v| matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == "distribution"))
    }

    /// Convert this metric to an `AnyValue::KvlistValue` suitable for use as
    /// an OtelLog body. Includes name, description, unit, and data (sum/gauge/
    /// histogram/summary/exponentialHistogram). Does NOT include resource/scope
    /// — those should be transferred directly to the OtelLog.
    pub fn to_log_body(&self) -> AnyValue {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;

        let metric = self.metric_proto();
        let mut kvs: Vec<KeyValue> = Vec::new();

        kvs.push(KeyValue { key: "name".into(), value: Some(string_value(&metric.name)) });
        if !metric.description.is_empty() {
            kvs.push(KeyValue { key: "description".into(), value: Some(string_value(&metric.description)) });
        }
        if !metric.unit.is_empty() {
            kvs.push(KeyValue { key: "unit".into(), value: Some(string_value(&metric.unit)) });
        }

        if let Some(ref data) = metric.data {
            match data {
                MetricData::Sum(sum) => {
                    kvs.push(KeyValue { key: "sum".into(), value: Some(sum_to_any_value(sum)) });
                }
                MetricData::Gauge(gauge) => {
                    kvs.push(KeyValue { key: "gauge".into(), value: Some(gauge_to_any_value(gauge)) });
                }
                MetricData::Histogram(hist) => {
                    kvs.push(KeyValue { key: "histogram".into(), value: Some(histogram_to_any_value(hist)) });
                }
                MetricData::Summary(summary) => {
                    kvs.push(KeyValue { key: "summary".into(), value: Some(summary_to_any_value(summary)) });
                }
                MetricData::ExponentialHistogram(exp) => {
                    kvs.push(KeyValue { key: "exponentialHistogram".into(), value: Some(exp_histogram_to_any_value(exp)) });
                }
            }
        }

        AnyValue {
            value: Some(OtelValueKind::KvlistValue(
                opentelemetry_proto::tonic::common::v1::KeyValueList { values: kvs },
            )),
        }
    }
}

fn double_value(d: f64) -> AnyValue {
    AnyValue { value: Some(OtelValueKind::DoubleValue(d)) }
}

fn bool_value(b: bool) -> AnyValue {
    AnyValue { value: Some(OtelValueKind::BoolValue(b)) }
}

fn kvlist_any_value(kvs: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::KvlistValue(
            opentelemetry_proto::tonic::common::v1::KeyValueList { values: kvs },
        )),
    }
}

fn array_any_value(values: Vec<AnyValue>) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::ArrayValue(
            opentelemetry_proto::tonic::common::v1::ArrayValue { values },
        )),
    }
}

fn attrs_to_any_value(attrs: &[KeyValue]) -> AnyValue {
    array_any_value(
        attrs.iter().map(|kv| {
            let mut inner = vec![
                KeyValue { key: "key".into(), value: Some(string_value(&kv.key)) },
            ];
            if let Some(ref v) = kv.value {
                inner.push(KeyValue { key: "value".into(), value: Some(v.clone()) });
            }
            kvlist_any_value(inner)
        }).collect()
    )
}

fn number_dp_to_any_value(
    dp: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint,
) -> AnyValue {
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;
    let mut kvs = Vec::new();
    if let Some(ref v) = dp.value {
        match v {
            NDPValue::AsDouble(d) => kvs.push(KeyValue { key: "asDouble".into(), value: Some(double_value(*d)) }),
            NDPValue::AsInt(i) => kvs.push(KeyValue { key: "asInt".into(), value: Some(int_value(*i)) }),
        }
    }
    if !dp.attributes.is_empty() {
        kvs.push(KeyValue { key: "attributes".into(), value: Some(attrs_to_any_value(&dp.attributes)) });
    }
    if dp.time_unix_nano != 0 {
        kvs.push(KeyValue { key: "timeUnixNano".into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
    }
    if dp.start_time_unix_nano != 0 {
        kvs.push(KeyValue { key: "startTimeUnixNano".into(), value: Some(string_value(dp.start_time_unix_nano.to_string())) });
    }
    kvlist_any_value(kvs)
}

fn sum_to_any_value(sum: &opentelemetry_proto::tonic::metrics::v1::Sum) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = sum.data_points.iter().map(number_dp_to_any_value).collect();
    kvs.push(KeyValue { key: "dataPoints".into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: "aggregationTemporality".into(), value: Some(int_value(sum.aggregation_temporality as i64)) });
    kvs.push(KeyValue { key: "isMonotonic".into(), value: Some(bool_value(sum.is_monotonic)) });
    kvlist_any_value(kvs)
}

fn gauge_to_any_value(gauge: &opentelemetry_proto::tonic::metrics::v1::Gauge) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = gauge.data_points.iter().map(number_dp_to_any_value).collect();
    kvs.push(KeyValue { key: "dataPoints".into(), value: Some(array_any_value(dps)) });
    kvlist_any_value(kvs)
}

fn histogram_to_any_value(hist: &opentelemetry_proto::tonic::metrics::v1::Histogram) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = hist.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: "attributes".into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: "timeUnixNano".into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: "count".into(), value: Some(string_value(dp.count.to_string())) });
        if let Some(sum) = dp.sum {
            m.push(KeyValue { key: "sum".into(), value: Some(double_value(sum)) });
        }
        if !dp.bucket_counts.is_empty() {
            m.push(KeyValue { key: "bucketCounts".into(), value: Some(array_any_value(
                dp.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
            )) });
        }
        if !dp.explicit_bounds.is_empty() {
            m.push(KeyValue { key: "explicitBounds".into(), value: Some(array_any_value(
                dp.explicit_bounds.iter().map(|b| double_value(*b)).collect()
            )) });
        }
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: "dataPoints".into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: "aggregationTemporality".into(), value: Some(int_value(hist.aggregation_temporality as i64)) });
    kvlist_any_value(kvs)
}

fn summary_to_any_value(summary: &opentelemetry_proto::tonic::metrics::v1::Summary) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = summary.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: "attributes".into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: "timeUnixNano".into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: "count".into(), value: Some(string_value(dp.count.to_string())) });
        m.push(KeyValue { key: "sum".into(), value: Some(double_value(dp.sum)) });
        let qvs: Vec<AnyValue> = dp.quantile_values.iter().map(|q| {
            kvlist_any_value(vec![
                KeyValue { key: "quantile".into(), value: Some(double_value(q.quantile)) },
                KeyValue { key: "value".into(), value: Some(double_value(q.value)) },
            ])
        }).collect();
        m.push(KeyValue { key: "quantileValues".into(), value: Some(array_any_value(qvs)) });
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: "dataPoints".into(), value: Some(array_any_value(dps)) });
    kvlist_any_value(kvs)
}

fn exp_histogram_to_any_value(
    exp: &opentelemetry_proto::tonic::metrics::v1::ExponentialHistogram,
) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = exp.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: "attributes".into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: "timeUnixNano".into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: "count".into(), value: Some(string_value(dp.count.to_string())) });
        if let Some(sum) = dp.sum {
            m.push(KeyValue { key: "sum".into(), value: Some(double_value(sum)) });
        }
        m.push(KeyValue { key: "scale".into(), value: Some(int_value(dp.scale as i64)) });
        m.push(KeyValue { key: "zeroCount".into(), value: Some(string_value(dp.zero_count.to_string())) });
        if let Some(ref pos) = dp.positive {
            m.push(KeyValue { key: "positive".into(), value: Some(kvlist_any_value(vec![
                KeyValue { key: "offset".into(), value: Some(int_value(pos.offset as i64)) },
                KeyValue { key: "bucketCounts".into(), value: Some(array_any_value(
                    pos.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
                )) },
            ])) });
        }
        if let Some(ref neg) = dp.negative {
            m.push(KeyValue { key: "negative".into(), value: Some(kvlist_any_value(vec![
                KeyValue { key: "offset".into(), value: Some(int_value(neg.offset as i64)) },
                KeyValue { key: "bucketCounts".into(), value: Some(array_any_value(
                    neg.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
                )) },
            ])) });
        }
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: "dataPoints".into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: "aggregationTemporality".into(), value: Some(int_value(exp.aggregation_temporality as i64)) });
    kvlist_any_value(kvs)
}

// -- Trait implementations --

macro_rules! impl_otel_event_traits {
    ($ty:ident, $proto_field:ident) => {
        impl ByteSizeOf for $ty {
            fn allocated_bytes(&self) -> usize {
                self.$proto_field.encoded_len()
                    + self
                        .resource
                        .as_ref()
                        .map_or(0, |r| r.encoded_len())
                    + self
                        .scope
                        .as_ref()
                        .map_or(0, |s| s.encoded_len())
                    + self.metadata.allocated_bytes()
            }
        }

        impl EstimatedJsonEncodedSizeOf for $ty {
            fn estimated_json_encoded_size_of(&self) -> JsonSize {
                // Approximate: proto encoded_len * 3 accounts for JSON overhead
                // (field names, quoting, braces). For OtelLog/OtelSpan this should
                // closely match `to_value_canonical().estimated_json_encoded_size_of()`.
                JsonSize::new(self.$proto_field.encoded_len() * 3)
            }
        }

        impl EventCount for $ty {
            fn event_count(&self) -> usize {
                1
            }
        }

        impl Finalizable for $ty {
            fn take_finalizers(&mut self) -> EventFinalizers {
                self.metadata.take_finalizers()
            }
        }

    };
}

impl_otel_event_traits!(OtelLog, record);
impl_otel_event_traits!(OtelSpan, span);
impl_otel_event_traits!(OtelMetric, metric);

impl GetEventCountTags for OtelSpan {
    fn get_tags(&self) -> TaggedEventsSent {
        TaggedEventsSent::new_unspecified()
    }
}

// Override GetEventCountTags for OtelLog with proper source/service extraction.
impl GetEventCountTags for OtelLog {
    fn get_tags(&self) -> TaggedEventsSent {
        use crate::config::telemetry;
        use vector_common::internal_event::OptionalTag;

        let source = if telemetry().tags().emit_source {
            self.metadata().source_id().cloned().into()
        } else {
            OptionalTag::Ignored
        };

        let service = if telemetry().tags().emit_service {
            self.resource_attrs.get("service.name")
                .and_then(|av| match &av.value {
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        s,
                    )) => Some(s.clone()),
                    _ => None,
                })
                .into()
        } else {
            OptionalTag::Ignored
        };

        TaggedEventsSent { source, service }
    }
}

// Override GetEventCountTags for OtelMetric with proper source/service extraction.
impl GetEventCountTags for OtelMetric {
    fn get_tags(&self) -> TaggedEventsSent {
        use crate::config::telemetry;
        use vector_common::internal_event::OptionalTag;

        let source = if telemetry().tags().emit_source {
            self.metadata().source_id().cloned().into()
        } else {
            OptionalTag::Ignored
        };

        let service = if telemetry().tags().emit_service {
            self.resource_attribute("service.name")
                .and_then(|av| match &av.value {
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        s,
                    )) => Some(s.clone()),
                    _ => None,
                })
                .into()
        } else {
            OptionalTag::Ignored
        };

        TaggedEventsSent { source, service }
    }
}

impl EventDataEq for OtelLog {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.record.body == other.record.body
            && self.record.severity_text == other.record.severity_text
            && self.record.severity_number == other.record.severity_number
            && self.record.time_unix_nano == other.record.time_unix_nano
            && self.record.flags == other.record.flags
            && self.record.trace_id == other.record.trace_id
            && self.record.span_id == other.record.span_id
            && self.record_attrs == other.record_attrs
            && self.record.dropped_attributes_count == other.record.dropped_attributes_count
            && self.resource == other.resource
            && self.resource_attrs == other.resource_attrs
            && self.scope == other.scope
            && self.scope_attrs == other.scope_attrs
    }
}

impl EventDataEq for OtelSpan {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.span == other.span
            && self.span_attrs == other.span_attrs
            && self.resource == other.resource
            && self.resource_attrs == other.resource_attrs
            && self.scope == other.scope
            && self.scope_attrs == other.scope_attrs
    }
}

impl EventDataEq for OtelMetric {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.metric == other.metric
            && self.dp_attrs == other.dp_attrs
            && self.resource == other.resource
            && self.resource_attrs == other.resource_attrs
            && self.scope == other.scope
            && self.scope_attrs == other.scope_attrs
    }
}

impl Default for OtelLog {
    fn default() -> Self {
        Self::new(LogRecord::default())
    }
}

impl From<&str> for OtelLog {
    fn from(s: &str) -> Self {
        Self::from_bytes(bytes::Bytes::from(s.to_owned()))
    }
}

impl From<String> for OtelLog {
    fn from(s: String) -> Self {
        Self::from_bytes(bytes::Bytes::from(s))
    }
}

impl From<bytes::Bytes> for OtelLog {
    fn from(b: bytes::Bytes) -> Self {
        Self::from_bytes(b)
    }
}

impl From<Value> for OtelLog {
    fn from(value: Value) -> Self {
        Self::from_value_map(value, EventMetadata::default())
    }
}

impl From<ObjectMap> for OtelLog {
    fn from(map: ObjectMap) -> Self {
        Self::from_value_map(Value::Object(map), EventMetadata::default())
    }
}

impl From<std::collections::BTreeMap<String, Value>> for OtelLog {
    fn from(map: std::collections::BTreeMap<String, Value>) -> Self {
        let obj: ObjectMap = map.into_iter().map(|(k, v)| (k.into(), v)).collect();
        Self::from(obj)
    }
}

impl From<std::collections::HashMap<vrl::prelude::KeyString, Value>> for OtelLog {
    fn from(map: std::collections::HashMap<vrl::prelude::KeyString, Value>) -> Self {
        let obj: ObjectMap = map.into_iter().collect();
        Self::from(obj)
    }
}

fn hex_encode_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Serialize for OtelLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use super::otel_json::*;

        let mut map = serializer.serialize_map(None)?;
        if let Some(ref body) = self.record.body {
            map.serialize_entry("body", &SerializableAnyValue(body))?;
        }
        if !self.record.severity_text.is_empty() {
            map.serialize_entry("severityText", &self.record.severity_text)?;
        }
        if self.record.severity_number != 0 {
            map.serialize_entry("severityNumber", &self.record.severity_number)?;
        }
        if self.record.time_unix_nano != 0 {
            map.serialize_entry("timeUnixNano", &self.record.time_unix_nano.to_string())?;
        }
        if self.record.observed_time_unix_nano != 0 {
            map.serialize_entry("observedTimeUnixNano", &self.record.observed_time_unix_nano.to_string())?;
        }
        if !self.record.trace_id.is_empty() {
            map.serialize_entry("traceId", &hex_encode_bytes(&self.record.trace_id))?;
        }
        if !self.record.span_id.is_empty() {
            map.serialize_entry("spanId", &hex_encode_bytes(&self.record.span_id))?;
        }
        if self.record.flags != 0 {
            map.serialize_entry("flags", &self.record.flags)?;
        }
        if !self.record_attrs.is_empty() {
            let kvs = self.record_attrs.to_key_values();
            map.serialize_entry("attributes", &SerializableAttributes(&kvs))?;
        }
        if let Some(res) = self.resource_proto() {
            map.serialize_entry("resource", &SerializableResource(&res))?;
        }
        if let Some(scope) = self.scope_proto() {
            map.serialize_entry("scope", &SerializableScope(&scope))?;
        }
        map.end()
    }
}

impl Serialize for OtelSpan {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use super::otel_json::*;

        let mut map = serializer.serialize_map(None)?;
        if !self.span.name.is_empty() {
            map.serialize_entry("name", &self.span.name)?;
        }
        if !self.span.trace_id.is_empty() {
            map.serialize_entry("traceId", &hex_encode_bytes(&self.span.trace_id))?;
        }
        if !self.span.span_id.is_empty() {
            map.serialize_entry("spanId", &hex_encode_bytes(&self.span.span_id))?;
        }
        if !self.span.parent_span_id.is_empty() {
            map.serialize_entry("parentSpanId", &hex_encode_bytes(&self.span.parent_span_id))?;
        }
        if self.span.kind != 0 {
            map.serialize_entry("kind", &self.span.kind)?;
        }
        if self.span.start_time_unix_nano != 0 {
            map.serialize_entry("startTimeUnixNano", &self.span.start_time_unix_nano.to_string())?;
        }
        if self.span.end_time_unix_nano != 0 {
            map.serialize_entry("endTimeUnixNano", &self.span.end_time_unix_nano.to_string())?;
        }
        if !self.span_attrs.is_empty() {
            let kvs = self.span_attrs.to_key_values();
            map.serialize_entry("attributes", &SerializableAttributes(&kvs))?;
        }
        if let Some(ref status) = self.span.status {
            map.serialize_entry("status", &serde_json::json!({
                "code": status.code,
                "message": status.message,
            }))?;
        }
        if self.span.flags != 0 {
            map.serialize_entry("flags", &self.span.flags)?;
        }
        if let Some(res) = self.resource_proto() {
            map.serialize_entry("resource", &SerializableResource(&res))?;
        }
        if let Some(scope) = self.scope_proto() {
            map.serialize_entry("scope", &SerializableScope(&scope))?;
        }
        map.end()
    }
}

impl Serialize for OtelMetric {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use opentelemetry_proto::tonic::metrics::v1::metric;
        use super::otel_json::*;

        let metric_with_attrs = self.metric_proto();

        let mut len = 1;
        if !metric_with_attrs.description.is_empty() { len += 1; }
        if !metric_with_attrs.unit.is_empty() { len += 1; }
        if metric_with_attrs.data.is_some() { len += 1; }
        if self.resource.is_some() { len += 1; }
        if self.scope.is_some() { len += 1; }

        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("name", &metric_with_attrs.name)?;
        if !metric_with_attrs.description.is_empty() {
            map.serialize_entry("description", &metric_with_attrs.description)?;
        }
        if !metric_with_attrs.unit.is_empty() {
            map.serialize_entry("unit", &metric_with_attrs.unit)?;
        }

        if let Some(ref data) = metric_with_attrs.data {
            match data {
                metric::Data::Sum(sum) => {
                    map.serialize_entry("sum", &SerializableSum(sum))?;
                }
                metric::Data::Gauge(gauge) => {
                    map.serialize_entry("gauge", &SerializableGauge(gauge))?;
                }
                metric::Data::Histogram(hist) => {
                    map.serialize_entry("histogram", &SerializableHistogram(hist))?;
                }
                metric::Data::Summary(summary) => {
                    map.serialize_entry("summary", &SerializableSummary(summary))?;
                }
                metric::Data::ExponentialHistogram(exp) => {
                    map.serialize_entry("exponentialHistogram", &SerializableExpHistogram(exp))?;
                }
            }
        }

        if self.resource.is_some() || !self.resource_attrs.is_empty() {
            let mut res = self.resource.clone().unwrap_or_default();
            res.attributes = self.resource_attrs.to_key_values();
            map.serialize_entry("resource", &SerializableResource(&res))?;
        }
        if self.scope.is_some() || !self.scope_attrs.is_empty() {
            let mut scope = self.scope.clone().unwrap_or_default();
            scope.attributes = self.scope_attrs.to_key_values();
            map.serialize_entry("scope", &SerializableScope(&scope))?;
        }
        map.end()
    }
}

impl std::fmt::Display for OtelMetric {
    /// Display in Prometheus-like text format:
    /// `TIMESTAMP NAMESPACE_NAME{TAGS} KIND VALUE`
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, value, timestamp) = self.extract_metric_data();
        if let Some(ts) = timestamp {
            write!(fmt, "{ts:?} ")?;
        }
        if let Some(ns) = self.namespace() {
            write!(fmt, "{ns}_")?;
        }
        write!(fmt, "{}", self.name())?;
        write!(fmt, "{{")?;
        if let Some(tags) = self.tags() {
            let mut first = true;
            for (tag, value) in tags.iter_single() {
                if !first {
                    write!(fmt, ",")?;
                }
                first = false;
                write!(fmt, "{tag}={value:?}")?;
            }
        }
        write!(fmt, "}}")?;
        let kind_char = match kind {
            super::MetricKind::Absolute => '=',
            super::MetricKind::Incremental => '+',
        };
        write!(fmt, " {kind_char} {value}")
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use lookup::event_path;

    #[test]
    fn otel_log_event_default_fields() {
        let event = OtelLog::new(LogRecord::default());
        assert_eq!(event.time_unix_nano(), 0);
        assert_eq!(event.severity_text(), "");
        assert!(event.body().is_none());
        assert!(event.trace_id().is_empty());
        assert!(event.attributes().is_empty());
        assert!(event.resource().is_none());
        assert!(event.scope().is_none());
    }

    #[test]
    fn otel_log_event_attribute_crud() {
        let mut event = OtelLog::new(LogRecord::default());

        let value = AnyValue {
            value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                "bar".to_string(),
            )),
        };

        assert!(event.attribute("foo").is_none());

        event.set_attribute("foo".to_string(), value.clone());
        assert_eq!(event.attribute("foo"), Some(&value));

        let new_value = AnyValue {
            value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::IntValue(42)),
        };
        event.set_attribute("foo".to_string(), new_value.clone());
        assert_eq!(event.attribute("foo"), Some(&new_value));

        let removed = event.remove_attribute("foo");
        assert_eq!(removed, Some(new_value));
        assert!(event.attribute("foo").is_none());
    }

    #[test]
    fn otel_log_event_resource_attribute() {
        let mut event = OtelLog::new(LogRecord::default());
        assert!(event.resource_attribute("host.name").is_none());

        let host = AnyValue {
            value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                "myhost".to_string(),
            )),
        };
        event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "host.name".to_string(),
                value: Some(host.clone()),
            }],
            dropped_attributes_count: 0,
        });
        assert_eq!(event.resource_attribute("host.name"), Some(&host));
    }

    #[test]
    fn otel_span_event_typed_accessors() {
        let span = Span {
            name: "test-span".to_string(),
            trace_id: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            span_id: vec![1, 2, 3, 4, 5, 6, 7, 8],
            parent_span_id: vec![],
            start_time_unix_nano: 1000,
            end_time_unix_nano: 2000,
            kind: 2, // SPAN_KIND_SERVER
            ..Default::default()
        };
        let event = OtelSpan::new(span);

        assert_eq!(event.name(), "test-span");
        assert_eq!(event.trace_id().len(), 16);
        assert_eq!(event.span_id().len(), 8);
        assert!(event.parent_span_id().is_empty());
        assert_eq!(event.start_time_unix_nano(), 1000);
        assert_eq!(event.end_time_unix_nano(), 2000);
        assert_eq!(event.kind(), 2);
    }

    #[test]
    fn otel_span_pre_epoch_timestamp_preserved_as_attribute() {
        use chrono::{TimeZone, Utc};
        // Pre-1970 timestamps cannot be represented in a u64 nanos field.
        // Confirm they are preserved as attributes rather than wrapping
        // to huge future values via an `i64 as u64` cast.
        let pre_epoch = Utc.with_ymd_and_hms(1960, 1, 1, 0, 0, 0).unwrap();
        let mut event = OtelSpan::new(Span::default());
        event.insert(vrl::event_path!("start_time"), Value::Timestamp(pre_epoch));
        event.insert(vrl::event_path!("new_field"), "v");

        // Native field must stay zero — no u64 wrap.
        assert_eq!(event.start_time_unix_nano(), 0);
        // Pre-epoch value is preserved as an attribute. OTLP AnyValue has
        // no native timestamp kind, so it round-trips as its RFC3339 string
        // representation.
        let back = event.get(vrl::event_path!("start_time"));
        let s = match back {
            Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap(),
            Some(Value::Timestamp(t)) => t.to_rfc3339(),
            other => panic!("expected Bytes or Timestamp, got {other:?}"),
        };
        assert!(
            s.starts_with("1960-01-01"),
            "pre-epoch start_time should survive as attribute; got {s:?}"
        );
    }

    #[test]
    fn otel_span_insert_preserves_native_fields() {
        let span = Span {
            name: "test-span".to_string(),
            trace_id: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            span_id: vec![1, 2, 3, 4, 5, 6, 7, 8],
            parent_span_id: vec![9, 10, 11, 12, 13, 14, 15, 16],
            start_time_unix_nano: 1000,
            end_time_unix_nano: 2000,
            kind: 2, // SPAN_KIND_SERVER
            ..Default::default()
        };
        let mut event = OtelSpan::new(span);

        // Trigger the round-trip: insert → to_value_canonical →
        // apply_value_map.
        event.insert(vrl::event_path!("new_field"), "new_value");

        // Ideal: native proto fields survive.
        assert_eq!(event.name(), "test-span");
        assert_eq!(event.trace_id().len(), 16);
        assert_eq!(event.span_id().len(), 8);
        assert_eq!(event.parent_span_id().len(), 8);
        assert_eq!(event.start_time_unix_nano(), 1000);
        assert_eq!(event.end_time_unix_nano(), 2000);
        assert_eq!(event.kind(), 2);
    }

    #[test]
    fn otel_metric_event_name_and_unit() {
        let metric = OtelMetricProto {
            name: "http.request.duration".to_string(),
            description: "Duration of HTTP requests".to_string(),
            unit: "ms".to_string(),
            ..Default::default()
        };
        let event = OtelMetric::new(metric);

        assert_eq!(event.name(), "http.request.duration");
        assert_eq!(event.description(), "Duration of HTTP requests");
        assert_eq!(event.unit(), "ms");
    }

    #[test]
    fn byte_size_of_non_zero() {
        let event = OtelLog::new(LogRecord {
            body: Some(AnyValue {
                value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                    "hello world".to_string(),
                )),
            }),
            ..Default::default()
        });
        assert!(event.allocated_bytes() > 0);
    }

    #[test]
    fn event_data_eq_works() {
        let a = OtelLog::new(LogRecord::default());
        let b = OtelLog::new(LogRecord::default());
        assert!(a.event_data_eq(&b));

        let mut c = OtelLog::new(LogRecord::default());
        c.record.severity_text = "ERROR".to_string();
        assert!(!a.event_data_eq(&c));
    }

    #[test]
    fn event_count_is_one() {
        assert_eq!(OtelLog::new(LogRecord::default()).event_count(), 1);
        assert_eq!(OtelSpan::new(Span::default()).event_count(), 1);
        assert_eq!(
            OtelMetric::new(OtelMetricProto::default()).event_count(),
            1
        );
    }

    #[test]
    fn from_parts_round_trip() {
        let record = LogRecord {
            severity_text: "INFO".to_string(),
            ..Default::default()
        };
        let resource = Some(Resource {
            attributes: vec![],
            dropped_attributes_count: 0,
        });
        let scope = Some(InstrumentationScope {
            name: "my-lib".to_string(),
            ..Default::default()
        });
        let metadata = EventMetadata::default();

        let event =
            OtelLog::from_parts(record.clone(), resource.clone(), scope.clone(), metadata);
        let (r, res, sc, _meta) = event.into_parts();
        assert_eq!(r.severity_text, "INFO");
        assert_eq!(res, resource);
        assert_eq!(sc.as_ref().map(|s| s.name.as_str()), Some("my-lib"));
    }

    #[test]
    fn legacy_layout_projects_fields() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        let mut event = OtelLog::new(LogRecord {
            body: Some(AnyValue {
                value: Some(Kind::StringValue("hello world".into())),
            }),
            severity_text: "ERROR".into(),
            severity_number: 17,
            time_unix_nano: 1_700_000_000_000_000_000,
            trace_id: vec![0xab, 0xcd],
            span_id: vec![0x12, 0x34],
            attributes: vec![KeyValue {
                key: "env".into(),
                value: Some(AnyValue {
                    value: Some(Kind::StringValue("prod".into())),
                }),
            }],
            ..Default::default()
        });
        event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(AnyValue {
                    value: Some(Kind::StringValue("my-svc".into())),
                }),
            }],
            dropped_attributes_count: 0,
        });
        event.set_scope(InstrumentationScope {
            name: "my-lib".into(),
            version: "1.0".into(),
            ..Default::default()
        });

        let value = event.to_value_canonical();
        let map = value.as_object().expect("expected object");
        assert_eq!(map.get("body").unwrap().as_str().unwrap(), "hello world");
        assert_eq!(map.get("severity_text").unwrap().as_str().unwrap(), "ERROR");
        assert_eq!(map.get("severity_number").unwrap().as_integer().unwrap(), 17);
        assert_eq!(map.get("time_unix_nano").unwrap().as_integer().unwrap(), 1_700_000_000_000_000_000);
        assert_eq!(map.get("trace_id").unwrap().as_str().unwrap(), "abcd");
        assert_eq!(map.get("span_id").unwrap().as_str().unwrap(), "1234");
        assert_eq!(map.get("env").unwrap().as_str().unwrap(), "prod");
        assert!(map.get("resource").unwrap().is_object());
        assert!(map.get("scope").unwrap().is_object());
    }

    #[test]
    fn body_string_returns_body_text() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        let event = OtelLog::new(LogRecord {
            body: Some(AnyValue {
                value: Some(Kind::StringValue("test body".into())),
            }),
            ..Default::default()
        });
        assert_eq!(event.body_string(), "test body");

        let empty = OtelLog::new(LogRecord::default());
        assert_eq!(empty.body_string(), "");

        let int_body = OtelLog::new(LogRecord {
            body: Some(AnyValue {
                value: Some(Kind::IntValue(42)),
            }),
            ..Default::default()
        });
        assert_eq!(int_body.body_string(), "42");
    }

    #[test]
    fn otel_log_serializes_as_otlp_json() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;
        let record = LogRecord {
            severity_text: "INFO".to_string(),
            body: Some(AnyValue {
                value: Some(Kind::StringValue("hello".to_string())),
            }),
            ..Default::default()
        };
        let resource = Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_string(),
                value: Some(AnyValue {
                    value: Some(Kind::StringValue("test-svc".to_string())),
                }),
            }],
            dropped_attributes_count: 0,
        });
        let event = OtelLog::from_parts(record, resource, None, EventMetadata::default());
        let json = serde_json::to_string(&event).expect("serialize");

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["body"]["stringValue"], "hello");
        assert_eq!(v["severityText"], "INFO");
        assert!(v["resource"]["attributes"].is_array());
        assert_eq!(v["resource"]["attributes"][0]["key"], "service.name");
        assert_eq!(v["resource"]["attributes"][0]["value"]["stringValue"], "test-svc");
    }

    #[test]
    fn metric_to_otel_metric_round_trip_counter() {
        use crate::event::{MetricKind, MetricValue};
        use chrono::Utc;

        let otel = OtelMetric::new_counter("requests_total", MetricKind::Incremental, 42.0)
            .with_namespace(Some("http"))
            .with_timestamp(Some(Utc::now()));

        assert_eq!(otel.name(), "requests_total");
        assert_eq!(otel.namespace(), Some("http"));
        assert_eq!(otel.kind(), MetricKind::Incremental);
        match otel.value() {
            MetricValue::Counter { value } => assert!((value - 42.0).abs() < f64::EPSILON),
            other => panic!("expected Counter, got {other:?}"),
        }
    }

    #[test]
    fn metric_to_otel_metric_round_trip_gauge() {
        use crate::event::{MetricKind, MetricValue};

        let otel = OtelMetric::new_gauge("temperature", 98.6);

        assert_eq!(otel.name(), "temperature");
        assert_eq!(otel.kind(), MetricKind::Absolute);
        match otel.value() {
            MetricValue::Gauge { value } => assert!((value - 98.6).abs() < f64::EPSILON),
            other => panic!("expected Gauge, got {other:?}"),
        }
    }

    #[test]
    fn metric_to_otel_metric_round_trip_histogram() {
        use crate::event::{MetricKind, MetricValue};
        use crate::event::metric::Bucket;

        let buckets = vec![
            Bucket { count: 10, upper_limit: 5.0 },
            Bucket { count: 20, upper_limit: 10.0 },
            Bucket { count: 5, upper_limit: f64::INFINITY },
        ];
        let otel = OtelMetric::new_histogram(
            "latency",
            MetricKind::Absolute,
            &buckets,
            35,
            150.0,
        );

        assert_eq!(otel.name(), "latency");
        match otel.value() {
            MetricValue::AggregatedHistogram { buckets, count, sum } => {
                assert_eq!(count, 35);
                assert!((sum - 150.0).abs() < f64::EPSILON);
                assert_eq!(buckets.len(), 3);
                assert_eq!(buckets[0].count, 10);
                assert!((buckets[0].upper_limit - 5.0).abs() < f64::EPSILON);
            }
            other => panic!("expected AggregatedHistogram, got {other:?}"),
        }
    }

    #[test]
    fn from_metric_for_event_produces_otel_metric() {
        use crate::event::Event;

        let otel = OtelMetric::new_gauge("test", 1.0);
        let event: Event = Event::Metric(otel);
        assert!(matches!(event, Event::Metric(_)), "expected Event::Metric, got {event:?}");

        let metric = event.try_into_otel_metric().expect("should convert back");
        assert_eq!(metric.name(), "test");
    }

    #[test]
    fn set_timestamp_and_try_set_timestamp() {
        use chrono::{TimeZone, Utc};

        let mut log = OtelLog::from("hello");
        assert!(log.get_timestamp().is_none());

        let ts1 = Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap();
        let old = log.set_timestamp(ts1);
        assert!(old.is_none());
        assert_eq!(log.get_timestamp(), Some(Value::Timestamp(ts1)));

        let ts2 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let old = log.set_timestamp(ts2);
        assert_eq!(old, Some(Value::Timestamp(ts1)));
        assert_eq!(log.get_timestamp(), Some(Value::Timestamp(ts2)));

        // try_set_timestamp is a no-op when already set
        let ts3 = Utc.with_ymd_and_hms(2026, 3, 3, 3, 3, 3).unwrap();
        log.try_set_timestamp(ts3);
        assert_eq!(log.get_timestamp(), Some(Value::Timestamp(ts2)));

        // try_set_timestamp works when cleared
        log.remove_timestamp();
        log.try_set_timestamp(ts3);
        assert_eq!(log.get_timestamp(), Some(Value::Timestamp(ts3)));
    }

    #[test]
    fn get_by_meaning_resolves_schema_meaning() {
        use std::sync::Arc;

        // Create an OtelLog and insert a field at a custom path
        let mut event = OtelLog::from("test body");
        event.insert(
            &vrl::path::parse_target_path("@timestamp").unwrap(),
            Value::Bytes("2001-02-03T04:05:06Z".into()),
        );

        // Set Vector namespace metadata
        event.metadata_mut().value_mut().insert(
            lookup::path!("vector", "ns"),
            Value::Bytes("true".into()),
        );

        // Set schema meaning: "timestamp" → "@timestamp"
        let schema = event
            .metadata()
            .schema_definition()
            .as_ref()
            .clone()
            .with_meaning(
                vrl::path::parse_target_path("@timestamp").unwrap(),
                "timestamp",
            );
        event
            .metadata_mut()
            .set_schema_definition(&Arc::new(schema));

        // get_by_meaning should resolve the schema path
        let val = event.get_by_meaning("timestamp");
        assert!(val.is_some(), "expected Some, got None");
        assert_eq!(val.unwrap(), Value::Bytes("2001-02-03T04:05:06Z".into()));

        // Non-existent meaning returns None
        assert!(event.get_by_meaning("nonexistent").is_none());
    }

    #[test]
    fn tag_value_searches_data_point_resource_and_scope() {
        use crate::event::MetricKind;

        let otel = OtelMetric::new_counter("test_metric", MetricKind::Incremental, 1.0)
            .with_namespace(Some("ns"))
            .with_tags(Some(
                vec![("env".to_string(), "prod".to_string())]
                    .into_iter()
                    .collect(),
            ));

        // Data point attribute lookup
        assert_eq!(otel.tag_value("env"), Some("prod".to_string()));

        // Resource attribute lookup (prefixed with "resource.")
        // from_metric_parts stores namespace in resource as "metric.namespace"
        // but other resource attrs are prefixed with "resource." in tags
        assert!(otel.tag_value("nonexistent").is_none());

        // Scope lookup
        let otel_with_scope = OtelMetric::from_parts(
            otel.metric.clone(),
            otel.resource.clone(),
            Some(InstrumentationScope {
                name: "my-scope".into(),
                version: "1.0".into(),
                ..Default::default()
            }),
            EventMetadata::default(),
        );
        assert_eq!(
            otel_with_scope.tag_value("scope.name"),
            Some("my-scope".to_string())
        );
        assert_eq!(
            otel_with_scope.tag_value("scope.version"),
            Some("1.0".to_string())
        );
    }

    #[test]
    fn tags_builds_metric_tags_from_proto() {
        use crate::event::MetricKind;

        let otel = OtelMetric::new_counter("test_metric", MetricKind::Incremental, 1.0)
            .with_tags(Some(
                vec![
                    ("env".to_string(), "prod".to_string()),
                    ("region".to_string(), "us-east".to_string()),
                ]
                .into_iter()
                .collect(),
            ));

        let tags = otel.tags().expect("should have tags");
        assert_eq!(tags.get("env"), Some("prod"));
        assert_eq!(tags.get("region"), Some("us-east"));

        // Empty metric has no tags
        let empty = OtelMetric::new_gauge("empty", 0.0);
        assert!(empty.tags().is_none());
    }

    #[test]
    fn new_counter_matches_from_metric_parts() {
        use crate::event::MetricKind;

        let direct = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0);
        let via_ctor = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0);

        assert_eq!(direct.name(), via_ctor.name());
        assert_eq!(direct.kind(), via_ctor.kind());
        assert_eq!(direct.value(), via_ctor.value());
    }

    #[test]
    fn new_gauge_matches_from_metric_parts() {
        let direct = OtelMetric::new_gauge("temperature", 98.6);
        let via_ctor = OtelMetric::new_gauge("temperature", 98.6);

        assert_eq!(direct.name(), via_ctor.name());
        assert_eq!(direct.kind(), via_ctor.kind());
        assert_eq!(direct.value(), via_ctor.value());
    }

    #[test]
    fn new_histogram_matches_from_metric_parts() {
        use crate::event::MetricKind;

        let buckets = crate::buckets![1.0 => 10, 5.0 => 20, 10.0 => 5];
        let direct = OtelMetric::new_histogram(
            "request_duration",
            MetricKind::Absolute,
            &buckets,
            35,
            8.0,
        );
        let via_ctor = OtelMetric::new_histogram(
            "request_duration",
            MetricKind::Absolute,
            &buckets,
            35,
            8.0,
        );

        assert_eq!(direct.name(), via_ctor.name());
        assert_eq!(direct.kind(), via_ctor.kind());
        assert_eq!(direct.value(), via_ctor.value());
    }

    #[test]
    fn new_summary_matches_from_metric_parts() {
        let quantiles = crate::quantiles![0.5 => 100.0, 0.99 => 200.0];
        let direct = OtelMetric::new_summary(
            "request_latency",
            &quantiles,
            50,
            4200.0,
        );
        let via_ctor = OtelMetric::new_summary(
            "request_latency",
            &quantiles,
            50,
            4200.0,
        );

        assert_eq!(direct.name(), via_ctor.name());
        assert_eq!(direct.value(), via_ctor.value());
    }

    #[test]
    fn from_tracing_event_produces_expected_fields() {
        // Verify OtelLog::from_tracing_event produces the expected field shape
        // (standard metadata: kind, level, module_path, target, plus timestamp).
        use tracing::info;
        use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

        struct Capture {
            from_otel: std::sync::Mutex<Option<OtelLog>>,
        }
        impl<S> Layer<S> for &'static Capture
        where
            S: tracing::Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                *self.from_otel.lock().unwrap() = Some(OtelLog::from_tracing_event(event));
            }
        }

        let cap: &'static Capture = Box::leak(Box::new(Capture {
            from_otel: std::sync::Mutex::new(None),
        }));

        let subscriber = tracing_subscriber::registry().with(cap);
        tracing::subscriber::with_default(subscriber, || {
            info!(message = "hello", count = 42, ready = true, "static msg");
        });

        let ol = cap.from_otel.lock().unwrap().clone().expect("captured");

        // Standard metadata fields are present.
        assert!(
            ol.get(vrl::event_path!("metadata", "level"))
                .and_then(|v| v.as_str().map(|s| s.into_owned()))
                .is_some(),
            "metadata.level must be present"
        );
        assert!(
            ol.get(vrl::event_path!("metadata", "target"))
                .and_then(|v| v.as_str().map(|s| s.into_owned()))
                .is_some(),
            "metadata.target must be present"
        );
        assert!(
            ol.get(vrl::event_path!("metadata", "kind"))
                .and_then(|v| v.as_str().map(|s| s.into_owned()))
                .is_some(),
            "metadata.kind must be present"
        );
        // Visitor records fields at top level.
        assert!(
            ol.get(vrl::event_path!("count")).and_then(|v| v.as_integer()).is_some(),
            "i64 field via record_i64"
        );
        assert!(
            ol.get(vrl::event_path!("ready")).and_then(|v| v.as_boolean()).is_some(),
            "bool field via record_bool"
        );
        // time_unix_nano is present as integer.
        assert!(
            ol.get(vrl::event_path!("time_unix_nano"))
                .map(|v| matches!(v, vrl::value::Value::Integer(_)))
                .unwrap_or(false),
            "OtelLog has time_unix_nano"
        );
    }

    /// Round-trip fidelity test: OtelLog → to_value_canonical → apply_value_map
    /// should produce an OtelLog equivalent to the starting one for field lookups.
    /// This protects against regressions when rewriting apply_value_map.
    #[test]
    fn insert_preserves_body_via_round_trip() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        let mut event = OtelLog::new(LogRecord {
            body: Some(AnyValue {
                value: Some(Kind::StringValue("hello".into())),
            }),
            ..Default::default()
        });
        // Insert triggers the full round-trip
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(
            event.get(vrl::event_path!("body")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("hello".to_string())
        );
        assert_eq!(
            event.get(vrl::event_path!("new_field")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("new_value".to_string())
        );
    }

    #[test]
    fn insert_preserves_source_type_resource_attr() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        let mut event = OtelLog::new(LogRecord::default());
        event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "source_type".into(),
                value: Some(AnyValue { value: Some(Kind::StringValue("syslog".into())) }),
            }],
            dropped_attributes_count: 0,
        });
        event.insert(vrl::event_path!("another"), "x");
        // source_type lives at canonical resource path
        assert_eq!(
            event.get(vrl::event_path!("resource", "source_type")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("syslog".to_string())
        );
    }

    #[test]
    fn otel_log_with_tags_array_preserves_field_access() {
        // OtelLog with "tags" array field, queried via get().
        let mut otel = OtelLog::from("msg");
        otel.insert(
            vrl::event_path!("tags"),
            Value::Array(vec![Value::Bytes("a:foo".into())]),
        );

        // Lookup via get()
        let tags = otel.get(vrl::event_path!("tags"));
        assert!(tags.is_some(), "tags field must be accessible after insert");
        match tags.unwrap() {
            Value::Array(arr) => {
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0].as_str().map(|s| s.into_owned()), Some("a:foo".to_string()));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn insert_preserves_other_resource_attrs() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        // Resource attrs OTHER than source_type/host.name — these end up
        // in the "resource" sub-object in legacy layout. The round-trip
        // through from_value_map drops these because from_value_map only
        // reads source_type/host from the top level.
        // This test documents the current (lossy) behavior — any rewrite
        // should preserve it (or intentionally improve it).
        let mut event = OtelLog::new(LogRecord::default());
        event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(AnyValue { value: Some(Kind::StringValue("my-svc".into())) }),
            }],
            dropped_attributes_count: 0,
        });
        // Before insert, we can read the resource attr via the resource sub-object
        let before = event.get(vrl::event_path!("resource", "service.name"))
            .and_then(|v| v.as_str().map(|s| s.into_owned()));
        assert_eq!(before, Some("my-svc".to_string()),
            "resource sub-object should be readable before insert");

        event.insert(vrl::event_path!("attr"), "val");

        // After insert: the current implementation's behavior (for regression detection)
        let after = event.get(vrl::event_path!("resource", "service.name"))
            .and_then(|v| v.as_str().map(|s| s.into_owned()));
        assert_eq!(after, before,
            "resource sub-object fidelity must be preserved across insert");
    }

    #[test]
    fn insert_preserves_host_resource_attr() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;

        let mut event = OtelLog::new(LogRecord::default());
        event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "host.name".into(),
                value: Some(AnyValue { value: Some(Kind::StringValue("srv01".into())) }),
            }],
            dropped_attributes_count: 0,
        });
        event.insert(vrl::event_path!("attr"), "val");
        // host.name at canonical resource path
        assert_eq!(
            event.get(vrl::event_path!("resource", "host.name")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("srv01".to_string())
        );
    }

    // --- Round-trip fidelity for native proto fields -----------------------
    //
    // `apply_value_map` extracts these fields symmetrically with
    // `to_value_canonical`, so a round-trip via `insert()` preserves them.

    #[test]
    fn insert_preserves_severity_text_via_round_trip() {
        let mut event = OtelLog::new(LogRecord {
            severity_text: "ERROR".into(),
            ..Default::default()
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(event.severity_text(), "ERROR");
    }

    #[test]
    fn insert_preserves_severity_number_via_round_trip() {
        let mut event = OtelLog::new(LogRecord {
            severity_number: 17,
            ..Default::default()
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(event.severity_number(), 17);
    }

    #[test]
    fn insert_preserves_trace_id_via_round_trip() {
        let trace_id_bytes = vec![
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11,
            0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
        ];
        let mut event = OtelLog::new(LogRecord {
            trace_id: trace_id_bytes.clone(),
            ..Default::default()
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(event.trace_id(), trace_id_bytes.as_slice());
    }

    #[test]
    fn insert_preserves_span_id_via_round_trip() {
        let span_id_bytes = vec![0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11];
        let mut event = OtelLog::new(LogRecord {
            span_id: span_id_bytes.clone(),
            ..Default::default()
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(event.span_id(), span_id_bytes.as_slice());
    }

    #[test]
    fn insert_preserves_corrupt_trace_id_as_attribute() {
        // If trace_id contains non-hex data (e.g. from a user-set attribute
        // via VRL), hex_decode fails and the value is preserved as an
        // attribute rather than dropped.
        let mut event = OtelLog::new(LogRecord::default());
        event.insert(vrl::event_path!("trace_id"), "not-hex-data");
        assert!(event.trace_id().is_empty(), "native field should stay empty for invalid hex");
        assert_eq!(
            event.get(vrl::event_path!("trace_id")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("not-hex-data".to_string()),
            "invalid hex should be preserved as an attribute"
        );
    }

    #[test]
    fn insert_preserves_scope_via_round_trip() {
        let mut event = OtelLog::new(LogRecord::default());
        event.set_scope(InstrumentationScope {
            name: "my-lib".into(),
            version: "1.2.3".into(),
            attributes: vec![KeyValue {
                key: "lib.lang".into(),
                value: Some(AnyValue {
                    value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue("rust".into())),
                }),
            }],
            dropped_attributes_count: 0,
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(
            event.get(vrl::event_path!("scope", "name")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("my-lib".to_string())
        );
        assert_eq!(
            event.get(vrl::event_path!("scope", "version")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            event.get(vrl::event_path!("scope", "attributes", "lib.lang")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("rust".to_string())
        );
    }

    #[test]
    fn insert_preserves_observed_time_unix_nano_via_round_trip() {
        let mut event = OtelLog::new(LogRecord {
            observed_time_unix_nano: 1_700_000_000_000_000_000,
            ..Default::default()
        });
        event.insert(vrl::event_path!("new_field"), "new_value");
        assert_eq!(event.record.observed_time_unix_nano, 1_700_000_000_000_000_000);
    }

    #[test]
    fn with_namespace_tags_timestamp_matches_from_metric_parts() {
        use crate::event::MetricKind;
        use chrono::{TimeZone, Utc};

        let ts = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let tags: super::super::MetricTags = vec![("env".to_string(), "prod".to_string())]
            .into_iter()
            .collect();

        let direct = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0)
            .with_namespace(Some("http"))
            .with_tags(Some(tags.clone()))
            .with_timestamp(Some(ts));

        let via_ctor = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0)
            .with_namespace(Some("http"))
            .with_tags(Some(tags))
            .with_timestamp(Some(ts));

        assert_eq!(direct.name(), via_ctor.name());
        assert_eq!(direct.namespace(), via_ctor.namespace());
        assert_eq!(direct.kind(), via_ctor.kind());
        assert_eq!(direct.value(), via_ctor.value());
        assert_eq!(direct.timestamp(), via_ctor.timestamp());
        assert_eq!(direct.tag_value("env"), via_ctor.tag_value("env"));
    }

    #[test]
    fn with_tags_preserves_multi_value() {
        use crate::event::{MetricKind, metric::TagValue};
        use opentelemetry_proto::tonic::common::v1::any_value;

        // Build MetricTags with a single-value "host" and a multi-value
        // "env" tag ({"prod", None, "staging"}).
        let mut tags = super::super::MetricTags::default();
        tags.replace("host".to_string(), TagValue::Value("srv01".to_string()));
        tags.set_multi_value(
            "env".to_string(),
            vec![
                TagValue::Value("prod".to_string()),
                TagValue::Bare,
                TagValue::Value("staging".to_string()),
            ],
        );

        let direct = OtelMetric::new_counter("requests", MetricKind::Incremental, 1.0)
            .with_tags(Some(tags.clone()));

        let via_ctor = OtelMetric::new_counter("requests", MetricKind::Incremental, 1.0)
            .with_tags(Some(tags));

        // The key "env" must encode as an ArrayValue in both paths.
        let find_env = |m: &OtelMetric| -> any_value::Value {
            m.first_dp_attrs()
                .and_then(|attrs| attrs.get("env"))
                .and_then(|av| av.value.clone())
                .expect("env attribute missing")
        };
        match (find_env(&direct), find_env(&via_ctor)) {
            (any_value::Value::ArrayValue(a), any_value::Value::ArrayValue(b)) => {
                assert_eq!(a.values.len(), 3);
                assert_eq!(a.values.len(), b.values.len());
            }
            other => panic!("expected ArrayValue on both sides, got {other:?}"),
        }
        assert_eq!(direct.tag_value("host"), via_ctor.tag_value("host"));
    }

    #[test]
    fn multi_segment_resource_get_insert_remove() {
        let mut log = OtelLog::new(LogRecord::default());
        log.set_resource_attribute("service.name".to_string(), string_value("my-svc"));
        log.set_resource_attribute("source_type".to_string(), string_value("syslog"));

        // get resource attribute
        assert_eq!(
            log.get(event_path!("resource", "service.name")),
            Some(Value::Bytes("my-svc".into()))
        );
        // source_type visible at canonical resource path
        assert_eq!(
            log.get(event_path!("resource", "source_type")),
            Some(Value::Bytes("syslog".into()))
        );

        // insert resource attribute
        log.insert(event_path!("resource", "deployment.environment"), "prod");
        assert_eq!(
            log.get(event_path!("resource", "deployment.environment")),
            Some(Value::Bytes("prod".into()))
        );

        // remove resource attribute
        let removed = log.remove(event_path!("resource", "service.name"));
        assert_eq!(removed, Some(Value::Bytes("my-svc".into())));
        assert_eq!(log.get(event_path!("resource", "service.name")), None);
    }

    #[test]
    fn multi_segment_scope_get_insert_remove() {
        let mut log = OtelLog::new(LogRecord::default());
        log.scope = Some(InstrumentationScope {
            name: "my-lib".to_string(),
            version: "1.0".to_string(),
            attributes: vec![],
            dropped_attributes_count: 0,
        });

        assert_eq!(
            log.get(event_path!("scope", "name")),
            Some(Value::Bytes("my-lib".into()))
        );
        assert_eq!(
            log.get(event_path!("scope", "version")),
            Some(Value::Bytes("1.0".into()))
        );

        log.insert(event_path!("scope", "name"), "updated-lib");
        assert_eq!(
            log.get(event_path!("scope", "name")),
            Some(Value::Bytes("updated-lib".into()))
        );

        log.insert(event_path!("scope", "attributes", "env"), "prod");
        assert_eq!(
            log.get(event_path!("scope", "attributes", "env")),
            Some(Value::Bytes("prod".into()))
        );

        let removed = log.remove(event_path!("scope", "name"));
        assert_eq!(removed, Some(Value::Bytes("updated-lib".into())));
        assert_eq!(log.get(event_path!("scope", "name")), None);
    }

    #[test]
    fn multi_segment_nested_attribute_get_insert_remove() {
        let mut log = OtelLog::new(LogRecord::default());

        // Insert nested attribute (like kubernetes metadata)
        log.insert(event_path!("kubernetes", "pod_name"), "sandbox0");
        assert_eq!(
            log.get(event_path!("kubernetes", "pod_name")),
            Some(Value::Bytes("sandbox0".into()))
        );

        // 3-segment path
        log.insert(event_path!("kubernetes", "pod_labels", "app"), "my-app");
        assert_eq!(
            log.get(event_path!("kubernetes", "pod_labels", "app")),
            Some(Value::Bytes("my-app".into()))
        );

        // Remove nested path
        let removed = log.remove(event_path!("kubernetes", "pod_name"));
        assert_eq!(removed, Some(Value::Bytes("sandbox0".into())));
        assert_eq!(log.get(event_path!("kubernetes", "pod_name")), None);
        // pod_labels still there
        assert_eq!(
            log.get(event_path!("kubernetes", "pod_labels", "app")),
            Some(Value::Bytes("my-app".into()))
        );
    }

    #[test]
    fn multi_segment_prune_removes_empty_parents() {
        let mut log = OtelLog::new(LogRecord::default());
        log.insert(event_path!("nested", "only_child"), "val");
        assert_eq!(
            log.get(event_path!("nested", "only_child")),
            Some(Value::Bytes("val".into()))
        );

        let removed = log.remove_prune(event_path!("nested", "only_child"), true);
        assert_eq!(removed, Some(Value::Bytes("val".into())));
        // After prune, "nested" attribute itself should be gone
        assert_eq!(log.get(event_path!("nested")), None);
    }

    #[test]
    fn span_fast_path_single_and_multi_segment() {
        use opentelemetry_proto::tonic::trace::v1::Span;

        let mut span_event = OtelSpan::new(Span {
            name: "GET /api".to_string(),
            trace_id: vec![0xAA; 16],
            span_id: vec![0xBB; 8],
            kind: 2,
            ..Default::default()
        });
        span_event.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_string(),
                value: Some(string_value("my-svc")),
            }],
            dropped_attributes_count: 0,
        });

        // Single-segment gets
        assert_eq!(
            span_event.get(event_path!("name")),
            Some(Value::Bytes("GET /api".into()))
        );
        assert_eq!(
            span_event.get(event_path!("kind")),
            Some(Value::Integer(2))
        );

        // Multi-segment resource get
        assert_eq!(
            span_event.get(event_path!("resource", "service.name")),
            Some(Value::Bytes("my-svc".into()))
        );

        // Insert resource attribute
        span_event.insert(event_path!("resource", "deployment.env"), "staging");
        assert_eq!(
            span_event.get(event_path!("resource", "deployment.env")),
            Some(Value::Bytes("staging".into()))
        );

        // Insert nested attribute
        span_event.insert(event_path!("http", "method"), "GET");
        assert_eq!(
            span_event.get(event_path!("http", "method")),
            Some(Value::Bytes("GET".into()))
        );

        // Insert/get status sub-fields
        span_event.insert(event_path!("status"), Value::Object({
            let mut m = ObjectMap::new();
            m.insert("message".into(), Value::Bytes("OK".into()));
            m.insert("code".into(), Value::Integer(1));
            m
        }));
        assert_eq!(
            span_event.get(event_path!("status", "code")),
            Some(Value::Integer(1))
        );
        assert_eq!(
            span_event.get(event_path!("status", "message")),
            Some(Value::Bytes("OK".into()))
        );
    }

    #[test]
    fn otel_span_remove_single_and_multi_segment() {
        use opentelemetry_proto::tonic::trace::v1::Span;

        let mut span = OtelSpan::new(Span {
            name: "GET /api".to_string(),
            trace_id: vec![0xAA; 16],
            span_id: vec![0xBB; 8],
            kind: 2,
            flags: 1,
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 2_000_000_000,
            ..Default::default()
        });
        span.set_attribute("http.method".to_string(), string_value("GET"));
        span.set_resource(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_string(),
                value: Some(string_value("my-svc")),
            }],
            dropped_attributes_count: 0,
        });
        span.insert(event_path!("status"), Value::Object({
            let mut m = ObjectMap::new();
            m.insert("message".into(), Value::Bytes("OK".into()));
            m.insert("code".into(), Value::Integer(1));
            m
        }));

        // Remove single-segment proto field
        let removed = span.remove(event_path!("name"));
        assert_eq!(removed, Some(Value::Bytes("GET /api".into())));
        assert_eq!(span.name(), "");
        assert_eq!(span.remove(event_path!("name")), None);

        // Remove kind
        let removed = span.remove(event_path!("kind"));
        assert_eq!(removed, Some(Value::Integer(2)));
        assert_eq!(span.kind(), 0);

        // Remove flags
        let removed = span.remove(event_path!("flags"));
        assert_eq!(removed, Some(Value::Integer(1)));

        // Remove start_time_unix_nano
        let removed = span.remove(event_path!("start_time_unix_nano"));
        assert_eq!(removed, Some(Value::Integer(1_000_000_000)));
        assert_eq!(span.start_time_unix_nano(), 0);

        // Remove end_time via alias
        let removed = span.remove(event_path!("end_time"));
        assert!(removed.is_some());
        assert_eq!(span.end_time_unix_nano(), 0);

        // Remove span attribute
        let removed = span.remove(event_path!("http.method"));
        assert_eq!(removed, Some(Value::Bytes("GET".into())));
        assert_eq!(span.attribute("http.method"), None);

        // Remove resource attribute
        let removed = span.remove(event_path!("resource", "service.name"));
        assert_eq!(removed, Some(Value::Bytes("my-svc".into())));

        // Remove status.message sub-field
        let removed = span.remove(event_path!("status", "message"));
        assert_eq!(removed, Some(Value::Bytes("OK".into())));
        assert_eq!(span.status().unwrap().message, "");

        // Remove status.code sub-field
        let removed = span.remove(event_path!("status", "code"));
        assert_eq!(removed, Some(Value::Integer(1)));

        // Remove entire status
        span.insert(event_path!("status"), Value::Object({
            let mut m = ObjectMap::new();
            m.insert("code".into(), Value::Integer(2));
            m
        }));
        let removed = span.remove(event_path!("status"));
        assert!(removed.is_some());
        assert!(span.status().is_none());

        // Remove trace_id
        let removed = span.remove(event_path!("trace_id"));
        assert!(removed.is_some());
        assert!(span.trace_id().is_empty());

        // Remove with prune: nested attribute is cleaned up
        let mut span2 = OtelSpan::new(Span::default());
        span2.insert(event_path!("nested", "only_child"), "value");
        assert!(span2.attribute("nested").is_some());
        let removed = span2.remove_prune(event_path!("nested", "only_child"), true);
        assert_eq!(removed, Some(Value::Bytes("value".into())));
        assert!(span2.attribute("nested").is_none(), "prune should remove empty parent");
    }

    #[test]
    fn serialize_produces_otlp_json_for_log() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as Kind;
        let record = LogRecord {
            severity_text: "WARN".into(),
            severity_number: 13,
            time_unix_nano: 1_000_000_000,
            observed_time_unix_nano: 2_000_000_000,
            trace_id: vec![0xab; 16],
            span_id: vec![0xcd; 8],
            body: Some(AnyValue { value: Some(Kind::StringValue("test body".into())) }),
            ..Default::default()
        };
        let resource = Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(AnyValue { value: Some(Kind::StringValue("svc".into())) }),
            }],
            dropped_attributes_count: 0,
        });
        let scope = Some(InstrumentationScope {
            name: "my-lib".into(),
            version: "1.0".into(),
            ..Default::default()
        });
        let mut log = OtelLog::from_parts(record, resource, scope, EventMetadata::default());
        log.record_attrs.insert("custom.attr".into(), string_value("val"));

        let json: serde_json::Value = serde_json::to_value(&log).unwrap();

        // OTLP/JSON camelCase field names
        assert_eq!(json["body"]["stringValue"], "test body");
        assert_eq!(json["severityText"], "WARN");
        assert_eq!(json["severityNumber"], 13);
        assert_eq!(json["timeUnixNano"], "1000000000");
        assert_eq!(json["observedTimeUnixNano"], "2000000000");
        assert_eq!(json["traceId"], "abababababababababababababababab");
        assert_eq!(json["spanId"], "cdcdcdcdcdcdcdcd");

        // Attributes as OTLP array of {key, value}
        let attrs = json["attributes"].as_array().expect("attributes array");
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0]["key"], "custom.attr");
        assert_eq!(attrs[0]["value"]["stringValue"], "val");

        // Resource with nested attributes array
        let res_attrs = json["resource"]["attributes"].as_array().expect("resource attributes");
        assert_eq!(res_attrs[0]["key"], "service.name");
        assert_eq!(res_attrs[0]["value"]["stringValue"], "svc");

        // Scope
        assert_eq!(json["scope"]["name"], "my-lib");
        assert_eq!(json["scope"]["version"], "1.0");
    }

    #[test]
    fn serialize_produces_otlp_json_for_span() {
        use opentelemetry_proto::tonic::trace::v1::Status;
        let span = Span {
            name: "GET /api".into(),
            trace_id: vec![0x11; 16],
            span_id: vec![0x22; 8],
            parent_span_id: vec![0x33; 8],
            start_time_unix_nano: 100,
            end_time_unix_nano: 200,
            kind: 2,
            status: Some(Status { code: 1, message: "OK".into() }),
            ..Default::default()
        };
        let resource = Some(Resource {
            attributes: vec![KeyValue {
                key: "host".into(),
                value: Some(AnyValue { value: Some(OtelValueKind::StringValue("box1".into())) }),
            }],
            dropped_attributes_count: 0,
        });
        let mut otel_span = OtelSpan::from_parts(span, resource, None, EventMetadata::default());
        otel_span.span_attrs.insert("http.method".into(), string_value("GET"));

        let json: serde_json::Value = serde_json::to_value(&otel_span).unwrap();

        // OTLP/JSON camelCase field names
        assert_eq!(json["name"], "GET /api");
        assert_eq!(json["traceId"], "11111111111111111111111111111111");
        assert_eq!(json["spanId"], "2222222222222222");
        assert_eq!(json["parentSpanId"], "3333333333333333");
        assert_eq!(json["startTimeUnixNano"], "100");
        assert_eq!(json["endTimeUnixNano"], "200");
        assert_eq!(json["kind"], 2);

        // Status
        assert_eq!(json["status"]["code"], 1);
        assert_eq!(json["status"]["message"], "OK");

        // Attributes as OTLP array
        let attrs = json["attributes"].as_array().expect("attributes array");
        assert_eq!(attrs[0]["key"], "http.method");
        assert_eq!(attrs[0]["value"]["stringValue"], "GET");

        // Resource
        let res_attrs = json["resource"]["attributes"].as_array().expect("resource attributes");
        assert_eq!(res_attrs[0]["key"], "host");
        assert_eq!(res_attrs[0]["value"]["stringValue"], "box1");
    }

    #[test]
    fn otel_metric_add_sum() {
        use crate::event::MetricKind;
        let mut m1 = OtelMetric::new_counter("c", MetricKind::Incremental, 10.0);
        let m2 = OtelMetric::new_counter("c", MetricKind::Incremental, 5.0);
        assert!(m1.add(&m2));
        assert_eq!(m1.first_value_as_f64(), Some(15.0));
    }

    #[test]
    fn otel_metric_add_gauge() {
        let mut m1 = OtelMetric::new_gauge("g", 10.0);
        let m2 = OtelMetric::new_gauge("g", 3.0);
        assert!(m1.add(&m2));
        assert_eq!(m1.first_value_as_f64(), Some(13.0));
    }

    #[test]
    fn otel_metric_add_mismatched_types() {
        use crate::event::MetricKind;
        let mut m1 = OtelMetric::new_counter("c", MetricKind::Incremental, 10.0);
        let m2 = OtelMetric::new_gauge("g", 5.0);
        assert!(!m1.add(&m2));
        assert_eq!(m1.first_value_as_f64(), Some(10.0));
    }

    #[test]
    fn otel_metric_subtract_sum() {
        use crate::event::MetricKind;
        let mut m1 = OtelMetric::new_counter("c", MetricKind::Incremental, 10.0);
        let m2 = OtelMetric::new_counter("c", MetricKind::Incremental, 3.0);
        assert!(m1.subtract(&m2));
        assert_eq!(m1.first_value_as_f64(), Some(7.0));
    }

    #[test]
    fn otel_metric_subtract_sum_underflow() {
        use crate::event::MetricKind;
        let mut m1 = OtelMetric::new_counter("c", MetricKind::Incremental, 3.0);
        let m2 = OtelMetric::new_counter("c", MetricKind::Incremental, 10.0);
        assert!(!m1.subtract(&m2));
        assert_eq!(m1.first_value_as_f64(), Some(3.0));
    }

    #[test]
    fn otel_metric_zero() {
        use crate::event::MetricKind;
        let mut m = OtelMetric::new_counter("c", MetricKind::Incremental, 42.0);
        m.zero();
        assert_eq!(m.first_value_as_f64(), Some(0.0));
    }

    #[test]
    fn otel_metric_is_delta() {
        use crate::event::MetricKind;
        let delta = OtelMetric::new_counter("c", MetricKind::Incremental, 1.0);
        let cumulative = OtelMetric::new_counter("c", MetricKind::Absolute, 1.0);
        let gauge = OtelMetric::new_gauge("g", 1.0);
        assert!(delta.is_delta());
        assert!(!cumulative.is_delta());
        assert!(!gauge.is_delta());
    }

    #[test]
    fn otel_metric_is_cumulative() {
        use crate::event::MetricKind;
        let delta = OtelMetric::new_counter("c", MetricKind::Incremental, 1.0);
        let cumulative = OtelMetric::new_counter("c", MetricKind::Absolute, 1.0);
        let gauge = OtelMetric::new_gauge("g", 1.0);
        assert!(!delta.is_cumulative());
        assert!(cumulative.is_cumulative());
        assert!(!gauge.is_cumulative(), "Gauge has no temporality per OTel spec");
    }

    #[test]
    fn otel_metric_has_temporality() {
        use crate::event::MetricKind;
        let counter = OtelMetric::new_counter("c", MetricKind::Incremental, 1.0);
        let gauge = OtelMetric::new_gauge("g", 1.0);
        assert!(counter.has_temporality());
        assert!(!gauge.has_temporality());
    }

    #[test]
    fn otel_metric_add_histogram() {
        use crate::event::{MetricKind, MetricValue, metric::Bucket};
        let mut m1 = OtelMetric::new_histogram(
            "h",
            MetricKind::Incremental,
            &[Bucket { upper_limit: 1.0, count: 5 }, Bucket { upper_limit: 2.0, count: 10 }],
            15,
            25.0,
        );
        let m2 = OtelMetric::new_histogram(
            "h",
            MetricKind::Incremental,
            &[Bucket { upper_limit: 1.0, count: 3 }, Bucket { upper_limit: 2.0, count: 7 }],
            10,
            15.0,
        );
        assert!(m1.add(&m2));
        let (_, value, _) = m1.extract_metric_data();
        if let MetricValue::AggregatedHistogram { buckets, count, sum } = value {
            assert_eq!(count, 25);
            assert_eq!(sum, 40.0);
            assert_eq!(buckets[0].count, 8);
            assert_eq!(buckets[1].count, 17);
        } else {
            panic!("expected AggregatedHistogram");
        }
    }
}
