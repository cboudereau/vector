use crate::{
    codecs::{self, DecodingConfig, FramingConfig, ParserConfig},
    config::{
        AcknowledgementsConfig, DataType, GenerateConfig, Resource, SourceConfig, SourceContext,
    },
    event::Event,
    internal_events::HttpDecompressError,
    serde::{bool_or_struct, default_decoding, default_framing_message_based},
    sources::{
        self,
        datadog::logs::LogsAgent,
        util::ErrorMessage,
    },
    tls::{MaybeTlsSettings, TlsConfig},
    Pipeline,
};

use bytes::{Buf, Bytes};
use dyn_clone::DynClone;
use flate2::read::{MultiGzDecoder, ZlibDecoder};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use http::StatusCode;
use regex::Regex;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::{fmt::Debug, io::Read, net::SocketAddr, sync::Arc};
use vector_core::event::{BatchNotifier, BatchStatus};
use warp::{filters::BoxedFilter, reject::Rejection, reply::Response, Filter, Reply};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DatadogAgentConfig {
    address: SocketAddr,
    tls: Option<TlsConfig>,
    #[serde(default = "crate::serde::default_true")]
    store_api_key: bool,

    // Those are probably useless as we know input encoding
    #[serde(default = "default_framing_message_based")]
    framing: Box<dyn FramingConfig>,
    #[serde(default = "default_decoding")]
    decoding: Box<dyn ParserConfig>,
    #[serde(default, deserialize_with = "bool_or_struct")]
    acknowledgements: AcknowledgementsConfig,
    #[serde(flatten)]
    agent: Box<dyn AgentKind>,
    #[serde(default = "crate::serde::default_true")]
    accept_logs: bool,
    #[serde(default = "crate::serde::default_true")]
    accept_traces: bool,
}

impl GenerateConfig for DatadogAgentConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            address: "0.0.0.0:8080".parse().unwrap(),
            tls: None,
            store_api_key: true,
            framing: default_framing_message_based(),
            decoding: default_decoding(),
            acknowledgements: AcknowledgementsConfig::default(),
            accept_logs: true,
            accept_traces: true,
            agent: Box::new(LogsAgent {}),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_agent")]
impl SourceConfig for DatadogAgentConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<sources::Source> {
        let decoder = DecodingConfig::new(self.framing.clone(), self.decoding.clone()).build()?;
        let source = DatadogAgentSource::new(
            self.acknowledgements.enabled,
            cx.out.clone(),
            self.store_api_key,
            decoder.clone(),
        );

        let tls = MaybeTlsSettings::from_config(&self.tls, true)?;
        let listener = tls.bind(&self.address).await?;

        let filters = self.agent.build_warp_filter(
            self.acknowledgements.enabled,
            cx.out,
            source.api_key_extractor,
            decoder,
        );

        let shutdown = cx.shutdown;
        Ok(Box::pin(async move {
            let span = crate::trace::current_span();
            let routes = filters
                .with(warp::trace(move |_info| span.clone()))
                .recover(|r: Rejection| async move {
                    if let Some(e_msg) = r.find::<ErrorMessage>() {
                        let json = warp::reply::json(e_msg);
                        Ok(warp::reply::with_status(json, e_msg.status_code()))
                    } else {
                        // other internal error - will return 500 internal server error
                        Err(r)
                    }
                });
            warp::serve(routes)
                .serve_incoming_with_graceful_shutdown(
                    listener.accept_stream(),
                    shutdown.map(|_| ()),
                )
                .await;

            Ok(())
        }))
    }

    fn output_type(&self) -> DataType {
        DataType::Any
    }

    fn source_type(&self) -> &'static str {
        "datadog_agent"
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::tcp(self.address)]
    }
}

#[derive(Clone, Copy, Debug, Snafu)]
pub(crate) enum ApiError {
    BadRequest,
    InvalidDataFormat,
    ServerShutdown,
}

#[typetag::serde(tag = "agent")]
pub(crate) trait AgentKind: DynClone + Debug + Send + Sync {
    fn build_warp_filter(
        &self,
        acknowledgements: bool,
        out: Pipeline,
        api_key_extractor: ApiKeyExtractor,
        decoder: codecs::Decoder,
    ) -> BoxedFilter<(Response,)>;
}

dyn_clone::clone_trait_object!(AgentKind);

impl warp::reject::Reject for ApiError {}

#[derive(Deserialize)]
pub struct ApiKeyQueryParams {
    #[serde(rename = "dd-api-key")]
    pub dd_api_key: Option<String>,
}

#[derive(Clone)]
pub(crate) struct DatadogAgentSource {
    acknowledgements: bool,
    pub(crate) api_key_extractor: ApiKeyExtractor,
    decoder: codecs::Decoder,
    out: Pipeline,
}

