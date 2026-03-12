use otel_proto_types::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use otel_proto_types::logs::v1::LogRecord;
use otel_proto_types::metrics::v1::Metric as OtelMetricProto;
use otel_proto_types::resource::v1::Resource;
use otel_proto_types::trace::v1::Span;
use prost::Message;
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

use super::{BatchNotifier, EstimatedJsonEncodedSizeOf, EventFinalizer, EventMetadata};

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

// -- OtelLogEvent --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelLogEvent {
    pub(crate) record: LogRecord,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelLogEvent {
    pub fn new(record: LogRecord) -> Self {
        Self {
            record,
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

// -- OtelSpanEvent --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelSpanEvent {
    pub(crate) span: Span,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelSpanEvent {
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

    pub fn status(&self) -> Option<&otel_proto_types::trace::v1::Status> {
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
}

// -- OtelMetricEvent --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelMetricEvent {
    pub(crate) metric: OtelMetricProto,
    pub(crate) resource: Option<Resource>,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) metadata: EventMetadata,
}

impl OtelMetricEvent {
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

impl_otel_event_traits!(OtelLogEvent, record);
impl_otel_event_traits!(OtelSpanEvent, span);
impl_otel_event_traits!(OtelMetricEvent, metric);

// Serde: serialize as JSON via prost-serde (needed for Event derive)

impl Serialize for OtelLogEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelLogEvent", 3)?;
        state.serialize_field("record", &JsonProto(&self.record))?;
        state.serialize_field("resource", &self.resource.as_ref().map(JsonProto))?;
        state.serialize_field("scope", &self.scope.as_ref().map(JsonProto))?;
        state.end()
    }
}

impl Serialize for OtelSpanEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelSpanEvent", 3)?;
        state.serialize_field("span", &JsonProto(&self.span))?;
        state.serialize_field("resource", &self.resource.as_ref().map(JsonProto))?;
        state.serialize_field("scope", &self.scope.as_ref().map(JsonProto))?;
        state.end()
    }
}

impl Serialize for OtelMetricEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("OtelMetricEvent", 3)?;
        state.serialize_field("metric", &JsonProto(&self.metric))?;
        state.serialize_field("resource", &self.resource.as_ref().map(JsonProto))?;
        state.serialize_field("scope", &self.scope.as_ref().map(JsonProto))?;
        state.end()
    }
}

struct JsonProto<'a, M: Message>(&'a M);

impl<M: Message> Serialize for JsonProto<'_, M> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = self.0.encode_to_vec();
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for OtelLogEvent {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        // Full deserialization deferred to Step 5b+; placeholder for Derive on Event
        Err(serde::de::Error::custom(
            "OtelLogEvent deserialization not yet implemented",
        ))
    }
}

impl<'de> Deserialize<'de> for OtelSpanEvent {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "OtelSpanEvent deserialization not yet implemented",
        ))
    }
}

impl<'de> Deserialize<'de> for OtelMetricEvent {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "OtelMetricEvent deserialization not yet implemented",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_log_event_default_fields() {
        let event = OtelLogEvent::new(LogRecord::default());
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
        let mut event = OtelLogEvent::new(LogRecord::default());

        let value = AnyValue {
            value: Some(otel_proto_types::common::v1::any_value::Value::StringValue(
                "bar".to_string(),
            )),
        };

        assert!(event.attribute("foo").is_none());

        event.set_attribute("foo".to_string(), value.clone());
        assert_eq!(event.attribute("foo"), Some(&value));

        let new_value = AnyValue {
            value: Some(otel_proto_types::common::v1::any_value::Value::IntValue(42)),
        };
        event.set_attribute("foo".to_string(), new_value.clone());
        assert_eq!(event.attribute("foo"), Some(&new_value));

        let removed = event.remove_attribute("foo");
        assert_eq!(removed, Some(new_value));
        assert!(event.attribute("foo").is_none());
    }

    #[test]
    fn otel_log_event_resource_attribute() {
        let mut event = OtelLogEvent::new(LogRecord::default());
        assert!(event.resource_attribute("host.name").is_none());

        let host = AnyValue {
            value: Some(otel_proto_types::common::v1::any_value::Value::StringValue(
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
        let event = OtelSpanEvent::new(span);

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
        let event = OtelMetricEvent::new(metric);

        assert_eq!(event.name(), "http.request.duration");
        assert_eq!(event.description(), "Duration of HTTP requests");
        assert_eq!(event.unit(), "ms");
    }

    #[test]
    fn byte_size_of_non_zero() {
        let event = OtelLogEvent::new(LogRecord {
            body: Some(AnyValue {
                value: Some(otel_proto_types::common::v1::any_value::Value::StringValue(
                    "hello world".to_string(),
                )),
            }),
            ..Default::default()
        });
        assert!(event.allocated_bytes() > 0);
    }

    #[test]
    fn event_data_eq_works() {
        let a = OtelLogEvent::new(LogRecord::default());
        let b = OtelLogEvent::new(LogRecord::default());
        assert!(a.event_data_eq(&b));

        let mut c = OtelLogEvent::new(LogRecord::default());
        c.record.severity_text = "ERROR".to_string();
        assert!(!a.event_data_eq(&c));
    }

    #[test]
    fn event_count_is_one() {
        assert_eq!(OtelLogEvent::new(LogRecord::default()).event_count(), 1);
        assert_eq!(OtelSpanEvent::new(Span::default()).event_count(), 1);
        assert_eq!(
            OtelMetricEvent::new(OtelMetricProto::default()).event_count(),
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
            OtelLogEvent::from_parts(record.clone(), resource.clone(), scope.clone(), metadata);
        let (r, res, sc, _meta) = event.into_parts();
        assert_eq!(r.severity_text, "INFO");
        assert_eq!(res, resource);
        assert_eq!(sc.as_ref().map(|s| s.name.as_str()), Some("my-lib"));
    }
}
