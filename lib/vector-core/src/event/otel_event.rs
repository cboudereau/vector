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

impl tracing::field::Visit for OtelLogTracingBuilder {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.map
            .insert(field.name().into(), Value::Bytes(value.to_string().into()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.map
            .insert(field.name().into(), Value::Bytes(format!("{value:?}").into()));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.map.insert(field.name().into(), Value::Integer(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match i64::try_from(value) {
            Ok(v) => self.map.insert(field.name().into(), Value::Integer(v)),
            Err(_) => self
                .map
                .insert(field.name().into(), Value::Bytes(value.to_string().into())),
        };
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.map.insert(field.name().into(), Value::Boolean(value));
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

    /// Build an `OtelLog` from a `tracing::Event` — the same semantics as
    /// Converts a `tracing::Event` directly into an `OtelLog`.
    /// Accumulates fields via the `tracing::field::Visit`
    /// trait into a single `ObjectMap`, then converts to `OtelLog` once
    /// to amortize the legacy-layout round-trip.
    pub fn from_tracing_event(event: &tracing::Event<'_>) -> Self {
        let mut builder = OtelLogTracingBuilder::default();
        event.record(&mut builder);

        let meta = event.metadata();
        builder.map.insert(
            "timestamp".into(),
            Value::Timestamp(chrono::Utc::now()),
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
    // Field access methods
    //
    // These use `to_value_legacy_layout()` to build a flat Value tree from
    // proto fields. Target state: replace with direct proto accessors once
    // all external callers (codecs, sinks, transforms) are migrated.
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
    /// Builds a Value tree matching the legacy layout, without intermediate conversion.
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        match path.prefix() {
            lookup::PathPrefix::Event => {
                let value = self.to_value_legacy_layout();
                value.get(path.value_path()).cloned()
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value().get(path.value_path()).cloned()
            }
        }
    }

    /// Insert a field value by path.
    /// Builds a Value, inserts, writes back — without intermediate conversion.
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        match path.prefix() {
            lookup::PathPrefix::Event => {
                let mut val = self.to_value_legacy_layout();
                let old = val.insert(path.value_path(), value);
                self.apply_value_legacy_layout(val);
                old
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().insert(path.value_path(), value)
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
        match path.prefix() {
            lookup::PathPrefix::Event => {
                let mut val = self.to_value_legacy_layout();
                let old = val.remove(path.value_path(), prune);
                self.apply_value_legacy_layout(val);
                old
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().remove(path.value_path(), prune)
            }
        }
    }

    /// Amortize the `to_value_legacy_layout` → `apply_value_legacy_layout`
    /// round-trip across multiple mutations. Each call to `insert` /
    /// `remove` / `maybe_insert` on `OtelLog` does a full round-trip via
    /// the legacy layout (O(event size)). Hot paths that mutate many
    /// fields per event (e.g. kubernetes_logs annotators, syslog
    /// decoder's header injection) can call this instead:
    ///
    /// ```ignore
    /// log.modify_as_value(|v| {
    ///     if let Some(m) = v.as_object_mut() {
    ///         m.insert("key1".into(), val1);
    ///         m.insert("key2".into(), val2);
    ///         m.insert("key3".into(), val3);
    ///     }
    /// });
    /// ```
    ///
    /// All mutations happen on a single owned `Value` tree, with a
    /// single round-trip on entry and exit. See `LEGACY_REMOVAL_PLAN.md`
    /// — "Performance findings" for details.
    pub fn modify_as_value<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Value) -> R,
    {
        let mut value = self.to_value_legacy_layout();
        let result = f(&mut value);
        self.apply_value_legacy_layout(value);
        result
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
    pub fn to_value_legacy_layout(&self) -> Value {
        let mut map = ObjectMap::new();

        // Body: KvList → expand to top-level; other → "body" field
        if let Some(body) = self.body() {
            match &body.value {
                Some(OtelValueKind::KvlistValue(kvl)) => {
                    for kv in &kvl.values {
                        let v = kv.value.as_ref().map(any_value_to_vrl).unwrap_or(Value::Null);
                        map.insert(kv.key.clone().into(), v);
                    }
                }
                _ => {
                    map.insert("body".into(), any_value_to_vrl(body));
                }
            }
        }

        // Attributes → top-level (timestamp coerced)
        for kv in &self.record.attributes {
            let mut v = kv.value.as_ref().map(any_value_to_vrl).unwrap_or(Value::Null);
            if kv.key == "timestamp" {
                v = coerce_to_timestamp(v);
            }
            map.insert(kv.key.clone().into(), v);
        }

        // Severity
        if !self.record.severity_text.is_empty() {
            map.insert("severity_text".into(), Value::Bytes(self.record.severity_text.clone().into()));
        }
        if self.record.severity_number != 0 {
            map.insert("severity_number".into(), Value::Integer(self.record.severity_number as i64));
        }

        // Timestamp
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
            if !map.contains_key("timestamp") {
                let nanos = self.record.observed_time_unix_nano;
                let secs = (nanos / 1_000_000_000) as i64;
                let nsecs = (nanos % 1_000_000_000) as u32;
                if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                    map.insert("timestamp".into(), Value::Timestamp(ts));
                }
            }
        }

        // Trace/span IDs
        if !self.record.trace_id.is_empty() {
            map.insert("trace_id".into(), hex_encode(&self.record.trace_id));
        }
        if !self.record.span_id.is_empty() {
            map.insert("span_id".into(), hex_encode(&self.record.span_id));
        }

        // Resource/scope hoisting
        hoist_resource_fields(&self.resource, &mut map);
        hoist_scope_fields(&self.scope, &mut map);

        Value::Object(map)
    }

    /// Construct an OtelLog from a legacy-layout Value + metadata.
    /// Routes fields into OTel structure: body, timestamp, source_type/host
    /// → resource attrs, everything else → record.attributes. Clears scope.
    pub fn from_value_map(value: Value, metadata: EventMetadata) -> Self {
        let mut out = Self {
            record: LogRecord::default(),
            resource: None,
            scope: None,
            metadata,
        };
        out.apply_value_legacy_layout(value);
        out
    }

    /// Write back a Value tree (legacy layout) to proto fields.
    ///
    /// Symmetric with `to_value_legacy_layout`: well-known proto fields
    /// (`body`/`message`, `timestamp`, `severity_text`, `severity_number`,
    /// `trace_id`, `span_id`) are extracted into their native `LogRecord`
    /// slots; `source_type`/`host` go to resource attributes; the remainder
    /// becomes `record.attributes`. Malformed hex for `trace_id`/`span_id`
    /// falls back to storing the raw Value as an attribute so corrupt data
    /// is not silently dropped.
    fn apply_value_legacy_layout(&mut self, value: Value) {
        let mut map = match value {
            Value::Object(m) => m,
            other => {
                // Non-object: store as body, clear everything else
                self.record = LogRecord {
                    body: Some(vrl_value_to_any_value(&other)),
                    ..Default::default()
                };
                self.resource = None;
                self.scope = None;
                return;
            }
        };

        // Body (with legacy "message" fallback)
        let body = map.remove("body")
            .or_else(|| map.remove("message"))
            .map(|v| vrl_value_to_any_value(&v));

        // Timestamp
        let ts_extract = extract_timestamp_nanos(&mut map);
        let time_unix_nano = ts_extract.nanos;

        // Severity: Value::Bytes → severity_text, Value::Integer → severity_number.
        let severity_text = match map.remove("severity_text") {
            Some(Value::Bytes(b)) => {
                String::from_utf8(b.to_vec()).unwrap_or_default()
            }
            Some(other) => {
                // Non-bytes value: keep as attribute so we don't drop data.
                map.insert("severity_text".into(), other);
                String::new()
            }
            None => String::new(),
        };
        let severity_number = match map.remove("severity_number") {
            Some(Value::Integer(i)) => i as i32,
            Some(other) => {
                map.insert("severity_number".into(), other);
                0
            }
            None => 0,
        };

        // Trace/span IDs: hex-decode back to bytes. If decoding fails
        // (corrupt hex), preserve the original Value as an attribute.
        let trace_id = match map.remove("trace_id") {
            Some(v) => match hex_decode(&v) {
                Some(bytes) => bytes,
                None => {
                    map.insert("trace_id".into(), v);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let span_id = match map.remove("span_id") {
            Some(v) => match hex_decode(&v) {
                Some(bytes) => bytes,
                None => {
                    map.insert("span_id".into(), v);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        // Route well-known log_schema fields (source_type, host) back to resource attrs
        // — matches `to_value_legacy_layout`'s resource hoisting so round-trips
        // are lossless.
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
        self.resource = if resource_attrs.is_empty() {
            None
        } else {
            Some(Resource {
                attributes: resource_attrs,
                dropped_attributes_count: 0,
            })
        };

        // Everything else → record.attributes (including "resource"/"scope"
        // sub-objects if present — they become regular attributes).
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

        self.record = LogRecord {
            body,
            time_unix_nano,
            severity_text,
            severity_number,
            trace_id,
            span_id,
            attributes,
            ..Default::default()
        };
        self.scope = None;
    }

    /// Get the timestamp from the event.
    ///
    /// In Vector namespace, delegates to the schema-meaning-aware
    /// Respects semantic meanings for timestamp resolution.
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
        // In Vector namespace, timestamp may be stored via schema meaning (e.g. @timestamp).
        // Check this FIRST before falling back to time_unix_nano (which may be ingest time).
        if self.namespace() == crate::config::LogNamespace::Vector {
            if let Some(ts) = self.get_by_meaning("timestamp") {
                return Some(coerce_to_timestamp(ts));
            }
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
            None
        }
    }

    /// Remove the timestamp from the event.
    pub fn remove_timestamp(&mut self) -> Option<Value> {
        let ts = self.get_timestamp();
        self.record.time_unix_nano = 0;
        ts
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
            .map(|av| any_value_to_vrl(&av))
    }

    /// Get the host value from resource attributes.
    pub fn get_host(&self) -> Option<Value> {
        self.resource_attribute("host.name")
            .map(any_value_to_vrl)
    }

    /// Parse a path and get a value.
    pub fn parse_path_and_get_value(
        &self,
        path: &str,
    ) -> Result<Option<Value>, vrl::path::PathParseError> {
        let target_path = vrl::path::parse_target_path(path)?;
        Ok(self.get(&target_path))
    }

    /// Get the LogNamespace from metadata.
    pub fn namespace(&self) -> crate::config::LogNamespace {
        if self.metadata.value().get(lookup::path!("vector")).is_some() {
            crate::config::LogNamespace::Vector
        } else {
            crate::config::LogNamespace::Legacy
        }
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
        match self.to_value_legacy_layout() {
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
        match self.to_value_legacy_layout() {
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

    /// Get the timestamp path.
    pub fn timestamp_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("timestamp")))
    }

    /// Get the host path.
    pub fn host_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("host")))
    }

    /// Get the body path.
    pub fn body_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("body")))
    }

    /// Deprecated alias for body_path.
    pub fn message_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        self.body_path()
    }

    /// Get the source type path.
    pub fn source_type_path(&self) -> Option<vrl::path::OwnedTargetPath> {
        use vrl::path::OwnedTargetPath;
        use lookup::owned_value_path;
        Some(OwnedTargetPath::event(owned_value_path!("source_type")))
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
    /// In Legacy namespace: returns full event with all fields.
    pub fn value(&self) -> Value {
        if self.namespace() == crate::config::LogNamespace::Vector {
            self.body()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null)
        } else {
            self.to_value_legacy_layout()
        }
    }

    /// Get all top-level keys from the event.
    /// Uses to_value_legacy_layout() to guarantee the same deduplication
    /// semantics as other accessors (e.g. attribute key colliding with body key).
    pub fn keys(&self) -> Option<std::vec::IntoIter<vrl::value::KeyString>> {
        match self.to_value_legacy_layout() {
            Value::Object(map) => Some(map.into_keys().collect::<Vec<_>>().into_iter()),
            _ => None,
        }
    }

    /// Check if the log has no body and no attributes.
    pub fn is_empty_object(&self) -> bool {
        self.record.body.is_none() && self.record.attributes.is_empty()
    }

    /// Convert to fields unquoted — recursively flatten nested objects with unquoted dotted keys.
    pub fn convert_to_fields_unquoted(&self) -> Vec<(vrl::value::KeyString, Value)> {
        match self.to_value_legacy_layout() {
            Value::Object(map) => super::util::log::all_fields_unquoted(&map)
                .map(|(k, v)| (k, v.clone()))
                .collect(),
            _ => vec![],
        }
    }

    /// Get a snapshot of the value (mutations won't persist — use insert/remove).
    pub fn value_mut(&mut self) -> Value {
        self.to_value_legacy_layout()
    }

    /// Get as an object map.
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_value_legacy_layout() {
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
            resource: None,
            scope: None,
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

    /// Create an OtelSpan from an OtelLog (for trace signal detection in OTLP decoder).
    ///
    /// The OtelLog's fields become span attributes. Resource and scope are preserved.
    pub fn from_otel_log(log: OtelLog) -> Self {
        let map = log.as_map().unwrap_or_default();
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
            resource: log.resource,
            scope: log.scope,
            metadata: log.metadata,
        }
    }

    /// Construct an `OtelSpan` from a legacy-layout Value + metadata.
    ///
    /// Routes native span fields (`name`, `trace_id`, `span_id`,
    /// `parent_span_id`, `start_time`/`end_time`, `kind`, `status`) into
    /// their proto slots; everything else becomes `span.attributes`. See
    /// `apply_value_legacy_layout` for the full routing contract.
    pub fn from_value_map(value: Value, metadata: EventMetadata) -> Self {
        let mut out = Self {
            span: Span::default(),
            resource: None,
            scope: None,
            metadata,
        };
        out.apply_value_legacy_layout(value);
        out
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
    // Field access methods (same pattern as OtelLog — see comment above)
    // -----------------------------------------------------------------------

    /// Build a Value tree with the legacy layout — no intermediate conversion.
    pub fn to_value_legacy_layout(&self) -> Value {
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

        Value::Object(map)
    }

    /// Write back a Value tree (legacy layout) to proto fields.
    ///
    /// Symmetric with `to_value_legacy_layout`: native span proto fields
    /// (`name`, `trace_id`, `span_id`, `parent_span_id`, `start_time`,
    /// `end_time`, `kind`, `status`) are extracted into their `Span` slots;
    /// the remainder becomes `span.attributes`. Malformed hex IDs and
    /// non-matching shapes fall back to attribute storage so no data is
    /// silently lost.
    fn apply_value_legacy_layout(&mut self, value: Value) {
        use opentelemetry_proto::tonic::trace::v1::Status;

        let mut map = match value {
            Value::Object(m) => m,
            _ => ObjectMap::new(),
        };

        let name = match map.remove("name") {
            Some(Value::Bytes(b)) => String::from_utf8(b.to_vec()).unwrap_or_default(),
            Some(other) => {
                map.insert("name".into(), other);
                String::new()
            }
            None => String::new(),
        };

        // Hex-encoded IDs; malformed → kept as attribute.
        let take_id = |map: &mut ObjectMap, key: &str| -> Vec<u8> {
            match map.remove(key) {
                Some(v) => match hex_decode(&v) {
                    Some(bytes) => bytes,
                    None => {
                        map.insert(key.into(), v);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            }
        };
        let trace_id = take_id(&mut map, "trace_id");
        let span_id = take_id(&mut map, "span_id");
        let parent_span_id = take_id(&mut map, "parent_span_id");

        // Timestamps encoded as Value::Timestamp → nanos since epoch.
        // Span proto fields are u64: pre-epoch or out-of-range timestamps
        // cannot be represented, so we preserve the original value as an
        // attribute rather than wrapping negatives to huge future times.
        let take_time = |map: &mut ObjectMap, key: &str| -> u64 {
            match map.remove(key) {
                Some(Value::Timestamp(ts)) => match ts.timestamp_nanos_opt() {
                    Some(n) if n >= 0 => n as u64,
                    _ => {
                        map.insert(key.into(), Value::Timestamp(ts));
                        0
                    }
                },
                Some(other) => {
                    map.insert(key.into(), other);
                    0
                }
                None => 0,
            }
        };
        let start_time_unix_nano = take_time(&mut map, "start_time");
        let end_time_unix_nano = take_time(&mut map, "end_time");

        let kind = match map.remove("kind") {
            Some(Value::Integer(i)) => i as i32,
            Some(other) => {
                map.insert("kind".into(), other);
                0
            }
            None => 0,
        };

        // Status: { message: String, code: Integer }.
        let status = match map.remove("status") {
            Some(Value::Object(mut status_map)) => {
                let message = match status_map.remove("message") {
                    Some(Value::Bytes(b)) => {
                        String::from_utf8(b.to_vec()).unwrap_or_default()
                    }
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
            Some(other) => {
                map.insert("status".into(), other);
                None
            }
            None => None,
        };

        let attributes: Vec<KeyValue> = map
            .into_iter()
            .map(|(k, v)| KeyValue {
                key: k.to_string(),
                value: Some(vrl_value_to_any_value(&v)),
            })
            .collect();

        self.span = Span {
            name,
            trace_id,
            span_id,
            parent_span_id,
            start_time_unix_nano,
            end_time_unix_nano,
            kind,
            status,
            attributes,
            ..Default::default()
        };
        self.resource = None;
        self.scope = None;
    }

    /// Get a field value by path.
    pub fn get<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> Option<Value> {
        match path.prefix() {
            lookup::PathPrefix::Event => {
                let value = self.to_value_legacy_layout();
                value.get(path.value_path()).cloned()
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value().get(path.value_path()).cloned()
            }
        }
    }

    /// Insert a field value by path.
    pub fn insert<'a>(
        &mut self,
        path: impl lookup::lookup_v2::TargetPath<'a>,
        value: impl Into<Value>,
    ) -> Option<Value> {
        match path.prefix() {
            lookup::PathPrefix::Event => {
                let mut val = self.to_value_legacy_layout();
                let old = val.insert(path.value_path(), value);
                self.apply_value_legacy_layout(val);
                old
            }
            lookup::PathPrefix::Metadata => {
                self.metadata.value_mut().insert(path.value_path(), value)
            }
        }
    }

    /// Check if a field exists.
    pub fn contains<'a>(&self, path: impl lookup::lookup_v2::TargetPath<'a>) -> bool {
        self.get(path).is_some()
    }

    /// Get as an object map.
    pub fn as_map(&self) -> Option<ObjectMap> {
        match self.to_value_legacy_layout() {
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

    /// Convenience constructor for a distribution metric.
    /// OTel has no direct distribution equivalent; represented as a Gauge
    /// with value 0 and an attribute `"vector.metric_type" = "distribution"`.
    /// Use `from_legacy_metric` for lossless round-trips.
    pub fn new_distribution(name: impl Into<String>, kind: super::MetricKind) -> Self {
        let mut m = Self::new_gauge(name, 0.0);
        if kind == super::MetricKind::Incremental {
            m.set_data_point_attribute(
                "vector.metric_kind".to_string(),
                string_value("incremental"),
            );
        }
        m.set_data_point_attribute(
            "vector.metric_type".to_string(),
            string_value("distribution"),
        );
        m
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

    /// Get the attributes of the first data point (for VRL `.tags` backward compat).
    pub fn first_data_point_attributes(&self) -> &[KeyValue] {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        match self.metric.data.as_ref() {
            Some(MetricData::Sum(s)) => s.data_points.first().map(|dp| dp.attributes.as_slice()).unwrap_or(&[]),
            Some(MetricData::Gauge(g)) => g.data_points.first().map(|dp| dp.attributes.as_slice()).unwrap_or(&[]),
            Some(MetricData::Histogram(h)) => h.data_points.first().map(|dp| dp.attributes.as_slice()).unwrap_or(&[]),
            Some(MetricData::Summary(s)) => s.data_points.first().map(|dp| dp.attributes.as_slice()).unwrap_or(&[]),
            Some(MetricData::ExponentialHistogram(e)) => e.data_points.first().map(|dp| dp.attributes.as_slice()).unwrap_or(&[]),
            None => &[],
        }
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

    /// Reduce multi-value (array) data point attributes to single values.
    /// Keeps only the last non-null string value from each array attribute.
    pub fn reduce_tags_to_single(&mut self) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let reduce_attrs = |attrs: &mut Vec<KeyValue>| {
            for attr in attrs.iter_mut() {
                if let Some(AnyValue { value: Some(OtelValueKind::ArrayValue(arr)) }) = &attr.value {
                    // Find the last non-null string value
                    let last = arr.values.iter().rev().find_map(|v| match &v.value {
                        Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                        _ => None,
                    });
                    if let Some(s) = last {
                        attr.value = Some(AnyValue {
                            value: Some(OtelValueKind::StringValue(s)),
                        });
                    }
                }
            }
        };
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => { for dp in &mut s.data_points { reduce_attrs(&mut dp.attributes); } }
                MetricData::Gauge(g) => { for dp in &mut g.data_points { reduce_attrs(&mut dp.attributes); } }
                MetricData::Histogram(h) => { for dp in &mut h.data_points { reduce_attrs(&mut dp.attributes); } }
                MetricData::Summary(s) => { for dp in &mut s.data_points { reduce_attrs(&mut dp.attributes); } }
                MetricData::ExponentialHistogram(e) => { for dp in &mut e.data_points { reduce_attrs(&mut dp.attributes); } }
            }
        }
    }

    /// Remove a data point attribute by key from all data points.
    pub fn remove_data_point_attribute(&mut self, key: &str) -> Option<AnyValue> {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let mut removed = None;
        let remove_from = |attrs: &mut Vec<KeyValue>| -> Option<AnyValue> {
            if let Some(pos) = attrs.iter().position(|a| a.key == key) {
                attrs.remove(pos).value
            } else {
                None
            }
        };
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => { for dp in &mut s.data_points { removed = removed.or(remove_from(&mut dp.attributes)); } }
                MetricData::Gauge(g) => { for dp in &mut g.data_points { removed = removed.or(remove_from(&mut dp.attributes)); } }
                MetricData::Histogram(h) => { for dp in &mut h.data_points { removed = removed.or(remove_from(&mut dp.attributes)); } }
                MetricData::Summary(s) => { for dp in &mut s.data_points { removed = removed.or(remove_from(&mut dp.attributes)); } }
                MetricData::ExponentialHistogram(e) => { for dp in &mut e.data_points { removed = removed.or(remove_from(&mut dp.attributes)); } }
            }
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
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, key))
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

    /// Builder-style: set the metric namespace (stored as `metric.namespace` resource attribute).
    pub fn with_namespace(mut self, namespace: Option<impl Into<String>>) -> Self {
        if let Some(ns) = namespace {
            let ns_str = ns.into();
            let resource = self.resource.get_or_insert_with(|| Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            });
            // Remove existing metric.namespace if present
            resource.attributes.retain(|kv| kv.key != "metric.namespace");
            resource.attributes.push(KeyValue {
                key: "metric.namespace".to_string(),
                value: Some(string_value(&ns_str)),
            });
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
    /// (multi). Mirrors the tag encoding used by `from_legacy_metric`.
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

    /// Build MetricTags from proto data point, resource, and scope attributes.
    ///
    /// Returns an owned `MetricTags` (not a reference) because the tags are
    /// assembled from multiple proto fields. Returns `None` if there are no
    /// tags at all.
    pub fn tags(&self) -> Option<super::metric::MetricTags> {
        let mut tags = super::MetricTags::default();

        // Resource attributes (prefixed with "resource.")
        if let Some(ref res) = self.resource {
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
        for attr in self.first_data_point_attributes() {
            if attr.key.starts_with("vector.") {
                continue;
            }
            if let Some(ref val) = attr.value {
                insert_otel_attr_as_tag_from_any_value(&mut tags, &attr.key, val);
            } else {
                tags.replace(attr.key.clone(), super::metric::TagValue::Bare);
            }
        }

        if tags.is_empty() { None } else { Some(tags) }
    }

    /// Get the metric namespace from the `metric.namespace` resource attribute.
    pub fn namespace(&self) -> Option<&str> {
        self.resource
            .as_ref()
            .and_then(|r| attribute_value(&r.attributes, "metric.namespace"))
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

    /// Get a tag value by searching resource, scope, and data point attributes.
    pub fn tag_value(&self, key: &str) -> Option<String> {
        // Check data point attributes first
        let dp_attrs = self.first_data_point_attributes();
        if let Some(av) = attribute_value(dp_attrs, key) {
            if let Some(ref v) = av.value {
                return Some(otel_value_to_tag_string(v));
            }
        }
        // Check resource attributes (prefixed with "resource." in legacy)
        if let Some(stripped) = key.strip_prefix("resource.") {
            if let Some(ref res) = self.resource {
                if let Some(av) = attribute_value(&res.attributes, stripped) {
                    if let Some(ref v) = av.value {
                        return Some(otel_value_to_tag_string(v));
                    }
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
        Option<Vec<KeyValue>>,
    ) {
        use chrono::{TimeZone, Utc};
        use opentelemetry_proto::tonic::metrics::v1::{
            metric, number_data_point::Value as NDPValue, AggregationTemporality,
        };
        use super::{MetricKind, MetricValue};
        use super::metric::{Bucket, Quantile};

        let nanos_to_ts = |nanos: u64| -> Option<chrono::DateTime<Utc>> {
            if nanos == 0 { None } else { Some(Utc.timestamp_nanos(nanos as i64)) }
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
                        attrs.retain(|a| !a.key.starts_with("vector."));
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
                        attrs.retain(|a| a.key != "vector.metric_type" && a.key != "vector.statistic");
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
        }
    }

    /// Decompose this OtelMetric into legacy metric parts without creating
    /// an intermediate Metric. Used by aggregate and other transforms that
    /// store MetricSeries/MetricData separately.
    pub fn into_metric_parts(self) -> (super::metric::MetricSeries, super::metric::MetricData, super::EventMetadata) {
        use super::metric::{MetricData, MetricName, MetricSeries, MetricTime};

        let name = self.metric.name.clone();
        let namespace = self.namespace().map(|s| s.to_string());
        let metric_tags = self.tags();
        let (kind, value, timestamp, _dp_attrs) = self.extract_metric_data();
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

// Compare OtelLog via legacy layout equivalence so that two events
// carrying the same logical data but stored differently in proto
// (e.g., source_type in resource vs record.attributes) compare equal.
impl EventDataEq for OtelLog {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.to_value_legacy_layout() == other.to_value_legacy_layout()
    }
}

impl EventDataEq for OtelSpan {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.to_value_legacy_layout() == other.to_value_legacy_layout()
    }
}

impl EventDataEq for OtelMetric {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.metric == other.metric
            && self.resource == other.resource
            && self.scope == other.scope
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

impl Serialize for OtelLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Legacy flat layout — many sinks use field paths in the serialized JSON
        // at runtime (websocket ack message_id, Elasticsearch _id, Splunk HEC
        // timestamp extraction). Changing to OTLP JSON breaks runtime behavior,
        // not just tests. Use OtlpJsonLog wrapper for explicit opt-in.
        self.to_value_legacy_layout().serialize(serializer)
    }
}

impl Serialize for OtelSpan {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value_legacy_layout().serialize(serializer)
    }
}

impl Serialize for OtelMetric {
    /// Serialize in OTLP-native JSON format.
    ///
    /// Produces the proto3 JSON mapping of the OTel Metric proto with
    /// resource and scope at the top level. Field names use camelCase
    /// per the proto3 JSON spec.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use opentelemetry_proto::tonic::metrics::v1::metric;
        use super::otel_json::*;

        let mut len = 1; // name always present
        if !self.metric.description.is_empty() { len += 1; }
        if !self.metric.unit.is_empty() { len += 1; }
        if self.metric.data.is_some() { len += 1; }
        if self.resource.is_some() { len += 1; }
        if self.scope.is_some() { len += 1; }

        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("name", &self.metric.name)?;
        if !self.metric.description.is_empty() {
            map.serialize_entry("description", &self.metric.description)?;
        }
        if !self.metric.unit.is_empty() {
            map.serialize_entry("unit", &self.metric.unit)?;
        }

        if let Some(ref data) = self.metric.data {
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

        if let Some(ref res) = self.resource {
            map.serialize_entry("resource", &SerializableResource(res))?;
        }
        if let Some(ref scope) = self.scope {
            map.serialize_entry("scope", &SerializableScope(scope))?;
        }
        map.end()
    }
}

impl std::fmt::Display for OtelMetric {
    /// Display in Prometheus-like text format:
    /// `TIMESTAMP NAMESPACE_NAME{TAGS} KIND VALUE`
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, value, timestamp, _) = self.extract_metric_data();
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

        // Trigger the round-trip: insert → to_value_legacy_layout →
        // apply_value_legacy_layout.
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

        let value = event.to_value_legacy_layout();
        let map = value.as_object().expect("expected object");
        assert_eq!(map.get("body").unwrap().as_str().unwrap(), "hello world");
        assert_eq!(map.get("severity_text").unwrap().as_str().unwrap(), "ERROR");
        assert_eq!(map.get("severity_number").unwrap().as_integer().unwrap(), 17);
        assert!(map.get("timestamp").unwrap().is_timestamp());
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
        // Serialize produces legacy flat layout (resource as sub-object)
        assert_eq!(v["body"], "hello");
        assert_eq!(v["severity_text"], "INFO");
        assert_eq!(v["resource"]["service.name"], "test-svc");
    }

    #[test]
    fn metric_to_otel_metric_round_trip_counter() {
        use crate::event::{Metric, MetricKind, MetricValue};
        use chrono::Utc;

        let m = Metric::new("requests_total", MetricKind::Incremental, MetricValue::Counter { value: 42.0 })
            .with_namespace(Some("http"))
            .with_timestamp(Some(Utc::now()));

        let otel = OtelMetric::from_legacy_metric(m);
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
        use crate::event::{Metric, MetricKind, MetricValue};

        let m = Metric::new("temperature", MetricKind::Absolute, MetricValue::Gauge { value: 98.6 });
        let otel = OtelMetric::from_legacy_metric(m);

        assert_eq!(otel.name(), "temperature");
        assert_eq!(otel.kind(), MetricKind::Absolute);
        match otel.value() {
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
        use crate::event::{Event, Metric, MetricKind, MetricValue, OtelMetric};

        let m = Metric::new("test", MetricKind::Absolute, MetricValue::Gauge { value: 1.0 });
        let event: Event = Event::Metric(OtelMetric::from_legacy_metric(m));
        assert!(matches!(event, Event::Metric(_)), "expected Event::Metric, got {event:?}");

        let metric = event.try_into_otel_metric().expect("should convert back");
        assert_eq!(metric.name(), "test");
    }

    #[test]
    fn get_by_meaning_resolves_schema_meaning() {
        use std::sync::Arc;
        use crate::config::LogNamespace;

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
        assert_eq!(event.namespace(), LogNamespace::Vector);

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
        use crate::event::{Metric, MetricKind, MetricValue};

        let m = Metric::new(
            "test_metric",
            MetricKind::Incremental,
            MetricValue::Counter { value: 1.0 },
        )
        .with_namespace(Some("ns"))
        .with_tags(Some(
            vec![("env".to_string(), "prod".to_string())]
                .into_iter()
                .collect(),
        ));

        let otel = OtelMetric::from_legacy_metric(m);

        // Data point attribute lookup
        assert_eq!(otel.tag_value("env"), Some("prod".to_string()));

        // Resource attribute lookup (prefixed with "resource.")
        // from_legacy_metric stores namespace in resource as "metric.namespace"
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
        use crate::event::{Metric, MetricKind, MetricValue};

        let m = Metric::new(
            "test_metric",
            MetricKind::Incremental,
            MetricValue::Counter { value: 1.0 },
        )
        .with_tags(Some(
            vec![
                ("env".to_string(), "prod".to_string()),
                ("region".to_string(), "us-east".to_string()),
            ]
            .into_iter()
            .collect(),
        ));

        let otel = OtelMetric::from_legacy_metric(m);
        let tags = otel.tags().expect("should have tags");
        assert_eq!(tags.get("env"), Some("prod"));
        assert_eq!(tags.get("region"), Some("us-east"));

        // Empty metric has no tags
        let empty = OtelMetric::from_legacy_metric(Metric::new(
            "empty",
            MetricKind::Absolute,
            MetricValue::Gauge { value: 0.0 },
        ));
        assert!(empty.tags().is_none());
    }

    #[test]
    fn new_counter_matches_from_legacy_metric() {
        use crate::event::{Metric, MetricKind, MetricValue};

        let direct = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0);
        let via_legacy = OtelMetric::from_legacy_metric(Metric::new(
            "requests",
            MetricKind::Incremental,
            MetricValue::Counter { value: 42.0 },
        ));

        assert_eq!(direct.name(), via_legacy.name());
        assert_eq!(direct.kind(), via_legacy.kind());
        assert_eq!(direct.value(), via_legacy.value());
    }

    #[test]
    fn new_gauge_matches_from_legacy_metric() {
        use crate::event::{Metric, MetricKind, MetricValue};

        let direct = OtelMetric::new_gauge("temperature", 98.6);
        let via_legacy = OtelMetric::from_legacy_metric(Metric::new(
            "temperature",
            MetricKind::Absolute,
            MetricValue::Gauge { value: 98.6 },
        ));

        assert_eq!(direct.name(), via_legacy.name());
        assert_eq!(direct.kind(), via_legacy.kind());
        assert_eq!(direct.value(), via_legacy.value());
    }

    #[test]
    fn new_histogram_matches_from_legacy_metric() {
        use crate::event::{Metric, MetricKind, MetricValue};

        let buckets = crate::buckets![1.0 => 10, 5.0 => 20, 10.0 => 5];
        let direct = OtelMetric::new_histogram(
            "request_duration",
            MetricKind::Absolute,
            &buckets,
            35,
            8.0,
        );
        let via_legacy = OtelMetric::from_legacy_metric(Metric::new(
            "request_duration",
            MetricKind::Absolute,
            MetricValue::AggregatedHistogram {
                buckets,
                count: 35,
                sum: 8.0,
            },
        ));

        assert_eq!(direct.name(), via_legacy.name());
        assert_eq!(direct.kind(), via_legacy.kind());
        assert_eq!(direct.value(), via_legacy.value());
    }

    #[test]
    fn new_summary_matches_from_legacy_metric() {
        use crate::event::{Metric, MetricKind, MetricValue};

        let quantiles = crate::quantiles![0.5 => 100.0, 0.99 => 200.0];
        let direct = OtelMetric::new_summary(
            "request_latency",
            &quantiles,
            50,
            4200.0,
        );
        let via_legacy = OtelMetric::from_legacy_metric(Metric::new(
            "request_latency",
            MetricKind::Absolute,
            MetricValue::AggregatedSummary {
                quantiles,
                count: 50,
                sum: 4200.0,
            },
        ));

        assert_eq!(direct.name(), via_legacy.name());
        assert_eq!(direct.value(), via_legacy.value());
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
        // Timestamp is present.
        assert!(
            ol.get(vrl::event_path!("timestamp"))
                .map(|v| matches!(v, vrl::value::Value::Timestamp(_)))
                .unwrap_or(false),
            "OtelLog has timestamp"
        );
    }

    /// Round-trip fidelity test: OtelLog → to_value_legacy_layout → apply_value_legacy_layout
    /// should produce an OtelLog equivalent to the starting one for field lookups.
    /// This protects against regressions when rewriting apply_value_legacy_layout.
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
        // source_type should survive the round-trip
        assert_eq!(
            event.get(vrl::event_path!("source_type")).and_then(|v| v.as_str().map(|s| s.into_owned())),
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
        // host.name → top-level "host" in value layout
        assert_eq!(
            event.get(vrl::event_path!("host")).and_then(|v| v.as_str().map(|s| s.into_owned())),
            Some("srv01".to_string())
        );
    }

    // --- Round-trip fidelity for native proto fields -----------------------
    //
    // `apply_value_legacy_layout` extracts these fields symmetrically with
    // `to_value_legacy_layout`, so a round-trip via `insert()` preserves them.

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
    fn modify_as_value_equivalent_to_multiple_inserts() {
        // Confirm that one modify_as_value with N mutations produces the
        // same event state as N individual insert() calls.
        let seed_record = LogRecord {
            body: Some(AnyValue {
                value: Some(
                    opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        "seed".into(),
                    ),
                ),
            }),
            ..Default::default()
        };
        let mut via_inserts = OtelLog::new(seed_record.clone());
        via_inserts.insert(vrl::event_path!("field_a"), "a");
        via_inserts.insert(vrl::event_path!("field_b"), 42i64);
        via_inserts.insert(vrl::event_path!("field_c"), true);

        let mut via_modify = OtelLog::new(seed_record);
        via_modify.modify_as_value(|v| {
            if let Some(m) = v.as_object_mut() {
                m.insert("field_a".into(), Value::from("a"));
                m.insert("field_b".into(), Value::from(42i64));
                m.insert("field_c".into(), Value::from(true));
            }
        });

        assert_eq!(via_inserts, via_modify);
    }

    #[test]
    fn with_namespace_tags_timestamp_matches_from_legacy_metric() {
        use crate::event::{Metric, MetricKind, MetricValue};
        use chrono::{TimeZone, Utc};

        let ts = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let tags: super::super::MetricTags = vec![("env".to_string(), "prod".to_string())]
            .into_iter()
            .collect();

        let direct = OtelMetric::new_counter("requests", MetricKind::Incremental, 42.0)
            .with_namespace(Some("http"))
            .with_tags(Some(tags.clone()))
            .with_timestamp(Some(ts));

        let via_legacy = OtelMetric::from_legacy_metric(
            Metric::new(
                "requests",
                MetricKind::Incremental,
                MetricValue::Counter { value: 42.0 },
            )
            .with_namespace(Some("http"))
            .with_tags(Some(tags))
            .with_timestamp(Some(ts)),
        );

        assert_eq!(direct.name(), via_legacy.name());
        assert_eq!(direct.namespace(), via_legacy.namespace());
        assert_eq!(direct.kind(), via_legacy.kind());
        assert_eq!(direct.value(), via_legacy.value());
        assert_eq!(direct.timestamp(), via_legacy.timestamp());
        assert_eq!(direct.tag_value("env"), via_legacy.tag_value("env"));
    }

    #[test]
    fn with_tags_preserves_multi_value() {
        use crate::event::{Metric, MetricKind, MetricValue, metric::TagValue};
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

        let via_legacy = OtelMetric::from_legacy_metric(
            Metric::new(
                "requests",
                MetricKind::Incremental,
                MetricValue::Counter { value: 1.0 },
            )
            .with_tags(Some(tags)),
        );

        // The key "env" must encode as an ArrayValue in both paths.
        let find_env = |m: &OtelMetric| -> any_value::Value {
            m.first_data_point_attributes()
                .iter()
                .find(|a| a.key == "env")
                .and_then(|a| a.value.as_ref())
                .and_then(|v| v.value.clone())
                .expect("env attribute missing")
        };
        match (find_env(&direct), find_env(&via_legacy)) {
            (any_value::Value::ArrayValue(a), any_value::Value::ArrayValue(b)) => {
                assert_eq!(a.values.len(), 3);
                assert_eq!(a.values.len(), b.values.len());
            }
            other => panic!("expected ArrayValue on both sides, got {other:?}"),
        }
        assert_eq!(direct.tag_value("host"), via_legacy.tag_value("host"));
    }
}
