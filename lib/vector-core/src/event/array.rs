#![deny(missing_docs)]
//! This module contains the definitions and wrapper types for handling
//! arrays of type `Event`, in the various forms they may appear.

use std::{iter, slice, vec};

use futures::{Stream, stream};
#[cfg(test)]
use quickcheck::{Arbitrary, Gen};
use vector_buffers::EventCount;
use vector_common::{
    byte_size_of::ByteSizeOf,
    finalization::{AddBatchNotifier, BatchNotifier, EventFinalizers, Finalizable},
    json_size::JsonSize,
};

use super::{
    EstimatedJsonEncodedSizeOf, Event, EventDataEq, EventFinalizer, EventMetadata, EventMutRef,
    EventRef, LogEvent, Metric, OtelLogEvent, OtelMetricEvent, OtelSpanEvent, TraceEvent,
};

/// The type alias for an array of `LogEvent` elements.
pub type LogArray = Vec<LogEvent>;

/// The type alias for an array of `TraceEvent` elements.
pub type TraceArray = Vec<TraceEvent>;

/// The type alias for an array of `Metric` elements.
pub type MetricArray = Vec<Metric>;

/// The type alias for an array of `OtelLogEvent` elements.
pub type OtelLogArray = Vec<OtelLogEvent>;

/// The type alias for an array of `OtelSpanEvent` elements.
pub type OtelSpanArray = Vec<OtelSpanEvent>;

/// The type alias for an array of `OtelMetricEvent` elements.
pub type OtelMetricArray = Vec<OtelMetricEvent>;

/// The core trait to abstract over any type that may work as an array
/// of events. This is effectively the same as the standard
/// `IntoIterator<Item = Event>` implementations, but that would
/// conflict with the base implementation for the type aliases below.
pub trait EventContainer: ByteSizeOf + EstimatedJsonEncodedSizeOf {
    /// The type of `Iterator` used to turn this container into events.
    type IntoIter: Iterator<Item = Event>;

    /// The number of events in this container.
    fn len(&self) -> usize;

    /// Is this container empty?
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Turn this container into an iterator over `Event`.
    fn into_events(self) -> Self::IntoIter;
}

/// Turn a container into a futures stream over the contained `Event`
/// type.  This would ideally be implemented as a default method on
/// `trait EventContainer`, but the required feature (associated type
/// defaults) is still unstable.
/// See <https://github.com/rust-lang/rust/issues/29661>
pub fn into_event_stream(container: impl EventContainer) -> impl Stream<Item = Event> + Unpin {
    stream::iter(container.into_events())
}

impl EventContainer for Event {
    type IntoIter = iter::Once<Event>;

    fn len(&self) -> usize {
        1
    }

    fn is_empty(&self) -> bool {
        false
    }

    fn into_events(self) -> Self::IntoIter {
        iter::once(self)
    }
}

impl EventContainer for LogEvent {
    type IntoIter = iter::Once<Event>;

    fn len(&self) -> usize {
        1
    }

    fn is_empty(&self) -> bool {
        false
    }

    fn into_events(self) -> Self::IntoIter {
        iter::once(self.into())
    }
}

impl EventContainer for Metric {
    type IntoIter = iter::Once<Event>;

    fn len(&self) -> usize {
        1
    }

    fn is_empty(&self) -> bool {
        false
    }

    fn into_events(self) -> Self::IntoIter {
        iter::once(self.into())
    }
}

impl EventContainer for LogArray {
    type IntoIter = iter::Map<vec::IntoIter<LogEvent>, fn(LogEvent) -> Event>;

    fn len(&self) -> usize {
        self.len()
    }

    fn into_events(self) -> Self::IntoIter {
        self.into_iter().map(Into::into)
    }
}

impl EventContainer for MetricArray {
    type IntoIter = iter::Map<vec::IntoIter<Metric>, fn(Metric) -> Event>;

    fn len(&self) -> usize {
        self.len()
    }

    fn into_events(self) -> Self::IntoIter {
        self.into_iter().map(Into::into)
    }
}

