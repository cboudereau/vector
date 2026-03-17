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
        let body_str = String::from_utf8_lossy(&bytes).into_owned();
        Self {
            record: LogRecord {
                body: Some(AnyValue {
                    value: Some(OtelValueKind::StringValue(body_str)),
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
    /// Equivalent to `LogNamespace::insert_standard_vector_source_metadata`.
    pub fn set_source_metadata(
        &mut self,
        source_name: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        self.set_resource_attribute("source_type".to_string(), string_value(source_name));
        self.set_observed_timestamp(now);
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

    /// Lossy projection of this OTel log event into a legacy `LogEvent`.
    ///
    /// The body becomes `message`, attributes become top-level fields, and
    /// resource / scope are preserved as nested objects.  Useful for
    /// text-oriented serializers (text, logfmt, CSV, GELF, CEF, syslog, etc.)
    /// that only understand `LogEvent`.
    pub fn to_log_event(&self) -> LogEvent {
        let mut map = ObjectMap::new();

        if let Some(body) = self.body() {
            map.insert("message".into(), any_value_to_vrl(body));
        }

        for kv in &self.record.attributes {
            let v = kv
                .value
                .as_ref()
                .map(any_value_to_vrl)
                .unwrap_or(Value::Null);
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
        if self.record.time_unix_nano != 0 {
            let nanos = self.record.time_unix_nano;
            let secs = (nanos / 1_000_000_000) as i64;
            let nsecs = (nanos % 1_000_000_000) as u32;
            if let Some(ts) = chrono::DateTime::from_timestamp(secs, nsecs) {
                map.insert("timestamp".into(), Value::Timestamp(ts));
            }
        }
        if !self.record.trace_id.is_empty() {
            map.insert("trace_id".into(), hex_encode(&self.record.trace_id));
        }
        if !self.record.span_id.is_empty() {
            map.insert("span_id".into(), hex_encode(&self.record.span_id));
        }

        if let Some(resource) = &self.resource {
            let mut res_map = kvlist_to_object_map(&resource.attributes);
            if resource.dropped_attributes_count != 0 {
                res_map.insert(
                    "dropped_attributes_count".into(),
                    Value::Integer(resource.dropped_attributes_count as i64),
                );
            }
            map.insert("resource".into(), Value::Object(res_map));
        }

        if let Some(scope) = &self.scope {
            let mut scope_map = ObjectMap::new();
            if !scope.name.is_empty() {
                scope_map.insert("name".into(), Value::Bytes(scope.name.clone().into()));
            }
            if !scope.version.is_empty() {
                scope_map
                    .insert("version".into(), Value::Bytes(scope.version.clone().into()));
            }
            if !scope.attributes.is_empty() {
                scope_map.insert(
                    "attributes".into(),
                    Value::Object(kvlist_to_object_map(&scope.attributes)),
                );
            }
            map.insert("scope".into(), Value::Object(scope_map));
        }

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

        if let Some(resource) = &self.resource {
            let mut res_map = kvlist_to_object_map(&resource.attributes);
            if resource.dropped_attributes_count != 0 {
                res_map.insert(
                    "dropped_attributes_count".into(),
                    Value::Integer(resource.dropped_attributes_count as i64),
                );
            }
            map.insert("resource".into(), Value::Object(res_map));
        }

        if let Some(scope) = &self.scope {
            let mut scope_map = ObjectMap::new();
            if !scope.name.is_empty() {
                scope_map.insert("name".into(), Value::Bytes(scope.name.clone().into()));
            }
            if !scope.version.is_empty() {
                scope_map
                    .insert("version".into(), Value::Bytes(scope.version.clone().into()));
            }
            if !scope.attributes.is_empty() {
                scope_map.insert(
                    "attributes".into(),
                    Value::Object(kvlist_to_object_map(&scope.attributes)),
                );
            }
            map.insert("scope".into(), Value::Object(scope_map));
        }

        LogEvent::from_map(map, self.metadata.clone())
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

        impl EventDataEq for $ty {
            fn event_data_eq(&self, other: &Self) -> bool {
                self.$proto_field == other.$proto_field
                    && self.resource == other.resource
                    && self.scope == other.scope
            }
        }

        impl GetEventCountTags for $ty {
            fn get_tags(&self) -> TaggedEventsSent {
                TaggedEventsSent::new_unspecified()
            }
        }
    };
}

impl_otel_event_traits!(OtelLog, record);
impl_otel_event_traits!(OtelSpan, span);
impl_otel_event_traits!(OtelMetric, metric);

impl Serialize for OtelLog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelLog", 3)?;
        state.serialize_field("record", &self.record)?;
        state.serialize_field("resource", &self.resource)?;
        state.serialize_field("scope", &self.scope)?;
        state.end()
    }
}

impl Serialize for OtelSpan {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelSpan", 3)?;
        state.serialize_field("span", &self.span)?;
        state.serialize_field("resource", &self.resource)?;
        state.serialize_field("scope", &self.scope)?;
        state.end()
    }
}

impl Serialize for OtelMetric {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelMetric", 3)?;
        state.serialize_field("metric", &self.metric)?;
        state.serialize_field("resource", &self.resource)?;
        state.serialize_field("scope", &self.scope)?;
        state.end()
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
}
