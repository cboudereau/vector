use std::collections::BTreeSet;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, InstrumentationScope, KeyValue, any_value::Value as OtelValueKind,
};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use opentelemetry_proto::tonic::metrics::v1::Metric as OtelMetricProto;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq,
    byte_size_of::ByteSizeOf,
    finalization::{EventFinalizers, Finalizable},
    internal_event::TaggedEventsSent,
    json_size::JsonSize,
    request_metadata::GetEventCountTags,
};
use vrl::value::{ObjectMap, Value};

use super::TraceEvent;

use super::{
    BatchNotifier, EstimatedJsonEncodedSizeOf, EventFinalizer, EventMetadata, LogEvent,
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

fn hex_encode(bytes: &[u8]) -> Value {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    Value::Bytes(s.into())
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

/// Hoist well-known log_schema fields from `Resource` to top-level in the output map.
/// `source_type` → top-level `source_type`, `host.name` → top-level `host`.
/// Remaining resource attributes stay in a `resource` sub-object.
fn hoist_resource_fields(resource: &Option<Resource>, map: &mut ObjectMap) {
    if let Some(resource) = resource {
        let mut res_map = kvlist_to_object_map(&resource.attributes);
        if resource.dropped_attributes_count != 0 {
            res_map.insert(
                "dropped_attributes_count".into(),
                Value::Integer(resource.dropped_attributes_count as i64),
            );
        }
        if let Some(v) = res_map.remove("source_type") {
            map.entry("source_type".into()).or_insert(v);
        }
        if let Some(v) = res_map.remove("host.name") {
            map.entry("host".into()).or_insert(v);
        }
        if !res_map.is_empty() {
            map.insert("resource".into(), Value::Object(res_map));
        }
    }
}

/// Convert scope into a `scope` sub-object in the output map (only if non-empty).
fn hoist_scope_fields(scope: &Option<InstrumentationScope>, map: &mut ObjectMap) {
    if let Some(scope) = scope {
        let mut scope_map = ObjectMap::new();
        if !scope.name.is_empty() {
            scope_map.insert("name".into(), Value::Bytes(scope.name.clone().into()));
        }
        if !scope.version.is_empty() {
            scope_map.insert("version".into(), Value::Bytes(scope.version.clone().into()));
        }
        if !scope.attributes.is_empty() {
            scope_map.insert(
                "attributes".into(),
                Value::Object(kvlist_to_object_map(&scope.attributes)),
            );
        }
        if !scope_map.is_empty() {
            map.insert("scope".into(), Value::Object(scope_map));
        }
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

fn attribute_value<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a AnyValue> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
}

fn set_attribute(attrs: &mut Vec<KeyValue>, key: String, value: AnyValue) {
    if let Some(kv) = attrs.iter_mut().find(|kv| kv.key == key) {
        kv.value = Some(value);
    } else {
        attrs.push(KeyValue {
            key,
            value: Some(value),
        });
    }
}

fn remove_attribute(attrs: &mut Vec<KeyValue>, key: &str) -> Option<AnyValue> {
    if let Some(pos) = attrs.iter().position(|kv| kv.key == key) {
        attrs.remove(pos).value
    } else {
        None
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

/// Extract a `Timestamp` value from the map, trying the log schema timestamp
/// key (`"timestamp"`) first, then the common `"@timestamp"` variant.
/// Non-Timestamp values are preserved in the map. Returns nanoseconds.
struct TimestampExtract {
    nanos: u64,
    overflow_rfc3339: Option<String>,
}

fn extract_timestamp_nanos(map: &mut ObjectMap) -> TimestampExtract {
    for key in &["timestamp", "@timestamp"] {
        match map.remove(*key) {
            Some(Value::Timestamp(ts)) => {
                match ts.timestamp_nanos_opt() {
                    Some(n) => {
                        return TimestampExtract { nanos: n as u64, overflow_rfc3339: None };
                    }
                    None => {
                        let rfc = ts.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
                        return TimestampExtract { nanos: 0, overflow_rfc3339: Some(rfc) };
                    }
                }
            }
            Some(other) => {
                map.insert((*key).into(), other);
            }
            None => {}
        }
    }
    TimestampExtract { nanos: 0, overflow_rfc3339: None }
}

// -- OtelLog --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelLog {
    pub(crate) record: LogRecord,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelLog {
    pub fn new(record: LogRecord) -> Self {
        Self {
            record,
            resource: None,
            scope: None,
            metadata: EventMetadata::default(),
        }
    }

    /// Convert a legacy `LogEvent` into an `OtelLog`.
    ///
    /// The `LogEvent`'s value tree is stored as a kvlist body.
    /// Known fields (`message`, `timestamp`, `severity_text`, etc.) are
    /// extracted into their corresponding `LogRecord` fields.
    pub fn from_log_event(log: LogEvent) -> Self {
        let (value, metadata) = log.into_parts();
        match value {
            Value::Object(mut map) => {
                let body = map.remove("message").map(|v| vrl_value_to_any_value(&v));
                let ts_extract = extract_timestamp_nanos(&mut map);
                let time_unix_nano = ts_extract.nanos;

                // Route well-known log_schema fields back to resource attributes
                // so that from_log_event ↔ to_log_event round-trips are lossless.
                let mut resource_attrs: Vec<KeyValue> = Vec::new();
                if let Some(v) = map.remove("source_type") {
                    resource_attrs.push(KeyValue {
                        key: "source_type".to_string(),
                        value: Some(vrl_value_to_any_value(&v)),
                    });
                }
                if let Some(v) = map.remove("host") {
                    resource_attrs.push(KeyValue {
                        key: "host.name".to_string(),
                        value: Some(vrl_value_to_any_value(&v)),
                    });
                }
                let resource = if resource_attrs.is_empty() {
                    None
                } else {
                    Some(Resource {
                        attributes: resource_attrs,
                        dropped_attributes_count: 0,
                    })
                };

                let mut attributes: Vec<KeyValue> = map
                    .into_iter()
                    .map(|(k, v)| KeyValue {
                        key: k.to_string(),
                        value: Some(vrl_value_to_any_value(&v)),
                    })
                    .collect();
                if let Some(ref overflow_ts) = ts_extract.overflow_rfc3339 {
                    attributes.push(KeyValue {
                        key: "vector.timestamp_overflow".to_string(),
                        value: Some(string_value(overflow_ts)),
                    });
                }
                Self {
                    record: LogRecord {
                        body,
                        time_unix_nano,
                        attributes,
                        ..Default::default()
                    },
                    resource,
                    scope: None,
                    metadata,
                }
            }
            other => {
                let body = vrl_value_to_any_value(&other);
                Self {
                    record: LogRecord {
                        body: Some(body),
                        ..Default::default()
                    },
                    resource: None,
                    scope: None,
                    metadata,
                }
            }
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
            resource: None,
            scope: None,
            metadata: EventMetadata::default(),
        }
    }

    /// Create an `OtelLog` from a JSON value, setting `record.body` to a kvlist
    /// if the value is an object, or a string/int/float/bool/array otherwise.
    pub fn from_json_value(value: serde_json::Value) -> Self {
        let body = json_to_any_value(value);
        Self {
            record: LogRecord {
                body: Some(body),
                ..Default::default()
            },
            resource: None,
            scope: None,
            metadata: EventMetadata::default(),
        }
    }

    pub fn from_parts(
        record: LogRecord,
        resource: Option<Resource>,
        scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        Self {
            record,
            resource,
            scope,
            metadata,
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        LogRecord,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        (self.record, self.resource, self.scope, self.metadata)
    }

    pub fn record(&self) -> &LogRecord {
        &self.record
    }

    pub fn record_mut(&mut self) -> &mut LogRecord {
        &mut self.record
    }

    pub fn resource(&self) -> Option<&Resource> {
        self.resource.as_ref()
    }

    pub fn set_resource(&mut self, resource: Resource) {
        self.resource = Some(resource);
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, scope: InstrumentationScope) {
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

    pub fn attribute(&self, key: &str) -> Option<&AnyValue> {
        attribute_value(&self.record.attributes, key)
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
        set_attribute(&mut self.record.attributes, key, value);
    }

    pub fn remove_attribute(&mut self, key: &str) -> Option<AnyValue> {
        remove_attribute(&mut self.record.attributes, key)
    }

    pub fn attributes(&self) -> &[KeyValue] {
        &self.record.attributes
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, key))
    }

    /// Ensure the resource object exists, creating it if absent.
    pub fn resource_mut(&mut self) -> &mut Resource {
        self.resource.get_or_insert_with(|| Resource {
            attributes: Vec::new(),
            dropped_attributes_count: 0,
        })
    }

    /// Set a resource attribute (e.g. `host.name`, `source_type`).
    pub fn set_resource_attribute(&mut self, key: String, value: AnyValue) {
        let resource = self.resource_mut();
        if let Some(kv) = resource.attributes.iter_mut().find(|kv| kv.key == key) {
            kv.value = Some(value);
        } else {
            resource.attributes.push(KeyValue {
                key,
                value: Some(value),
            });
        }
    }

    /// Set the observed_time_unix_nano (ingest timestamp) from a chrono DateTime.
    pub fn set_observed_timestamp(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.record.observed_time_unix_nano =
            now.timestamp_nanos_opt().unwrap_or(0) as u64;
    }

    /// Set source metadata: source_type and observed_time_unix_nano.
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
    // LogEvent-compatible bridge methods
    //
    // These delegate to `to_log_event()` / `from_log_event()` to provide
    // backward-compatible access for code that was written against LogEvent.
    // -----------------------------------------------------------------------

    /// Get a field value by path (LogEvent-compatible bridge).
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        self.to_log_event().get(path).cloned()
    }

    /// Insert a field value by path (LogEvent-compatible bridge).
    /// Operates by round-tripping through LogEvent.
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        let mut log = self.to_log_event();
        let old = log.insert(path, value);
        *self = Self::from_log_event(log);
        old
    }

    /// Remove a field value by path (LogEvent-compatible bridge).
    pub fn remove<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
    ) -> Option<Value> {
        let mut log = self.to_log_event();
        let old = log.remove(path);
        *self = Self::from_log_event(log);
        old
    }

    /// Get the timestamp from the event (LogEvent-compatible bridge).
    ///
    /// In Vector namespace, delegates to the schema-meaning-aware
    /// `LogEvent::get_timestamp` so that semantic meanings are respected.
    /// String values are parsed as RFC 3339 timestamps (since OTLP
    /// `AnyValue` has no native timestamp type).
    /// In Legacy namespace, prefers `time_unix_nano` (event time), falls
    /// back to `observed_time_unix_nano` (ingest time).
    pub fn get_timestamp(&self) -> Option<Value> {
        if let Some(overflow) = self.attribute("vector.timestamp_overflow") {
            if let Some(OtelValueKind::StringValue(s)) = &overflow.value {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                    return Some(Value::Timestamp(dt.with_timezone(&chrono::Utc)));
                }
            }
        }
        if self.namespace() == crate::config::LogNamespace::Vector {
            let log = self.to_log_event();
            return log.get_timestamp().map(|v| coerce_to_timestamp(v.clone()));
        }
        if self.record.time_unix_nano != 0 {
            let nanos = self.record.time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nsecs).map(Value::Timestamp)
        } else if let Some(av) = self.attribute("timestamp") {
            let v = any_value_to_vrl(av);
            Some(coerce_to_timestamp(v))
        } else if self.record.observed_time_unix_nano != 0 {
            let nanos = self.record.observed_time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nsecs).map(Value::Timestamp)
        } else {
            self.to_log_event().get_timestamp().cloned()
        }
    }

    /// Remove the timestamp from the event (LogEvent-compatible bridge).
    pub fn remove_timestamp(&mut self) -> Option<Value> {
        let ts = self.get_timestamp();
        self.record.time_unix_nano = 0;
        ts
    }

    /// Check if a field exists (LogEvent-compatible bridge).
    pub fn contains<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> bool {
        self.get(path).is_some()
    }

    /// Get the "message" field value (LogEvent-compatible bridge).
    pub fn get_message(&self) -> Option<Value> {
        self.body().map(any_value_to_vrl)
    }

    /// Get the "source_type" from resource attributes or legacy log event bridge.
    pub fn get_source_type(&self) -> Option<Value> {
        self.resource_attribute("source_type")
            .map(|av| any_value_to_vrl(&av))
            .or_else(|| self.to_log_event().get_source_type().cloned())
    }

    /// Get the host value from the event (LogEvent-compatible bridge).
    pub fn get_host(&self) -> Option<Value> {
        self.resource_attribute("host.name")
            .map(any_value_to_vrl)
            .or_else(|| self.to_log_event().get_host().cloned())
    }

    /// Parse a path and get a value (LogEvent-compatible bridge).
    pub fn parse_path_and_get_value(
        &self,
        path: &str,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let log = self.to_log_event();
        log.parse_path_and_get_value(path)
            .map(|opt| opt.cloned())
    }

    /// Get the LogNamespace (LogEvent-compatible bridge).
    pub fn namespace(&self) -> crate::config::LogNamespace {
        self.to_log_event().namespace()
    }

    /// Convert to fields (LogEvent-compatible bridge).
    /// Returns owned values since the underlying LogEvent is ephemeral.
    pub fn convert_to_fields(&self) -> Vec<(vrl::value::KeyString, Value)> {
        let log = self.to_log_event();
        log.convert_to_fields()
            .map(|(k, v)| (k, v.clone()))
            .collect()
    }

    /// Rename a key (LogEvent-compatible bridge).
    pub fn rename_key<'a>(
        &mut self,
        from: impl lookup::lookup_v2::TargetPath<'a>,
        to: impl lookup::lookup_v2::TargetPath<'a>,
    ) {
        let mut log = self.to_log_event();
        log.rename_key(from, to);
        *self = Self::from_log_event(log);
    }

    /// Get the timestamp path (LogEvent-compatible bridge).
    pub fn timestamp_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        self.to_log_event().timestamp_path().cloned()
    }

    /// Get the host path (LogEvent-compatible bridge).
    pub fn host_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        self.to_log_event().host_path().cloned()
    }

    /// Get the message path (LogEvent-compatible bridge).
    pub fn message_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        self.to_log_event().message_path().cloned()
    }

    /// Get the source type path (LogEvent-compatible bridge).
    pub fn source_type_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        self.to_log_event().source_type_path().cloned()
    }

    /// Try insert - only inserts if the path doesn't exist (LogEvent-compatible bridge).
    pub fn try_insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) {
        let mut log = self.to_log_event();
        log.try_insert(path, value);
        *self = Self::from_log_event(log);
    }

    /// Get the underlying value (LogEvent-compatible bridge).
    ///
    /// In Vector namespace, returns only the body (the actual event payload).
    /// In Legacy namespace, returns the full reconstructed object with all fields.
    pub fn value(&self) -> Value {
        if self.namespace() == crate::config::LogNamespace::Vector {
            self.body()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null)
        } else {
            self.to_log_event().value().clone()
        }
    }

    /// Get all keys (LogEvent-compatible bridge).
    pub fn keys(&self) -> Option<std::vec::IntoIter<vrl::value::KeyString>> {
        let log = self.to_log_event();
        log.keys().map(|iter| iter.collect::<Vec<_>>().into_iter())
    }

    /// Check if the log is an empty object (LogEvent-compatible bridge).
    pub fn is_empty_object(&self) -> bool {
        self.to_log_event().is_empty_object()
    }

    /// Convert to fields unquoted (LogEvent-compatible bridge).
    pub fn convert_to_fields_unquoted(&self) -> Vec<(vrl::value::KeyString, Value)> {
        let log = self.to_log_event();
        log.convert_to_fields_unquoted()
            .map(|(k, v)| (k, v.clone()))
            .collect()
    }

    /// Get a mutable reference to the underlying value.
    /// Since OtelLog doesn't have a single VRL Value, this creates a LogEvent,
    /// returns a clone of its value, and any mutations won't persist.
    /// For mutations, use insert/remove instead.
    pub fn value_mut(&mut self) -> Value {
        self.to_log_event().value().clone()
    }

    /// Get as an object map (LogEvent-compatible bridge).
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_log_event().value() {
            Value::Object(map) => Some(map.clone()),
            _ => None,
        }
    }

    /// Lossy projection of this OTel log event into a legacy `LogEvent`.
    ///
    /// The body becomes `message`, attributes become top-level fields, and
    /// resource / scope are preserved as nested objects.  Useful for
    /// text-oriented serializers (text, logfmt, CSV, GELF, CEF, syslog, etc.)
    /// that only understand `LogEvent`.
    pub fn to_log_event(&self) -> LogEvent {
        let mut map = ObjectMap::new();

        if let Some(body) = self.body() {
            match &body.value {
                Some(OtelValueKind::KvlistValue(kvl)) => {
                    for kv in &kvl.values {
                        let v = kv
                            .value
                            .as_ref()
                            .map(any_value_to_vrl)
                            .unwrap_or(Value::Null);
                        map.insert(kv.key.clone().into(), v);
                    }
                }
                _ => {
                    map.insert("message".into(), any_value_to_vrl(body));
                }
            }
        }

        for kv in &self.record.attributes {
            let mut v = kv
                .value
                .as_ref()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null);
            if kv.key == "timestamp" {
                v = coerce_to_timestamp(v);
            }
            map.insert(kv.key.clone().into(), v);
        }

        if !self.record.severity_text.is_empty() {
            map.insert(
                "severity_text".into(),
                Value::Bytes(self.record.severity_text.clone().into()),
            );
        }
        if self.record.severity_number != 0 {
            map.insert(
                "severity_number".into(),
                Value::Integer(self.record.severity_number as i64),
            );
        }
        {
            if let Some(overflow) = attribute_value(&self.record.attributes, "vector.timestamp_overflow") {
                if let Some(OtelValueKind::StringValue(s)) = &overflow.value {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                        map.insert("timestamp".into(), Value::Timestamp(dt.with_timezone(&chrono::Utc)));
                    }
                }
            } else if self.record.time_unix_nano != 0 {
                let nanos = self.record.time_unix_nano;
                let secs = (nanos / 1_000_000_000) as i64;
                let nsecs = (nanos % 1_000_000_000) as u32;
                if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                    map.insert("timestamp".into(), Value::Timestamp(ts));
                }
            } else if self.record.observed_time_unix_nano != 0 {
                if map.contains_key("timestamp") {
                    // A source explicitly provided a timestamp attribute (e.g. epoch);
                    // don't overwrite it with observed_time.
                } else {
                    let nanos = self.record.observed_time_unix_nano;
                    let secs = (nanos / 1_000_000_000) as i64;
                    let nsecs = (nanos % 1_000_000_000) as u32;
                    if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                        map.insert("timestamp".into(), Value::Timestamp(ts));
                    }
                }
            }
        }
        if !self.record.trace_id.is_empty() {
            map.insert("trace_id".into(), hex_encode(&self.record.trace_id));
        }
        if !self.record.span_id.is_empty() {
            map.insert("span_id".into(), hex_encode(&self.record.span_id));
        }

        hoist_resource_fields(&self.resource, &mut map);
        hoist_scope_fields(&self.scope, &mut map);

        LogEvent::from_map(map, self.metadata.clone())
}
}