/// An array of one of the `Event` variants exclusively.
#[derive(Clone, Debug, PartialEq)]
pub enum EventArray {
    /// An array of type `LogEvent`
    Logs(LogArray),
    /// An array of type `Metric`
    Metrics(MetricArray),
    /// An array of type `TraceEvent`
    Traces(TraceArray),
    /// An array of type `OtelLogEvent`
    OtelLogs(OtelLogArray),
    /// An array of type `OtelMetricEvent`
    OtelMetrics(OtelMetricArray),
    /// An array of type `OtelSpanEvent`
    OtelSpans(OtelSpanArray),
}

impl EventArray {
    /// Iterate over references to this array's events.
    pub fn iter_events(&self) -> impl Iterator<Item = EventRef<'_>> {
        match self {
            Self::Logs(array) => EventArrayIter::Logs(array.iter()),
            Self::Metrics(array) => EventArrayIter::Metrics(array.iter()),
            Self::Traces(array) => EventArrayIter::Traces(array.iter()),
            Self::OtelLogs(array) => EventArrayIter::OtelLogs(array.iter()),
            Self::OtelMetrics(array) => EventArrayIter::OtelMetrics(array.iter()),
            Self::OtelSpans(array) => EventArrayIter::OtelSpans(array.iter()),
        }
    }

    /// Iterate over mutable references to this array's events.
    pub fn iter_events_mut(&mut self) -> impl Iterator<Item = EventMutRef<'_>> {
        match self {
            Self::Logs(array) => EventArrayIterMut::Logs(array.iter_mut()),
            Self::Metrics(array) => EventArrayIterMut::Metrics(array.iter_mut()),
            Self::Traces(array) => EventArrayIterMut::Traces(array.iter_mut()),
            Self::OtelLogs(array) => EventArrayIterMut::OtelLogs(array.iter_mut()),
            Self::OtelMetrics(array) => EventArrayIterMut::OtelMetrics(array.iter_mut()),
            Self::OtelSpans(array) => EventArrayIterMut::OtelSpans(array.iter_mut()),
        }
    }

    /// Iterate over references to the logs in this array.
    pub fn iter_logs_mut(&mut self) -> impl Iterator<Item = &mut LogEvent> {
        match self {
            Self::Logs(array) => TypedArrayIterMut(Some(array.iter_mut())),
            _ => TypedArrayIterMut(None),
        }
    }

    /// Applies a closure to each event's metadata in this array.
    pub fn for_each_metadata_mut(&mut self, mut f: impl FnMut(&mut EventMetadata)) {
        match self {
            Self::Logs(logs) => logs.iter_mut().for_each(|e| f(e.metadata_mut())),
            Self::Metrics(metrics) => metrics.iter_mut().for_each(|e| f(e.metadata_mut())),
            Self::Traces(traces) => traces.iter_mut().for_each(|e| f(e.metadata_mut())),
            Self::OtelLogs(a) => a.iter_mut().for_each(|e| f(e.metadata_mut())),
            Self::OtelMetrics(a) => a.iter_mut().for_each(|e| f(e.metadata_mut())),
            Self::OtelSpans(a) => a.iter_mut().for_each(|e| f(e.metadata_mut())),
        }
    }
}

impl From<Event> for EventArray {
    fn from(event: Event) -> Self {
        match event {
            Event::Log(log) => Self::Logs(vec![log]),
            Event::Metric(metric) => Self::Metrics(vec![metric]),
            Event::Trace(trace) => Self::Traces(vec![trace]),
            Event::OtelLog(e) => Self::OtelLogs(vec![e]),
            Event::OtelMetric(e) => Self::OtelMetrics(vec![e]),
            Event::OtelSpan(e) => Self::OtelSpans(vec![e]),
        }
    }
}

impl From<LogEvent> for EventArray {
    fn from(log: LogEvent) -> Self {
        Event::from(log).into()
    }
}

impl From<Metric> for EventArray {
    fn from(metric: Metric) -> Self {
        Event::from(metric).into()
    }
}

impl From<TraceEvent> for EventArray {
    fn from(trace: TraceEvent) -> Self {
        Event::from(trace).into()
    }
}

impl From<LogArray> for EventArray {
    fn from(array: LogArray) -> Self {
        Self::Logs(array)
    }
}

impl From<MetricArray> for EventArray {
    fn from(array: MetricArray) -> Self {
        Self::Metrics(array)
    }
}

