mod generic;

use async_graphql::Interface;

use super::{ReceivedEventsTotal, SentBytesTotal, SentEventsTotal};
use crate::event::OtelMetric;

#[derive(Debug, Clone, Interface)]
#[graphql(
    field(name = "received_events_total", ty = "Option<ReceivedEventsTotal>"),
    field(name = "sent_bytes_total", ty = "Option<SentBytesTotal>"),
    field(name = "sent_events_total", ty = "Option<SentEventsTotal>")
)]
pub enum SinkMetrics {
    GenericSinkMetrics(generic::GenericSinkMetrics),
}

pub trait IntoSinkMetrics {
    fn into_sink_metrics(self, component_type: &str) -> SinkMetrics;
}

impl IntoSinkMetrics for Vec<OtelMetric> {
    fn into_sink_metrics(self, _component_type: &str) -> SinkMetrics {
        SinkMetrics::GenericSinkMetrics(generic::GenericSinkMetrics::new(self))
    }
}