// -- OtelSpan --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelSpan {
    pub(crate) span: Span,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelSpan {
    pub fn new(span: Span) -> Self {
        Self {
            span,
            resource: None,
            scope: None,
            metadata: EventMetadata::default(),
        }
    }

    /// Convert a legacy `TraceEvent` into an `OtelSpan`.
    ///
    /// The `TraceEvent`'s fields are stored as span attributes.
    pub fn from_trace_event(trace: super::TraceEvent) -> Self {
        let (map, metadata) = trace.into_parts();
        let attributes: Vec<KeyValue> = map
            .into_iter()
            .map(|(k, v)| KeyValue {
                key: k.to_string(),
                value: Some(vrl_value_to_any_value(&v)),
            })
            .collect();
        Self {
            span: Span {
                attributes,
                ..Default::default()
            },
            resource: None,
            scope: None,
            metadata,
        }
    }

    pub fn from_parts(
        span: Span,
        resource: Option<Resource>,
        scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        Self {
            span,
            resource,
            scope,
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
        (self.span, self.resource, self.scope, self.metadata)
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

    pub fn set_resource(&mut self, resource: Resource) {
        self.resource = Some(resource);
    }

    pub fn resource_mut(&mut self) -> &mut Resource {
        self.resource.get_or_insert_with(|| Resource {
            attributes: Vec::new(),
            dropped_attributes_count: 0,
        })
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, scope: InstrumentationScope) {
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
        attribute_value(&self.span.attributes, key)
    }

    pub fn set_attribute(&mut self, key: String, value: AnyValue) {
        set_attribute(&mut self.span.attributes, key, value);
    }

    pub fn remove_attribute(&mut self, key: &str) -> Option<AnyValue> {
        remove_attribute(&mut self.span.attributes, key)
    }

    pub fn attributes(&self) -> &[KeyValue] {
        &self.span.attributes
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, key))
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
    // LogEvent/TraceEvent-compatible bridge methods
    // -----------------------------------------------------------------------

    /// Get a field value by path (TraceEvent-compatible bridge).
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        self.to_log_event().get(path).cloned()
    }

    /// Insert a field value by path (TraceEvent-compatible bridge).
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        let mut log = self.to_log_event();
        let old = log.insert(path, value);
        let metadata = std::mem::take(&mut self.metadata);
        *self = Self::from_trace_event(super::TraceEvent::from(log));
        self.metadata = metadata;
        old
    }

    /// Check if a field exists (TraceEvent-compatible bridge).
    pub fn contains<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> bool {
        self.get(path).is_some()
    }

    /// Get as an object map (TraceEvent-compatible bridge).
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_log_event().value() {
            Value::Object(map) => Some(map.clone()),
            _ => None,
        }
    }

    /// Parse a path and get a value (TraceEvent-compatible bridge).
    pub fn parse_path_and_get_value(
        &self,
        path: &str,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let log = self.to_log_event();
        log.parse_path_and_get_value(path)
            .map(|opt| opt.cloned())
    }

    /// Lossy projection of this OTel span event into a legacy `LogEvent`.
    ///
    /// Span name becomes `message`, attributes become top-level fields, and
    /// trace_id/span_id/timestamps/status are included.  Useful for
    /// `trace_to_log` and text-oriented serializers.
    pub fn to_log_event(&self) -> LogEvent {
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
            map.insert(
                "parent_span_id".into(),
                hex_encode(&self.span.parent_span_id),
            );
        }
        if self.span.start_time_unix_nano != 0 {
            let nanos = self.span.start_time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                map.insert("start_time".into(), Value::Timestamp(ts));
            }
        }
        if self.span.end_time_unix_nano != 0 {
            let nanos = self.span.end_time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                map.insert("end_time".into(), Value::Timestamp(ts));
            }
        }
        if self.span.kind != 0 {
            map.insert("kind".into(), Value::Integer(self.span.kind as i64));
        }
        if let Some(status) = &self.span.status {
            let mut status_map = ObjectMap::new();
            if !status.message.is_empty() {
                status_map.insert(
                    "message".into(),
                    Value::Bytes(status.message.clone().into()),
                );
            }
            status_map.insert("code".into(), Value::Integer(status.code as i64));
            map.insert("status".into(), Value::Object(status_map));
        }

        for kv in &self.span.attributes {
            let v = kv
                .value
                .as_ref()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null);
            map.insert(kv.key.clone().into(), v);
        }

        hoist_resource_fields(&self.resource, &mut map);
        hoist_scope_fields(&self.scope, &mut map);

        LogEvent::from_map(map, self.metadata.clone())
    }

    /// Convert this OtelSpan into a legacy `TraceEvent`.
    pub fn to_trace_event(&self) -> TraceEvent {
        TraceEvent::from(self.to_log_event())
    }
}

