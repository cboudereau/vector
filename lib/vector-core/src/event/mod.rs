use std::{convert::TryInto, fmt::Debug, sync::Arc};

pub use array::{
    EventArray, EventContainer, LogArray, MetricArray, OtelLogArray, OtelMetricArray,
    OtelSpanArray, TraceArray, into_event_stream,
};
pub use estimated_json_encoded_size_of::EstimatedJsonEncodedSizeOf;
pub use finalization::{
    BatchNotifier, BatchStatus, BatchStatusReceiver, EventFinalizer, EventFinalizers, EventStatus,
    Finalizable,
};
pub use log_event::LogEvent;
pub use metadata::{EventMetadata, WithMetadata};
pub use metric::{Metric, MetricKind, MetricTags, MetricValue, StatisticKind};
pub use r#ref::{EventMutRef, EventRef};
use serde::{Deserialize, Serialize};
pub use trace::TraceEvent;
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq, byte_size_of::ByteSizeOf, config::ComponentKey, finalization,
    internal_event::TaggedEventsSent, json_size::JsonSize, request_metadata::GetEventCountTags,
};
pub use vrl::value::{KeyString, ObjectMap, Value};
#[cfg(feature = "vrl")]
pub use vrl_target::{TargetEvents, VrlTarget};

use crate::config::{LogNamespace, OutputId};

pub mod array;
pub mod discriminant;
mod estimated_json_encoded_size_of;
mod log_event;
#[cfg(feature = "lua")]
pub mod lua;
pub mod merge_state;
mod metadata;
pub mod metric;
pub mod proto;
mod r#ref;
mod ser;
pub mod otel_event;
pub(crate) mod otel_json;
pub mod otlp;
pub use otel_event::{OtelLog, OtelMetric, OtelSpan, json_to_any_value, string_value, int_value, vrl_value_to_any_value};
pub use opentelemetry_proto::tonic::common::v1::any_value::Value as OtelValueKind;

/// Backward-compat aliases.
pub type OtelLogEvent = OtelLog;
pub type OtelMetricEvent = OtelMetric;
pub type OtelSpanEvent = OtelSpan;
pub use otlp::{OtlpCodec, register_otlp_codec};
pub use ser::{
    BufferFormat, BUFFER_FORMAT, EventEncodableMetadata, EventEncodableMetadataFlags,
};
#[cfg(test)]
mod test;
mod trace;
pub mod util;
#[cfg(feature = "vrl")]
mod vrl_target;

pub const PARTIAL: &str = "_partial";

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Log(OtelLog),
    Metric(OtelMetric),
    Trace(OtelSpan),
}

impl ByteSizeOf for Event {
    fn allocated_bytes(&self) -> usize {
        match self {
            Event::Log(e) => e.allocated_bytes(),
            Event::Metric(e) => e.allocated_bytes(),
            Event::Trace(e) => e.allocated_bytes(),
        }
    }
}

impl EstimatedJsonEncodedSizeOf for Event {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        match self {
            Event::Log(e) => e.estimated_json_encoded_size_of(),
            Event::Metric(e) => e.estimated_json_encoded_size_of(),
            Event::Trace(e) => e.estimated_json_encoded_size_of(),
        }
    }
}

impl EventCount for Event {
    fn event_count(&self) -> usize {
        1
    }
}

impl Finalizable for Event {
    fn take_finalizers(&mut self) -> EventFinalizers {
        match self {
            Event::Log(e) => e.take_finalizers(),
            Event::Metric(e) => e.take_finalizers(),
            Event::Trace(e) => e.take_finalizers(),
        }
    }
}

impl GetEventCountTags for Event {
    fn get_tags(&self) -> TaggedEventsSent {
        match self {
            Event::Log(e) => e.get_tags(),
            Event::Metric(e) => e.get_tags(),
            Event::Trace(e) => e.get_tags(),
        }
    }
}

