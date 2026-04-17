use std::fmt::Debug;

use lookup::lookup_v2::TargetPath;
use serde::{Deserialize, Serialize};
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq, byte_size_of::ByteSizeOf, internal_event::TaggedEventsSent, json_size::JsonSize,
    request_metadata::GetEventCountTags,
};
use vrl::path::PathParseError;

use super::{
    BatchNotifier, EstimatedJsonEncodedSizeOf, EventFinalizer, EventFinalizers, EventMetadata,
    Finalizable, OtelLog, ObjectMap, Value,
};

/// Traces are a newtype of `OtelLog`
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TraceEvent(OtelLog);

impl TraceEvent {
    /// Convert a `TraceEvent` into a tuple of its components
    /// # Panics
    ///
    /// Panics if the fields of the `TraceEvent` are not a `Value::Map`.
    pub fn into_parts(self) -> (ObjectMap, EventMetadata) {
        let map = self.0.as_map().unwrap_or_default();
        let metadata = self.0.into_parts().3;
        (map, metadata)
    }

    pub fn from_parts(fields: ObjectMap, metadata: EventMetadata) -> Self {
        Self(OtelLog::from_map(fields, metadata))
    }

    pub fn value(&self) -> Value {
        self.0.value()
    }

    pub fn value_mut(&mut self) -> Value {
        self.0.value_mut()
    }

    pub fn metadata(&self) -> &EventMetadata {
        self.0.metadata()
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        self.0.metadata_mut()
    }

    pub fn add_finalizer(&mut self, finalizer: EventFinalizer) {
        self.0.add_finalizer(finalizer);
    }

    #[must_use]
    pub fn with_batch_notifier(self, batch: &BatchNotifier) -> Self {
        Self(self.0.with_batch_notifier(batch))
    }

    #[must_use]
    pub fn with_batch_notifier_option(self, batch: &Option<BatchNotifier>) -> Self {
        Self(self.0.with_batch_notifier_option(batch))
    }

    /// Convert a `TraceEvent` into an `ObjectMap` of its fields
    pub fn as_map(&self) -> ObjectMap {
        self.0.as_map().unwrap_or_default()
    }

    /// Parse the specified `path` and if there are no parsing errors, attempt to get a value.
    /// # Errors
    /// Will return an error if path parsing failed.
    pub fn parse_path_and_get_value(
        &self,
        path: impl AsRef<str>,
    ) -> Result<Option<Value>, PathParseError> {
        self.0.parse_path_and_get_value(path.as_ref())
    }

    #[allow(clippy::needless_pass_by_value)] // TargetPath is always a reference
    pub fn get<'a>(&self, key: impl TargetPath<'a>) -> Option<Value> {
        self.0.get(key)
    }

    pub fn get_mut<'a>(&mut self, _key: impl TargetPath<'a>) -> Option<&mut Value> {
        // OtelLog does not support returning mutable references to values.
        // This method is retained for API compatibility but always returns None.
        None
    }

    pub fn contains<'a>(&self, key: impl TargetPath<'a>) -> bool {
        self.0.contains(key)
    }

    pub fn insert<'a>(
        &mut self,
        key: impl TargetPath<'a>,
        value: impl Into<Value> + Debug,
    ) -> Option<Value> {
        self.0.insert(key, value.into())
    }

    pub fn maybe_insert<'a, F: FnOnce() -> Value>(
        &mut self,
        path: Option<impl TargetPath<'a>>,
        value_callback: F,
    ) -> Option<Value> {
        if let Some(path) = path {
            return self.0.insert(path, value_callback());
        }
        None
    }

    pub fn remove<'a>(&mut self, key: impl TargetPath<'a>) -> Option<Value> {
        self.0.remove(key)
    }
}

impl From<Value> for TraceEvent {
    fn from(value: Value) -> Self {
        Self(OtelLog::from(value))
    }
}

impl From<OtelLog> for TraceEvent {
    fn from(log: OtelLog) -> Self {
        Self(log)
    }
}

impl From<TraceEvent> for OtelLog {
    fn from(trace: TraceEvent) -> Self {
        trace.0
    }
}

impl From<ObjectMap> for TraceEvent {
    fn from(map: ObjectMap) -> Self {
        Self(OtelLog::from(map))
    }
}

impl ByteSizeOf for TraceEvent {
    fn allocated_bytes(&self) -> usize {
        self.0.allocated_bytes()
    }
}

impl EstimatedJsonEncodedSizeOf for TraceEvent {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        self.0.estimated_json_encoded_size_of()
    }
}

impl EventCount for TraceEvent {
    fn event_count(&self) -> usize {
        1
    }
}

impl EventDataEq for TraceEvent {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.0.event_data_eq(&other.0)
    }
}

impl Finalizable for TraceEvent {
    fn take_finalizers(&mut self) -> EventFinalizers {
        self.0.take_finalizers()
    }
}

impl AsRef<OtelLog> for TraceEvent {
    fn as_ref(&self) -> &OtelLog {
        &self.0
    }
}

impl AsMut<OtelLog> for TraceEvent {
    fn as_mut(&mut self) -> &mut OtelLog {
        &mut self.0
    }
}

impl GetEventCountTags for TraceEvent {
    fn get_tags(&self) -> TaggedEventsSent {
        self.0.get_tags()
    }
}