// -- OtelMetric --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelMetric {
    pub(crate) metric: OtelMetricProto,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelMetric {
    pub fn new(metric: OtelMetricProto) -> Self {
        Self {
            metric,
            resource: None,
            scope: None,
            metadata: EventMetadata::default(),
        }
    }

    /// Convert a legacy Vector `Metric` into an `OtelMetric`.
    ///
    /// Maps MetricValue variants to OTel metric data types:
    /// - Counter → Sum (monotonic)
    /// - Gauge → Gauge
    /// - AggregatedHistogram → Histogram (explicit bounds)
    /// - AggregatedSummary → Summary
    /// - Distribution/Set → Gauge (lossy fallback)
    pub fn from_legacy_metric(m: super::Metric) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric, number_data_point::Value as NDPValue,
        };
        use super::{MetricKind, MetricValue};

        let (series, data, metadata) = m.into_parts();
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

        let otel_metric = OtelMetricProto {
            name,
            description: String::new(),
            unit: String::new(),
            metadata: vec![],
            data: Some(data),
        };

        let resource = if resource_attrs.is_empty() {
            None
        } else {
            Some(Resource {
                attributes: resource_attrs,
                dropped_attributes_count: 0,
            })
        };

        Self {
            metric: otel_metric,
            resource,
            scope: None,
            metadata,
        }
    }

    pub fn from_parts(
        metric: OtelMetricProto,
        resource: Option<Resource>,
        scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        Self {
            metric,
            resource,
            scope,
            metadata,
        }
    }

    pub fn into_parts(
        self,
    ) -> (
        OtelMetricProto,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        (self.metric, self.resource, self.scope, self.metadata)
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

    pub fn set_resource(&mut self, resource: Resource) {
        self.resource = Some(resource);
    }

    pub fn resource_mut(&mut self) -> &mut Resource {
        self.resource.get_or_insert_with(|| Resource {
            attributes: Vec::new(),
            dropped_attributes_count: 0,
        })
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, scope: InstrumentationScope) {
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

    pub fn set_data_point_attribute(&mut self, key: String, value: AnyValue) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let attr = KeyValue { key, value: Some(value) };
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => {
                    for dp in &mut s.data_points { dp.attributes.push(attr.clone()); }
                }
                MetricData::Gauge(g) => {
                    for dp in &mut g.data_points { dp.attributes.push(attr.clone()); }
                }
                MetricData::Histogram(h) => {
                    for dp in &mut h.data_points { dp.attributes.push(attr.clone()); }
                }
                MetricData::Summary(s) => {
                    for dp in &mut s.data_points { dp.attributes.push(attr.clone()); }
                }
                MetricData::ExponentialHistogram(e) => {
                    for dp in &mut e.data_points { dp.attributes.push(attr.clone()); }
                }
            }
        }
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, key))
    }

    // -----------------------------------------------------------------------
    // Metric-compatible bridge methods
    // -----------------------------------------------------------------------

    /// Get the metric timestamp (Metric-compatible bridge).
    pub fn timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.to_legacy_metric_ref_timestamp()
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

    /// Get the metric tags (Metric-compatible bridge).
    pub fn tags(&self) -> Option<&super::metric::MetricTags> {
        None
    }

    /// Get the metric namespace (Metric-compatible bridge).
    pub fn namespace(&self) -> Option<&str> {
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, "metric.namespace"))
            .and_then(|av| match &av.value {
                Some(OtelValueKind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
    }

    /// Get the metric value (Metric-compatible bridge).
    pub fn value(&self) -> super::MetricValue {
        self.clone().to_legacy_metric().value().clone()
    }

    /// Get the metric kind (Metric-compatible bridge).
    pub fn kind(&self) -> super::MetricKind {
        self.clone().to_legacy_metric().kind()
    }

    /// Get a tag value (Metric-compatible bridge).
    pub fn tag_value(&self, key: &str) -> Option<String> {
        self.clone().to_legacy_metric().tag_value(key)
    }

    /// Convert this `OtelMetric` back to a legacy `Metric`.
    ///
    /// Temporary bridge for sinks/transforms that still expect `Event::Metric`.
    pub fn to_legacy_metric(self) -> super::Metric {
        use chrono::{DateTime, TimeZone, Utc};
        use opentelemetry_proto::tonic::metrics::v1::{
            metric, number_data_point::Value as NDPValue, AggregationTemporality,
        };
        use super::{MetricKind, MetricValue, MetricTags};
        use super::metric::{Bucket, Quantile};

        let nanos_to_ts = |nanos: u64| -> Option<DateTime<Utc>> {
            if nanos == 0 { None } else { Some(Utc.timestamp_nanos(nanos as i64)) }
        };

        let metric_name = self.metric.name.clone();
        let mut namespace: Option<String> = None;

        let mut tags = MetricTags::default();
        if let Some(ref res) = self.resource {
            if let Some(ns_val) = attribute_value(&res.attributes, "metric.namespace") {
                if let Some(OtelValueKind::StringValue(ns)) = ns_val.value.as_ref() {
                    namespace = Some(ns.clone());
                }
            }
            for attr in &res.attributes {
                if attr.key == "metric.namespace" {
                    continue;
                }
                if let Some(ref val) = attr.value {
                    if let Some(ref v) = val.value {
                        tags.insert(
                            format!("resource.{}", attr.key),
                            otel_value_to_tag_string(v),
                        );
                    }
                }
            }
        }
        if let Some(ref scope) = self.scope {
            if !scope.name.is_empty() {
                tags.insert("scope.name".to_string(), scope.name.clone());
            }
            if !scope.version.is_empty() {
                tags.insert("scope.version".to_string(), scope.version.clone());
            }
        }

        let (kind, value, timestamp, dp_tags) = match self.metric.data.as_ref() {
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
                (kind, metric_value, ts, dp.map(|p| p.attributes.clone()))
            }
            Some(metric::Data::Gauge(gauge)) => {
                let dp = gauge.data_points.first();
                let is_set = dp
                    .map(|p| {
                        p.attributes.iter().any(|a| {
                            a.key == "vector.metric_type"
                                && a.value.as_ref().and_then(|v| v.value.as_ref())
                                    == Some(&OtelValueKind::StringValue("set".into()))
                        })
                    })
                    .unwrap_or(false);
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                if is_set {
                    let values = dp
                        .and_then(|p| attribute_value(&p.attributes, "vector.set_values"))
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
                    let kind = dp
                        .and_then(|p| attribute_value(&p.attributes, "vector.metric_kind"))
                        .and_then(|av| match &av.value {
                            Some(OtelValueKind::StringValue(s)) if s == "incremental" => {
                                Some(MetricKind::Incremental)
                            }
                            _ => Some(MetricKind::Absolute),
                        })
                        .unwrap_or(MetricKind::Absolute);
                    let mut dp_attrs = dp.map(|p| p.attributes.clone());
                    if let Some(ref mut attrs) = dp_attrs {
                        attrs.retain(|a| {
                            !a.key.starts_with("vector.")
                        });
                    }
                    (kind, MetricValue::Set { values }, ts, dp_attrs)
                } else {
                    let val = dp.and_then(|p| p.value.as_ref()).map(|v| match v {
                        NDPValue::AsDouble(f) => *f,
                        NDPValue::AsInt(i) => *i as f64,
                    }).unwrap_or(0.0);
                    let kind = dp
                        .and_then(|p| attribute_value(&p.attributes, "vector.metric_kind"))
                        .and_then(|av| match &av.value {
                            Some(OtelValueKind::StringValue(s)) if s == "incremental" => {
                                Some(MetricKind::Incremental)
                            }
                            _ => None,
                        })
                        .unwrap_or(MetricKind::Absolute);
                    let mut dp_attrs = dp.map(|p| p.attributes.clone());
                    if let Some(ref mut attrs) = dp_attrs {
                        attrs.retain(|a| !a.key.starts_with("vector."));
                    }
                    (kind, MetricValue::Gauge { value: val }, ts, dp_attrs)
                }
            }
            Some(metric::Data::Histogram(hist)) => {
                let dp = hist.data_points.first();
                let kind = if hist.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    MetricKind::Incremental
                } else {
                    MetricKind::Absolute
                };
                let is_distribution = dp
                    .map(|p| {
                        p.attributes.iter().any(|a| {
                            a.key == "vector.metric_type"
                                && a.value.as_ref().and_then(|v| v.value.as_ref())
                                    == Some(&OtelValueKind::StringValue("distribution".into()))
                        })
                    })
                    .unwrap_or(false);
                let ts = dp.and_then(|p| nanos_to_ts(p.time_unix_nano));
                if is_distribution {
                    let statistic = dp
                        .and_then(|p| {
                            attribute_value(&p.attributes, "vector.statistic")
                                .and_then(|av| match av.value.as_ref() {
                                    Some(OtelValueKind::StringValue(s)) if s == "summary" => {
                                        Some(super::StatisticKind::Summary)
                                    }
                                    _ => Some(super::StatisticKind::Histogram),
                                })
                        })
                        .unwrap_or(super::StatisticKind::Histogram);
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
                    let mut dp_attrs = dp.map(|p| p.attributes.clone());
                    if let Some(ref mut attrs) = dp_attrs {
                        attrs.retain(|a| {
                            a.key != "vector.metric_type" && a.key != "vector.statistic"
                        });
                    }
                    (kind, MetricValue::Distribution { samples, statistic }, ts, dp_attrs)
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
                    (kind, MetricValue::AggregatedHistogram { buckets, count, sum: sum_val }, ts, dp.map(|p| p.attributes.clone()))
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
                (MetricKind::Absolute, MetricValue::AggregatedSummary { quantiles, count, sum: sum_val }, ts, dp.map(|p| p.attributes.clone()))
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
                (kind, MetricValue::AggregatedHistogram { buckets, count, sum: sum_val }, ts, dp.map(|p| p.attributes.clone()))
            }
            None => (MetricKind::Absolute, MetricValue::Gauge { value: 0.0 }, None, None),
        };

        if let Some(dp_attrs) = dp_tags {
            for attr in dp_attrs {
                if let Some(ref val) = attr.value {
                    insert_otel_attr_as_tag_from_any_value(&mut tags, &attr.key, val);
                } else {
                    tags.replace(attr.key.clone(), super::metric::TagValue::Bare);
                }
            }
        }

        let interval_ms = self.reconstruct_interval_ms();

        let has_tags = !tags.is_empty();
        super::Metric::new_with_metadata(metric_name, kind, value, self.metadata)
            .with_namespace(namespace)
            .with_tags(has_tags.then_some(tags))
            .with_timestamp(timestamp)
            .with_interval_ms(interval_ms)
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
                // Approximate: proto encoded_len * 2 (JSON overhead for field names + quoting)
                JsonSize::new(self.$proto_field.encoded_len() * 2)
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
            self.attribute("service.name")
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

// Compare OtelLog via `to_log_event()` equivalence so that two events
// carrying the same logical data but stored differently in proto
// (e.g., source_type in resource vs record.attributes) compare equal.
impl EventDataEq for OtelLog {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.to_log_event().event_data_eq(&other.to_log_event())
    }
}

impl EventDataEq for OtelSpan {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.to_log_event().event_data_eq(&other.to_log_event())
    }
}

