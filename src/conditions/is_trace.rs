use crate::event::Event;

pub(crate) const fn check_is_trace(e: Event) -> (bool, Event) {
    (matches!(e, Event::Trace(_) | Event::OtelSpan(_)), e)
}

pub(crate) fn check_is_trace_with_context(e: Event) -> (Result<(), String>, Event) {
    let (result, event) = check_is_trace(e);
    if result {
        (Ok(()), event)
    } else {
        (Err("event is not a trace type".to_string()), event)
    }
}

#[cfg(test)]
mod test {
    use super::check_is_trace;
    use crate::event::{
        Event, LogEvent, OtelSpan, TraceEvent,
        metric::{Metric, MetricKind, MetricValue},
    };
    use opentelemetry_proto::tonic::trace::v1::Span;

    #[test]
    fn is_trace_basic() {
        assert!(
            check_is_trace(Event::from(TraceEvent::from(LogEvent::from(
                "just a trace"
            ))))
            .0
        );
        assert!(
            !check_is_trace(Event::from(Metric::new(
                "test metric",
                MetricKind::Incremental,
                MetricValue::Counter { value: 1.0 },
            )))
            .0,
        );
    }

    #[test]
    fn is_trace_matches_otel_span() {
        let event = Event::OtelSpan(OtelSpan::new(Span {
            name: "my-span".to_string(),
            ..Default::default()
        }));
        assert!(check_is_trace(event).0);
    }
}
