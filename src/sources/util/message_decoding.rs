use std::iter;

use bytes::BytesMut;
use chrono::{DateTime, Utc};
use tokio_util::codec::Decoder as _;
use vector_lib::{
    EstimatedJsonEncodedSizeOf,
    codecs::StreamDecodingError,
    config::LogNamespace,
    internal_event::{CountByteSize, EventsReceived, InternalEventHandle as _, Registered},
};

use crate::event::{BatchNotifier, Event};
use vector_lib::codecs::Decoder;

pub fn decode_message<'a>(
    mut decoder: Decoder,
    source_type: &'static str,
    message: &[u8],
    timestamp: Option<DateTime<Utc>>,
    batch: &'a Option<BatchNotifier>,
    _log_namespace: LogNamespace,
    events_received: &'a Registered<EventsReceived>,
) -> impl Iterator<Item = Event> + 'a + use<'a> {
    let mut buffer = BytesMut::with_capacity(message.len());
    buffer.extend_from_slice(message);
    let now = Utc::now();

    iter::from_fn(move || {
        loop {
            break match decoder.decode_eof(&mut buffer) {
                Ok(Some((events, _))) => Some(events.into_iter().map(move |mut event| {
                    if let Event::Log(ref mut otel_log) = event {
                            otel_log.set_source_metadata_vector_ns(source_type, now);
                        if let Some(timestamp) = timestamp {
                            otel_log.record_mut().time_unix_nano =
                                timestamp.timestamp_nanos_opt().unwrap_or(0) as u64;
                                otel_log.metadata_mut().value_mut().insert(
                                    vector_lib::lookup::path!(source_type, "timestamp"),
                                    vrl::value::Value::Timestamp(timestamp),
                                );
                        }
                    }
                    events_received.emit(CountByteSize(1, event.estimated_json_encoded_size_of()));
                    event
                })),
                Err(error) => {
                    if error.can_continue() {
                        continue;
                    }
                    None
                }
                Ok(None) => None,
            };
        }
    })
    .flatten()
    .map(move |event| event.with_batch_notifier_option(batch))
}