impl EventDataEq for OtelMetric {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.metric == other.metric
            && self.resource == other.resource
            && self.scope == other.scope
    }
}

impl Serialize for OtelLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_log_event().serialize(serializer)
    }
}

impl Serialize for OtelSpan {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_log_event().serialize(serializer)
    }
}

impl Serialize for OtelMetric {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.clone().to_legacy_metric().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OtelLog {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        // Full deserialization deferred to Step 5b+; placeholder for Derive on Event
        Err(serde::de::Error::custom(
            "OtelLog deserialization not yet implemented",
        ))
    }
}

impl<'de> Deserialize<'de> for OtelSpan {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "OtelSpan deserialization not yet implemented",
        ))
    }
}

impl<'de> Deserialize<'de> for OtelMetric {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "OtelMetric deserialization not yet implemented",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn to_log_event_projects_fields() {
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

        let log = event.to_log_event();
        assert_eq!(
            log.get("message").unwrap().as_str().unwrap(),
            "hello world"
        );
        assert_eq!(
            log.get("severity_text").unwrap().as_str().unwrap(),
            "ERROR"
        );
        assert_eq!(log.get("severity_number").unwrap().as_integer().unwrap(), 17);
        assert!(log.get("timestamp").unwrap().is_timestamp());
        assert_eq!(log.get("trace_id").unwrap().as_str().unwrap(), "abcd");
        assert_eq!(log.get("span_id").unwrap().as_str().unwrap(), "1234");
        assert_eq!(log.get("env").unwrap().as_str().unwrap(), "prod");