impl Event {
    /// Return self as an `OtelLog` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Log`.
    pub fn as_log(&self) -> &OtelLog {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Return self as a mutable `OtelLog` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Log`.
    pub fn as_mut_log(&mut self) -> &mut OtelLog {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Coerces self into an `OtelLog`.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Log`.
    pub fn into_log(self) -> OtelLog {
        match self {
            Event::Log(log) => log,
            _ => panic!("Failed type coercion, {self:?} is not a log event"),
        }
    }

    /// Fallibly coerces self into an `OtelLog`.
    pub fn try_into_log(self) -> Option<OtelLog> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Backward-compat alias for `try_into_log`.
    pub fn try_into_log_coerce(self) -> Option<OtelLog> {
        self.try_into_log()
    }

    /// Backward-compat alias for `into_log`.
    pub fn into_log_coerce(self) -> OtelLog {
        self.into_log()
    }

    /// Return self as an `OtelLog` if possible.
    pub fn maybe_as_log(&self) -> Option<&OtelLog> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }

    /// Return self as an `OtelMetric` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Metric`.
    pub fn as_metric(&self) -> &OtelMetric {
        match self {
            Event::Metric(metric) => metric,
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Return self as a mutable `OtelMetric` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Metric`.
    pub fn as_mut_metric(&mut self) -> &mut OtelMetric {
        match self {
            Event::Metric(metric) => metric,
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Convert self to a legacy `Metric` (for backward compat with sinks).
    ///
    /// # Panics
    ///
    /// This function panics if self is not a metric event.
    pub fn to_metric(&self) -> Metric {
        match self {
            Event::Metric(otel) => otel.clone().to_legacy_metric(),
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Coerces self into a legacy `Metric`.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Metric`.
    pub fn into_metric(self) -> Metric {
        match self {
            Event::Metric(otel) => otel.to_legacy_metric(),
            _ => panic!("Failed type coercion, {self:?} is not a metric"),
        }
    }

    /// Fallibly coerces self into a legacy `Metric`.
    pub fn try_into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(otel) => Some(otel.to_legacy_metric()),
            _ => None,
        }
    }