impl AddBatchNotifier for EventArray {
    fn add_batch_notifier(&mut self, batch: BatchNotifier) {
        macro_rules! add_notifier {
            ($array:expr, $batch:expr) => {
                $array
                    .iter_mut()
                    .for_each(|item| item.add_finalizer(EventFinalizer::new($batch.clone())))
            };
        }
        match self {
            Self::Logs(a) => add_notifier!(a, batch),
            Self::Metrics(a) => add_notifier!(a, batch),
            Self::Traces(a) => add_notifier!(a, batch),
            Self::OtelLogs(a) => add_notifier!(a, batch),
            Self::OtelMetrics(a) => add_notifier!(a, batch),
            Self::OtelSpans(a) => add_notifier!(a, batch),
        }
    }
}

impl ByteSizeOf for EventArray {
    fn allocated_bytes(&self) -> usize {
        match self {
            Self::Logs(a) => a.allocated_bytes(),
            Self::Metrics(a) => a.allocated_bytes(),
            Self::Traces(a) => a.allocated_bytes(),
            Self::OtelLogs(a) => a.allocated_bytes(),
            Self::OtelMetrics(a) => a.allocated_bytes(),
            Self::OtelSpans(a) => a.allocated_bytes(),
        }
    }
}

impl EstimatedJsonEncodedSizeOf for EventArray {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        match self {
            Self::Logs(v) => v.estimated_json_encoded_size_of(),
            Self::Metrics(v) => v.estimated_json_encoded_size_of(),
            Self::Traces(v) => v.estimated_json_encoded_size_of(),
            Self::OtelLogs(v) => v.estimated_json_encoded_size_of(),
            Self::OtelMetrics(v) => v.estimated_json_encoded_size_of(),
            Self::OtelSpans(v) => v.estimated_json_encoded_size_of(),
        }
    }
}

impl EventCount for EventArray {
    fn event_count(&self) -> usize {
        match self {
            Self::Logs(a) => a.len(),
            Self::Metrics(a) => a.len(),
            Self::Traces(a) => a.len(),
            Self::OtelLogs(a) => a.len(),
            Self::OtelMetrics(a) => a.len(),
            Self::OtelSpans(a) => a.len(),
        }
    }
}

impl EventContainer for EventArray {
    type IntoIter = EventArrayIntoIter;

    fn len(&self) -> usize {
        match self {
            Self::Logs(a) => a.len(),
            Self::Metrics(a) => a.len(),
            Self::Traces(a) => a.len(),
            Self::OtelLogs(a) => a.len(),
            Self::OtelMetrics(a) => a.len(),
            Self::OtelSpans(a) => a.len(),
        }
    }

    fn into_events(self) -> Self::IntoIter {
        match self {
            Self::Logs(a) => EventArrayIntoIter::Logs(a.into_iter()),
            Self::Metrics(a) => EventArrayIntoIter::Metrics(a.into_iter()),
            Self::Traces(a) => EventArrayIntoIter::Traces(a.into_iter()),
            Self::OtelLogs(a) => EventArrayIntoIter::OtelLogs(a.into_iter()),
            Self::OtelMetrics(a) => EventArrayIntoIter::OtelMetrics(a.into_iter()),
            Self::OtelSpans(a) => EventArrayIntoIter::OtelSpans(a.into_iter()),
        }
    }
}

impl EventDataEq for EventArray {
    fn event_data_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Logs(a), Self::Logs(b)) => a.event_data_eq(b),
            (Self::Metrics(a), Self::Metrics(b)) => a.event_data_eq(b),
            (Self::Traces(a), Self::Traces(b)) => a.event_data_eq(b),
            (Self::OtelLogs(a), Self::OtelLogs(b)) => a.event_data_eq(b),
            (Self::OtelMetrics(a), Self::OtelMetrics(b)) => a.event_data_eq(b),
            (Self::OtelSpans(a), Self::OtelSpans(b)) => a.event_data_eq(b),
            _ => false,
        }
    }
}