        let resource = log.get("resource").unwrap();
        assert!(resource.is_object());

        let scope = log.get("scope").unwrap();
        assert!(scope.is_object());
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
    fn otel_log_serializes_as_structured_json() {
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
        assert!(v.get("record").is_some(), "expected 'record' key, got: {json}");
        assert_eq!(v["record"]["severityText"], "INFO");
        assert!(v["resource"].is_object(), "expected 'resource' object");
    }

    #[test]
    fn metric_to_otel_metric_round_trip_counter() {
        use crate::event::{Metric, MetricKind, MetricValue};
        use chrono::Utc;

        let m = Metric::new("requests_total", MetricKind::Incremental, MetricValue::Counter { value: 42.0 })
            .with_namespace(Some("http"))
            .with_timestamp(Some(Utc::now()));

        let otel = OtelMetric::from_legacy_metric(m.clone());
        assert_eq!(otel.name(), "requests_total");

        let back = otel.to_legacy_metric();
        assert_eq!(back.name(), "requests_total");
        assert_eq!(back.namespace(), Some("http"));
        assert_eq!(back.kind(), MetricKind::Incremental);
        match back.value() {
            MetricValue::Counter { value } => assert!((value - 42.0).abs() < f64::EPSILON),
            other => panic!("expected Counter, got {other:?}"),
        }
    }

