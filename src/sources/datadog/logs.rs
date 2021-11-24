use crate::{
    codecs,
    config::log_schema,
    event::Event,
    internal_events::EventsReceived,
    sources::datadog::agent::{decode, handle_request, ApiKeyExtractor, ApiKeyQueryParams},
    sources::util::{ErrorMessage, TcpError},
    vector_core::ByteSizeOf,
    Pipeline,
};
use bytes::{BufMut, Bytes, BytesMut};
use chrono::Utc;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::codec::Decoder;
use warp::{filters::BoxedFilter, path, path::FullPath, reply::Response, Filter};

pub(crate) fn build_warp_filter(
    acknowledgements: bool,
    out: Pipeline,
    api_key_extractor: ApiKeyExtractor,
    decoder: codecs::Decoder,
) -> BoxedFilter<(Response,)> {
    warp::post()
        .and(path!("v1" / "input" / ..).or(path!("api" / "v2" / "logs" / ..)))
        .and(warp::path::full())
        .and(warp::header::optional::<String>("content-encoding"))
        .and(warp::header::optional::<String>("dd-api-key"))
        .and(warp::query::<ApiKeyQueryParams>())
        .and(warp::body::bytes())
        .and_then(
            move |_,
                  path: FullPath,
                  encoding_header: Option<String>,
                  api_token: Option<String>,
                  query_params: ApiKeyQueryParams,
                  body: Bytes| {
                let events = decode(&encoding_header, body).and_then(|body| {
                    decode_log_body(
                        body,
                        api_key_extractor.extract(
                            path.as_str(),
                            api_token,
                            query_params.dd_api_key,
                        ),
                        decoder.clone(),
                    )
                });
                handle_request(events, acknowledgements, out.clone())
            },
        )
        .boxed()
}

pub(crate) fn decode_log_body(
    body: Bytes,
    api_key: Option<Arc<str>>,
    decoder: codecs::Decoder,
) -> Result<Vec<Event>, ErrorMessage> {
    if body.is_empty() {
        // The datadog agent may send an empty payload as a keep alive
        debug!(
            message = "Empty payload ignored.",
            internal_log_rate_secs = 30
        );
        return Ok(Vec::new());
    }

    let messages: Vec<LogMsg> = serde_json::from_slice(&body).map_err(|error| {
        ErrorMessage::new(
            StatusCode::BAD_REQUEST,
            format!("Error parsing JSON: {:?}", error),
        )
    })?;

    let now = Utc::now();
    let mut decoded = Vec::new();

    for message in messages {
        let mut decoder = decoder.clone();
        let mut buffer = BytesMut::new();
        buffer.put(message.message);
        loop {
            match decoder.decode_eof(&mut buffer) {
                Ok(Some((events, _byte_size))) => {
                    for mut event in events {
                        if let Event::Log(ref mut log) = event {
                            log.try_insert_flat("status", message.status.clone());
                            log.try_insert_flat("timestamp", message.timestamp);
                            log.try_insert_flat("hostname", message.hostname.clone());
                            log.try_insert_flat("service", message.service.clone());
                            log.try_insert_flat("ddsource", message.ddsource.clone());
                            log.try_insert_flat("ddtags", message.ddtags.clone());
                            log.try_insert_flat(
                                log_schema().source_type_key(),
                                Bytes::from("datadog_agent"),
                            );
                            log.try_insert_flat(log_schema().timestamp_key(), now);
                            if let Some(k) = &api_key {
                                log.metadata_mut().set_datadog_api_key(Some(Arc::clone(k)));
                            }
                        }

                        decoded.push(event);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    // Error is logged by `crate::codecs::Decoder`, no further
                    // handling is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }

    emit!(&EventsReceived {
        byte_size: decoded.size_of(),
        count: decoded.len(),
    });

    Ok(decoded)
}

// https://github.com/DataDog/datadog-agent/blob/a33248c2bc125920a9577af1e16f12298875a4ad/pkg/logs/processor/json.go#L23-L49
#[derive(Deserialize, Clone, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogMsg {
    pub message: Bytes,
    pub status: Bytes,
    pub timestamp: i64,
    pub hostname: Bytes,
    pub service: Bytes,
    pub ddsource: Bytes,
    pub ddtags: Bytes,
}