    /// Return self as an `OtelSpan` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Trace`.
    pub fn as_trace(&self) -> &OtelSpan {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Return self as a mutable `OtelSpan` reference.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Trace`.
    pub fn as_mut_trace(&mut self) -> &mut OtelSpan {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Convert to a JSON value for serialization.
    /// Wraps the event in `{"log": ...}`, `{"metric": ...}`, or `{"trace": ...}`.
    pub fn to_legacy_json_value(self) -> serde_json::Value {
        match self {
            Event::Log(log) => {
                let v = serde_json::to_value(&log).unwrap_or(serde_json::Value::Null);
                serde_json::json!({"log": v})
            }
            Event::Metric(metric) => {
                let v = serde_json::to_value(&metric).unwrap_or(serde_json::Value::Null);
                serde_json::json!({"metric": v})
            }
            Event::Trace(trace) => {
                let v = serde_json::to_value(&trace).unwrap_or(serde_json::Value::Null);
                serde_json::json!({"trace": v})
            }
        }
    }

    /// Coerces self into an `OtelSpan`.
    ///
    /// # Panics
    ///
    /// This function panics if self is not an `Event::Trace`.
    pub fn into_trace(self) -> OtelSpan {
        match self {
            Event::Trace(trace) => trace,
            _ => panic!("Failed type coercion, {self:?} is not a trace event"),
        }
    }

    /// Fallibly coerces self into an `OtelSpan`.
    pub fn try_into_trace(self) -> Option<OtelSpan> {
        match self {
            Event::Trace(trace) => Some(trace),
            _ => None,
        }
    }

    pub fn metadata(&self) -> &EventMetadata {
        match self {
            Self::Log(e) => e.metadata(),
            Self::Metric(e) => e.metadata(),
            Self::Trace(e) => e.metadata(),
        }
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        match self {
            Self::Log(e) => e.metadata_mut(),
            Self::Metric(e) => e.metadata_mut(),
            Self::Trace(e) => e.metadata_mut(),
        }
    }

    /// Destroy the event and return the metadata.
    pub fn into_metadata(self) -> EventMetadata {
        match self {
            Self::Log(e) => e.into_parts().3,
            Self::Metric(e) => e.into_parts().3,
            Self::Trace(e) => e.into_parts().3,
        }
    }

    #[must_use]
    pub fn with_batch_notifier(self, batch: &BatchNotifier) -> Self {
        match self {
            Self::Log(e) => e.with_batch_notifier(batch).into(),
            Self::Metric(e) => e.with_batch_notifier(batch).into(),
            Self::Trace(e) => e.with_batch_notifier(batch).into(),
        }
    }

    #[must_use]
    pub fn with_batch_notifier_option(self, batch: &Option<BatchNotifier>) -> Self {
        match self {
            Self::Log(e) => e.with_batch_notifier_option(batch).into(),
            Self::Metric(e) => e.with_batch_notifier_option(batch).into(),
            Self::Trace(e) => e.with_batch_notifier_option(batch).into(),
        }
    }

    /// Backward-compat aliases — these just delegate to the renamed variants.
    pub fn as_otel_log(&self) -> &OtelLog { self.as_log() }
    pub fn as_mut_otel_log(&mut self) -> &mut OtelLog { self.as_mut_log() }
    pub fn into_otel_log(self) -> OtelLog { self.into_log() }
    pub fn try_into_otel_log(self) -> Option<OtelLog> { self.try_into_log() }
    pub fn maybe_as_otel_log(&self) -> Option<&OtelLog> { self.maybe_as_log() }
    pub fn as_otel_span(&self) -> &OtelSpan { self.as_trace() }
    pub fn as_mut_otel_span(&mut self) -> &mut OtelSpan { self.as_mut_trace() }
    pub fn into_otel_span(self) -> OtelSpan { self.into_trace() }
    pub fn try_into_otel_span(self) -> Option<OtelSpan> { self.try_into_trace() }
    pub fn as_otel_metric(&self) -> &OtelMetric { self.as_metric() }
    pub fn as_mut_otel_metric(&mut self) -> &mut OtelMetric { self.as_mut_metric() }
    pub fn into_otel_metric(self) -> OtelMetric { match self { Event::Metric(e) => e, _ => panic!("not a metric") } }
    pub fn try_into_otel_metric(self) -> Option<OtelMetric> { match self { Event::Metric(e) => Some(e), _ => None } }

    /// Returns a reference to the event metadata source.
    #[must_use]
    pub fn source_id(&self) -> Option<&Arc<ComponentKey>> {
        self.metadata().source_id()
    }

    /// Sets the `source_id` in the event metadata to the provided value.
    pub fn set_source_id(&mut self, source_id: Arc<ComponentKey>) {
        self.metadata_mut().set_source_id(source_id);
    }

    /// Sets the `upstream_id` in the event metadata to the provided value.
    pub fn set_upstream_id(&mut self, upstream_id: Arc<OutputId>) {
        self.metadata_mut().set_upstream_id(upstream_id);
    }

    /// Sets the `source_type` in the event metadata to the provided value.
    pub fn set_source_type(&mut self, source_type: &'static str) {
        self.metadata_mut().set_source_type(source_type);
    }

    /// Sets the `source_id` in the event metadata to the provided value.
    #[must_use]
    pub fn with_source_id(mut self, source_id: Arc<ComponentKey>) -> Self {
        self.metadata_mut().set_source_id(source_id);
        self
    }

    /// Sets the `source_type` in the event metadata to the provided value.
    #[must_use]
    pub fn with_source_type(mut self, source_type: &'static str) -> Self {
        self.metadata_mut().set_source_type(source_type);
        self
    }

    /// Sets the `upstream_id` in the event metadata to the provided value.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: Arc<OutputId>) -> Self {
        self.metadata_mut().set_upstream_id(upstream_id);
        self
    }

    /// Creates an Event from a JSON value.
    ///
    /// # Errors
    /// If a non-object JSON value is passed in with the `Legacy` namespace, this will return an error.
    pub fn from_json_value(
        value: serde_json::Value,
        log_namespace: LogNamespace,
    ) -> crate::Result<Self> {
        match log_namespace {
            LogNamespace::Vector => Ok(LogEvent::from(Value::from(value)).into()),
            LogNamespace::Legacy => match value {
                serde_json::Value::Object(fields) => Ok(LogEvent::from(
                    fields
                        .into_iter()
                        .map(|(k, v)| (k.into(), v.into()))
                        .collect::<ObjectMap>(),
                )
                .into()),
                _ => Err(crate::Error::from(
                    "Attempted to convert non-Object JSON into an Event.",
                )),
            },
        }
    }
}

impl EventDataEq for Event {
    fn event_data_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Log(a), Self::Log(b)) => a.event_data_eq(b),
            (Self::Metric(a), Self::Metric(b)) => a.event_data_eq(b),
            (Self::Trace(a), Self::Trace(b)) => a.event_data_eq(b),
            _ => false,
        }
    }
}

impl finalization::AddBatchNotifier for Event {
    fn add_batch_notifier(&mut self, batch: BatchNotifier) {
        let finalizer = EventFinalizer::new(batch);
        match self {
            Self::Log(e) => e.add_finalizer(finalizer),
            Self::Metric(e) => e.add_finalizer(finalizer),
            Self::Trace(e) => e.add_finalizer(finalizer),
        }
    }
}

impl TryInto<serde_json::Value> for Event {
    type Error = serde_json::Error;

    fn try_into(self) -> Result<serde_json::Value, Self::Error> {
        match self {
            Event::Log(e) => serde_json::to_value(e),
            Event::Metric(e) => serde_json::to_value(e),
            Event::Trace(e) => serde_json::to_value(e),
        }
    }
}

impl From<proto::StatisticKind> for StatisticKind {
    fn from(kind: proto::StatisticKind) -> Self {
        match kind {
            proto::StatisticKind::Histogram => StatisticKind::Histogram,
            proto::StatisticKind::Summary => StatisticKind::Summary,
        }
    }
}

impl From<metric::Sample> for proto::DistributionSample {
    fn from(sample: metric::Sample) -> Self {
        Self {
            value: sample.value,
            rate: sample.rate,
        }
    }
}

impl From<proto::DistributionSample> for metric::Sample {
    fn from(sample: proto::DistributionSample) -> Self {
        Self {
            value: sample.value,
            rate: sample.rate,
        }
    }
}

impl From<proto::HistogramBucket> for metric::Bucket {
    fn from(bucket: proto::HistogramBucket) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: u64::from(bucket.count),
        }
    }
}

impl From<metric::Bucket> for proto::HistogramBucket3 {
    fn from(bucket: metric::Bucket) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: bucket.count,
        }
    }
}

impl From<proto::HistogramBucket3> for metric::Bucket {
    fn from(bucket: proto::HistogramBucket3) -> Self {
        Self {
            upper_limit: bucket.upper_limit,
            count: bucket.count,
        }
    }
}

impl From<metric::Quantile> for proto::SummaryQuantile {
    fn from(quantile: metric::Quantile) -> Self {
        Self {
            quantile: quantile.quantile,
            value: quantile.value,
        }
    }
}

impl From<proto::SummaryQuantile> for metric::Quantile {
    fn from(quantile: proto::SummaryQuantile) -> Self {
        Self {
            quantile: quantile.quantile,
            value: quantile.value,
        }
    }
}

impl From<OtelLog> for Event {
    fn from(e: OtelLog) -> Self {
        Event::Log(e)
    }
}

impl From<OtelMetric> for Event {
    fn from(e: OtelMetric) -> Self {
        Event::Metric(e)
    }
}

impl From<OtelSpan> for Event {
    fn from(e: OtelSpan) -> Self {
        Event::Trace(e)
    }
}

impl From<Metric> for Event {
    fn from(metric: Metric) -> Self {
        Event::Metric(OtelMetric::from_legacy_metric(metric))
    }
}

impl From<LogEvent> for Event {
    fn from(log: LogEvent) -> Self {
        Event::Log(OtelLog::from_log_event(log))
    }
}

impl From<TraceEvent> for Event {
    fn from(trace: TraceEvent) -> Self {
        Event::Trace(OtelSpan::from_trace_event(trace))
    }
}

pub trait MaybeAsLogMut {
    fn maybe_as_log_mut(&mut self) -> Option<&mut OtelLog>;
}

impl MaybeAsLogMut for Event {
    fn maybe_as_log_mut(&mut self) -> Option<&mut OtelLog> {
        match self {
            Event::Log(log) => Some(log),
            _ => None,
        }
    }
}