impl Finalizable for EventArray {
    fn take_finalizers(&mut self) -> EventFinalizers {
        match self {
            Self::Logs(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
            Self::Metrics(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
            Self::Traces(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
            Self::OtelLogs(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
            Self::OtelMetrics(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
            Self::OtelSpans(a) => a.iter_mut().map(Finalizable::take_finalizers).collect(),
        }
    }
}

#[cfg(test)]
impl Arbitrary for EventArray {
    fn arbitrary(g: &mut Gen) -> Self {
        let len = u8::arbitrary(g) as usize;
        let choice: u8 = u8::arbitrary(g);
        // Quickcheck can't derive Arbitrary for enums, see
        // https://github.com/BurntSushi/quickcheck/issues/98
        if choice.is_multiple_of(2) {
            let mut logs = Vec::new();
            for _ in 0..len {
                logs.push(LogEvent::arbitrary(g));
            }
            EventArray::Logs(logs)
        } else {
            let mut metrics = Vec::new();
            for _ in 0..len {
                metrics.push(Metric::arbitrary(g));
            }
            EventArray::Metrics(metrics)
        }
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        match self {
            EventArray::Logs(logs) => Box::new(logs.shrink().map(EventArray::Logs)),
            EventArray::Metrics(metrics) => Box::new(metrics.shrink().map(EventArray::Metrics)),
            EventArray::Traces(traces) => Box::new(traces.shrink().map(EventArray::Traces)),
            EventArray::OtelLogs(_)
            | EventArray::OtelMetrics(_)
            | EventArray::OtelSpans(_) => Box::new(std::iter::empty()),
        }
    }
}

/// The iterator type for `EventArray::iter_events`.
#[derive(Debug)]
pub enum EventArrayIter<'a> {
    /// An iterator over type `LogEvent`.
    Logs(slice::Iter<'a, LogEvent>),
    /// An iterator over type `Metric`.
    Metrics(slice::Iter<'a, Metric>),
    /// An iterator over type `Trace`.
    Traces(slice::Iter<'a, TraceEvent>),
    /// An iterator over type `OtelLogEvent`.
    OtelLogs(slice::Iter<'a, OtelLogEvent>),
    /// An iterator over type `OtelMetricEvent`.
    OtelMetrics(slice::Iter<'a, OtelMetricEvent>),
    /// An iterator over type `OtelSpanEvent`.
    OtelSpans(slice::Iter<'a, OtelSpanEvent>),
}

impl<'a> Iterator for EventArrayIter<'a> {
    type Item = EventRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Logs(i) => i.next().map(EventRef::from),
            Self::Metrics(i) => i.next().map(EventRef::from),
            Self::Traces(i) => i.next().map(EventRef::from),
            Self::OtelLogs(i) => i.next().map(EventRef::OtelLog),
            Self::OtelMetrics(i) => i.next().map(EventRef::OtelMetric),
            Self::OtelSpans(i) => i.next().map(EventRef::OtelSpan),
        }
    }
}

/// The iterator type for `EventArray::iter_events_mut`.
#[derive(Debug)]
pub enum EventArrayIterMut<'a> {
    /// An iterator over type `LogEvent`.
    Logs(slice::IterMut<'a, LogEvent>),
    /// An iterator over type `Metric`.
    Metrics(slice::IterMut<'a, Metric>),
    /// An iterator over type `Trace`.
    Traces(slice::IterMut<'a, TraceEvent>),
    /// An iterator over type `OtelLogEvent`.
    OtelLogs(slice::IterMut<'a, OtelLogEvent>),
    /// An iterator over type `OtelMetricEvent`.
    OtelMetrics(slice::IterMut<'a, OtelMetricEvent>),
    /// An iterator over type `OtelSpanEvent`.
    OtelSpans(slice::IterMut<'a, OtelSpanEvent>),
}

impl<'a> Iterator for EventArrayIterMut<'a> {
    type Item = EventMutRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Logs(i) => i.next().map(EventMutRef::from),
            Self::Metrics(i) => i.next().map(EventMutRef::from),
            Self::Traces(i) => i.next().map(EventMutRef::from),
            Self::OtelLogs(i) => i.next().map(EventMutRef::OtelLog),
            Self::OtelMetrics(i) => i.next().map(EventMutRef::OtelMetric),
            Self::OtelSpans(i) => i.next().map(EventMutRef::OtelSpan),
        }
    }
}

/// The iterator type for `EventArray::into_events`.
#[derive(Debug)]
pub enum EventArrayIntoIter {
    /// An iterator over type `LogEvent`.
    Logs(vec::IntoIter<LogEvent>),
    /// An iterator over type `Metric`.
    Metrics(vec::IntoIter<Metric>),
    /// An iterator over type `TraceEvent`.
    Traces(vec::IntoIter<TraceEvent>),
    /// An iterator over type `OtelLogEvent`.
    OtelLogs(vec::IntoIter<OtelLogEvent>),
    /// An iterator over type `OtelMetricEvent`.
    OtelMetrics(vec::IntoIter<OtelMetricEvent>),
    /// An iterator over type `OtelSpanEvent`.
    OtelSpans(vec::IntoIter<OtelSpanEvent>),
}

impl Iterator for EventArrayIntoIter {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Logs(i) => i.next().map(Into::into),
            Self::Metrics(i) => i.next().map(Into::into),
            Self::Traces(i) => i.next().map(Event::Trace),
            Self::OtelLogs(i) => i.next().map(Event::OtelLog),
            Self::OtelMetrics(i) => i.next().map(Event::OtelMetric),
            Self::OtelSpans(i) => i.next().map(Event::OtelSpan),
        }
    }
}

struct TypedArrayIterMut<'a, T>(Option<slice::IterMut<'a, T>>);

impl<'a, T> Iterator for TypedArrayIterMut<'a, T> {
    type Item = &'a mut T;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.as_mut().and_then(Iterator::next)
    }
}

/// Intermediate buffer for conversion of a sequence of individual
/// `Event`s into a sequence of `EventArray`s by coalescing contiguous
/// events of the same type into one array. This is used by
/// `events_into_array`.
#[derive(Debug, Default)]
pub struct EventArrayBuffer {
    buffer: Option<EventArray>,
    max_size: usize,
}

impl EventArrayBuffer {
    fn new(max_size: Option<usize>) -> Self {
        let max_size = max_size.unwrap_or(usize::MAX);
        let buffer = None;
        Self { buffer, max_size }
    }

    #[must_use]
    fn push(&mut self, event: Event) -> Option<EventArray> {
        match (event, &mut self.buffer) {
            (Event::Log(event), Some(EventArray::Logs(array))) if array.len() < self.max_size => {
                array.push(event);
                None
            }
            (Event::Metric(event), Some(EventArray::Metrics(array)))
                if array.len() < self.max_size =>
            {
                array.push(event);
                None
            }
            (Event::Trace(event), Some(EventArray::Traces(array)))
                if array.len() < self.max_size =>
            {
                array.push(event);
                None
            }
            (Event::OtelLog(event), Some(EventArray::OtelLogs(array)))
                if array.len() < self.max_size =>
            {
                array.push(event);
                None
            }
            (Event::OtelMetric(event), Some(EventArray::OtelMetrics(array)))
                if array.len() < self.max_size =>
            {
                array.push(event);
                None
            }
            (Event::OtelSpan(event), Some(EventArray::OtelSpans(array)))
                if array.len() < self.max_size =>
            {
                array.push(event);
                None
            }
            (event, current) => current.replace(EventArray::from(event)),
        }
    }

    fn take(&mut self) -> Option<EventArray> {
        self.buffer.take()
    }
}

/// Convert the iterator over individual `Event`s into an iterator
/// over coalesced `EventArray`s.
pub fn events_into_arrays(
    events: impl IntoIterator<Item = Event>,
    max_size: Option<usize>,
) -> impl Iterator<Item = EventArray> {
    IntoEventArraysIter {
        inner: events.into_iter().fuse(),
        current: EventArrayBuffer::new(max_size),
    }
}

/// Iterator type implementing `into_arrays`
pub struct IntoEventArraysIter<I> {
    inner: iter::Fuse<I>,
    current: EventArrayBuffer,
}

impl<I: Iterator<Item = Event>> Iterator for IntoEventArraysIter<I> {
    type Item = EventArray;
    fn next(&mut self) -> Option<Self::Item> {
        for event in self.inner.by_ref() {
            if let Some(array) = self.current.push(event) {
                return Some(array);
            }
        }
        self.current.take()
    }
}