#[derive(Clone)]
pub struct ApiKeyExtractor {
    matcher: Regex,
    store_api_key: bool,
}

impl ApiKeyExtractor {
    pub fn extract(
        &self,
        path: &str,
        header: Option<String>,
        query_params: Option<String>,
    ) -> Option<Arc<str>> {
        if !self.store_api_key {
            return None;
        }
        // Grab from URL first
        self.matcher
            .captures(path)
            .and_then(|cap| cap.name("api_key").map(|key| key.as_str()).map(Arc::from))
            // Try from query params
            .or_else(|| query_params.map(Arc::from))
            // Try from header next
            .or_else(|| header.map(Arc::from))
    }
}

impl DatadogAgentSource {
    pub(crate) fn new(
        acknowledgements: bool,
        out: Pipeline,
        store_api_key: bool,
        decoder: codecs::Decoder,
    ) -> Self {
        Self {
            acknowledgements,
            api_key_extractor: ApiKeyExtractor {
                store_api_key,
                matcher: Regex::new(r"^/v1/input/(?P<api_key>[[:alnum:]]{32})/??")
                    .expect("static regex always compiles"),
            },
            decoder,
            out,
        }
    }
}

pub(crate) async fn handle_request(
    events: Result<Vec<Event>, ErrorMessage>,
    acknowledgements: bool,
    mut out: Pipeline,
) -> Result<Response, Rejection> {
    match events {
        Ok(mut events) => {
            let receiver = BatchNotifier::maybe_apply_to_events(acknowledgements, &mut events);

            let mut events = futures::stream::iter(events).map(Ok);
            out.send_all(&mut events)
                .map_err(move |error: crate::pipeline::ClosedError| {
                    // can only fail if receiving end disconnected, so we are shutting down,
                    // probably not gracefully.
                    error!(message = "Failed to forward events, downstream is closed.");
                    error!(message = "Tried to send the following event.", %error);
                    warp::reject::custom(ApiError::ServerShutdown)
                })
                .await?;
            match receiver {
                None => Ok(warp::reply().into_response()),
                Some(receiver) => match receiver.await {
                    BatchStatus::Delivered => Ok(warp::reply().into_response()),
                    BatchStatus::Errored => Err(warp::reject::custom(ErrorMessage::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Error delivering contents to sink".into(),
                    ))),
                    BatchStatus::Failed => Err(warp::reject::custom(ErrorMessage::new(
                        StatusCode::BAD_REQUEST,
                        "Contents failed to deliver to sink".into(),
                    ))),
                },
            }
        }
        Err(err) => Err(warp::reject::custom(err)),
    }
}

pub(crate) fn decode(header: &Option<String>, mut body: Bytes) -> Result<Bytes, ErrorMessage> {
    if let Some(encodings) = header {
        for encoding in encodings.rsplit(',').map(str::trim) {
            body = match encoding {
                "identity" => body,
                "gzip" | "x-gzip" => {
                    let mut decoded = Vec::new();
                    MultiGzDecoder::new(body.reader())
                        .read_to_end(&mut decoded)
                        .map_err(|error| handle_decode_error(encoding, error))?;
                    decoded.into()
                }
                "deflate" | "x-deflate" => {
                    let mut decoded = Vec::new();
                    ZlibDecoder::new(body.reader())
                        .read_to_end(&mut decoded)
                        .map_err(|error| handle_decode_error(encoding, error))?;
                    decoded.into()
                }
                encoding => {
                    return Err(ErrorMessage::new(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        format!("Unsupported encoding {}", encoding),
                    ))
                }
            }
        }
    }
    //emit!(&DatadogAgentRequestReceived {
    //    byte_size: body.len(),
    //    count: 1,
    //}); => to be dropped and replaced by
    // emit!(&HttpBytesReceived {
    //    byte_size: body.len(),
    //    http_path: path.as_str(),
    //   protocol: self.protocol,
    // });
    Ok(body)
}

fn handle_decode_error(encoding: &str, error: impl std::error::Error) -> ErrorMessage {
    emit!(&HttpDecompressError {
        encoding,
        error: &error
    });
    ErrorMessage::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("Failed decompressing payload with {} decoder.", encoding),
    )
}

// https://github.com/DataDog/datadog-agent/blob/a33248c2bc125920a9577af1e16f12298875a4ad/pkg/logs/processor/json.go#L23-L49
#[derive(Deserialize, Clone, Serialize, Debug)]
#[serde(deny_unknown_fields)]
struct LogMsg {
    pub message: Bytes,
    pub status: Bytes,
    pub timestamp: i64,
    pub hostname: Bytes,
    pub service: Bytes,
    pub ddsource: Bytes,
    pub ddtags: Bytes,
}