    #[test]
    fn metric_to_otel_metric_round_trip_gauge() {
        use crate::event::{Metric, MetricKind, MetricValue};

        let m = Metric::new("temperature", MetricKind::Absolute, MetricValue::Gauge { value: 98.6 });
        let otel = OtelMetric::from_legacy_metric(m);
        let back = otel.to_legacy_metric();

        assert_eq!(back.name(), "temperature");
        assert_eq!(back.kind(), MetricKind::Absolute);
        match back.value() {
            MetricValue::Gauge { value } => assert!((value - 98.6).abs() < f64::EPSILON),
            other => panic!("expected Gauge, got {other:?}"),
        }
    }

    #[test]
    fn metric_to_otel_metric_round_trip_histogram() {
        use crate::event::{Metric, MetricKind, MetricValue};
        use crate::event::metric::Bucket;

        let buckets = vec![
            Bucket { count: 10, upper_limit: 5.0 },
            Bucket { count: 20, upper_limit: 10.0 },
            Bucket { count: 5, upper_limit: f64::INFINITY },
        ];
        let m = Metric::new(
            "latency",
            MetricKind::Absolute,
            MetricValue::AggregatedHistogram { buckets, count: 35, sum: 150.0 },
        );
        let otel = OtelMetric::from_legacy_metric(m);
        let back = otel.to_legacy_metric();

        assert_eq!(back.name(), "latency");
        match back.value() {
            MetricValue::AggregatedHistogram { buckets, count, sum } => {
                assert_eq!(*count, 35);
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
        use crate::event::{Event, Metric, MetricKind, MetricValue};

        let m = Metric::new("test", MetricKind::Absolute, MetricValue::Gauge { value: 1.0 });
        let event: Event = m.into();
        assert!(matches!(event, Event::Metric(_)), "expected Event::Metric, got {event:?}");

        let metric = event.try_into_metric().expect("should convert back");
        assert_eq!(metric.name(), "test");
    }
}
