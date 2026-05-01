use std::{num::NonZeroUsize, task::{Context, Poll}};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, future::BoxFuture, stream::BoxStream};
use http::{header, Method, Request};
use hyper::Body;
use tower::{Service, ServiceBuilder};
use vector_lib::{
    ByteSizeOf, EstimatedJsonEncodedSizeOf,
    config::telemetry,
    configurable::configurable_component,
    request_metadata::{GroupedCountByteSize, MetaDescriptive, RequestMetadata},
    stream::{BatcherSettings, DriverResponse, batcher::data::BatchReduce},
};

use snafu::Snafu;

use crate::{
    config::{AcknowledgementsConfig, GenerateConfig, Input, SinkContext},
    event::{Event, EventFinalizers, EventStatus, Finalizable},
    http::HttpClient,
    internal_events::EndpointBytesSent,
    sinks::{
        Healthcheck, VectorSink,
        util::{
            BatchConfig, RealtimeEventBasedDefaultBatchSettings, ServiceBuilderExt,
            SinkBuilderExt, StreamSink, TowerRequestConfig, metadata::RequestMetadataBuilder,
            retries::RetryLogic,
        },
    },
    tls::TlsSettings,
};

use super::grpc::{
    otel_log_event_to_resource_logs, otel_metric_event_to_resource_metrics,
    otel_span_event_to_resource_spans,
};

#[derive(Debug, Snafu)]
pub enum OtlpHttpError {
    #[snafu(display("HTTP request failed: {message}"))]
    HttpRequest { message: String },
}

/// Configuration for the `opentelemetry` sink's OTLP/HTTP JSON transport.
#[configurable_component]
#[derive(Clone, Debug)]
pub struct OtlpHttpConfig {
    /// The base OTLP HTTP endpoint.
    ///
    /// Signal-specific paths (`/v1/logs`, `/v1/metrics`, `/v1/traces`) are
    /// appended automatically.
    #[configurable(metadata(docs::examples = "http://localhost:4318"))]
    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    #[configurable(derived)]
    #[serde(default)]
    pub batch: BatchConfig<RealtimeEventBasedDefaultBatchSettings>,

    #[configurable(derived)]
    #[serde(default)]
    pub request: TowerRequestConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub tls: Option<vector_lib::tls::TlsConfig>,

    #[configurable(derived)]
    #[serde(
        default,
        deserialize_with = "crate::serde::bool_or_struct",
        skip_serializing_if = "crate::serde::is_default"
    )]
    pub acknowledgements: AcknowledgementsConfig,
}

impl Default for OtlpHttpConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            batch: Default::default(),
            request: Default::default(),
            tls: Default::default(),
            acknowledgements: Default::default(),
        }
    }
}

fn default_endpoint() -> String {
    "http://localhost:4318".to_string()
}

impl GenerateConfig for OtlpHttpConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(r#"endpoint = "http://localhost:4318""#).unwrap()
    }
}

impl OtlpHttpConfig {
    pub async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        let tls = TlsSettings::from_options(self.tls.as_ref())?;
        let client = HttpClient::new(tls, cx.proxy())?;

        let service = OtlpHttpService {
            client: client.clone(),
            endpoint: self.endpoint.trim_end_matches('/').to_string(),
        };

        let batch = self
            .batch
            .into_batcher_settings()
            .map_err(|e| format!("invalid batch settings: {e}"))?;

        let tower_svc = ServiceBuilder::new()
            .settings(self.request.into_settings(), OtlpHttpRetryLogic)
            .service(service);

        let sink = OtlpHttpSink {
            batch_settings: batch,
            service: tower_svc,
        };

        let healthcheck: Healthcheck = Box::pin(async { Ok(()) });
        Ok((VectorSink::from_event_streamsink(sink), healthcheck))
    }

    pub fn input(&self) -> Input {
        Input::all()
    }

    pub fn acknowledgements(&self) -> &AcknowledgementsConfig {
        &self.acknowledgements
    }
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OtlpHttpRequest {
    pub body: Bytes,
    pub signal_path: &'static str,
    pub finalizers: EventFinalizers,
    pub metadata: RequestMetadata,
}

impl Finalizable for OtlpHttpRequest {
    fn take_finalizers(&mut self) -> EventFinalizers {
        self.finalizers.take_finalizers()
    }
}

impl MetaDescriptive for OtlpHttpRequest {
    fn get_metadata(&self) -> &RequestMetadata {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut RequestMetadata {
        &mut self.metadata
    }
}

pub struct OtlpHttpResponse {
    events_byte_size: GroupedCountByteSize,
}

impl DriverResponse for OtlpHttpResponse {
    fn event_status(&self) -> EventStatus {
        EventStatus::Delivered
    }

    fn events_sent(&self) -> &GroupedCountByteSize {
        &self.events_byte_size
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct OtlpHttpService {
    client: HttpClient,
    endpoint: String,
}

impl Service<OtlpHttpRequest> for OtlpHttpService {
    type Response = OtlpHttpResponse;
    type Error = OtlpHttpError;
    type Future = BoxFuture<'static, Result<OtlpHttpResponse, OtlpHttpError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), OtlpHttpError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: OtlpHttpRequest) -> Self::Future {
        let client = self.client.clone();
        let uri = format!("{}{}", self.endpoint, req.signal_path);
        let metadata = std::mem::take(req.metadata_mut());
        let events_byte_size = metadata.into_events_estimated_json_encoded_byte_size();
        let byte_size = req.body.len();
        let endpoint = self.endpoint.clone();

        Box::pin(async move {
            let http_req = Request::builder()
                .method(Method::POST)
                .uri(&uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req.body))
                .map_err(|e| OtlpHttpError::HttpRequest {
                    message: e.to_string(),
                })?;

            let response = client.send(http_req).await.map_err(|e| {
                OtlpHttpError::HttpRequest {
                    message: e.to_string(),
                }
            })?;

            let status = response.status();
            if !status.is_success() {
                return Err(OtlpHttpError::HttpRequest {
                    message: format!("HTTP {} from {}", status, uri),
                });
            }

            emit!(EndpointBytesSent {
                byte_size,
                protocol: "http",
                endpoint: &endpoint,
            });

            Ok(OtlpHttpResponse { events_byte_size })
        })
    }
}

// ---------------------------------------------------------------------------
// Retry logic
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct OtlpHttpRetryLogic;

impl RetryLogic for OtlpHttpRetryLogic {
    type Error = OtlpHttpError;
    type Request = OtlpHttpRequest;
    type Response = OtlpHttpResponse;

    fn is_retriable_error(&self, _error: &Self::Error) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

struct OtlpHttpSink<S> {
    batch_settings: BatcherSettings,
    service: S,
}

#[derive(Clone)]
struct EventCollection {
    finalizers: EventFinalizers,
    events: Vec<Event>,
    byte_size: usize,
    json_byte_size: GroupedCountByteSize,
}

impl Default for EventCollection {
    fn default() -> Self {
        Self {
            finalizers: Default::default(),
            events: Default::default(),
            byte_size: Default::default(),
            json_byte_size: telemetry().create_request_count_byte_size(),
        }
    }
}

impl<S> OtlpHttpSink<S>
where
    S: Service<OtlpHttpRequest, Response = OtlpHttpResponse> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Debug + Into<crate::Error> + Send,
{
    async fn run_inner(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        input
            .map(|mut event| {
                let byte_size = event.size_of();
                let mut json_byte_size = telemetry().create_request_count_byte_size();
                json_byte_size.add_event(&event, event.estimated_json_encoded_size_of());
                let finalizers = event.take_finalizers();
                (event, finalizers, byte_size, json_byte_size)
            })
            .batched(self.batch_settings.as_reducer_config(
                |(event, _, _, _): &(Event, _, _, _)| event.size_of().max(1),
                BatchReduce::new(
                    |col: &mut EventCollection,
                     (event, finalizers, byte_size, json_size)| {
                        col.finalizers.merge(finalizers);
                        col.events.push(event);
                        col.byte_size += byte_size;
                        col.json_byte_size += json_size;
                    },
                ),
            ))
            .flat_map(|col| futures::stream::iter(collection_into_http_requests(col)))
            .into_driver(self.service)
            .run()
            .await
    }
}

#[async_trait]
impl<S> StreamSink<Event> for OtlpHttpSink<S>
where
    S: Service<OtlpHttpRequest, Response = OtlpHttpResponse> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Debug + Into<crate::Error> + Send,
{
    async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.run_inner(input).await
    }
}

// ---------------------------------------------------------------------------
// Event → OtlpHttpRequest conversion
// ---------------------------------------------------------------------------

/// Proto-encode a local type, decode as upstream type (which has serde).
fn proto_roundtrip<L: prost::Message, U: prost::Message + Default>(local: &L) -> U {
    let bytes = local.encode_to_vec();
    U::decode(::bytes::Bytes::from(bytes)).expect("proto roundtrip decode")
}

fn collection_into_http_requests(col: EventCollection) -> Vec<OtlpHttpRequest> {
    use opentelemetry_proto::tonic::collector::{
        logs::v1::ExportLogsServiceRequest as UpstreamLogsReq,
        metrics::v1::ExportMetricsServiceRequest as UpstreamMetricsReq,
        trace::v1::ExportTraceServiceRequest as UpstreamTracesReq,
    };
    use vector_lib::opentelemetry::proto::{
        logs::v1::ResourceLogs,
        metrics::v1::ResourceMetrics,
        trace::v1::ResourceSpans,
    };

    let n = col.events.len();
    let mut log_resources: Vec<ResourceLogs> = vec![];
    let mut metric_resources: Vec<ResourceMetrics> = vec![];
    let mut trace_resources: Vec<ResourceSpans> = vec![];

    for event in &col.events {
        match event {
            Event::Log(log_event) => {
                log_resources.push(otel_log_event_to_resource_logs(log_event));
            }
            Event::Metric(metric_event) => {
                metric_resources.push(otel_metric_event_to_resource_metrics(metric_event));
            }
            Event::Trace(span_event) => {
                trace_resources.push(otel_span_event_to_resource_spans(span_event));
            }
        }
    }

    let mut requests = Vec::new();
    let builder = RequestMetadataBuilder::new(n, col.byte_size, col.json_byte_size);

    if !log_resources.is_empty() {
        let upstream: UpstreamLogsReq = UpstreamLogsReq {
            resource_logs: log_resources.iter().map(|r| proto_roundtrip(r)).collect(),
        };
        let body = Bytes::from(serde_json::to_vec(&upstream).expect("OTLP JSON serialization"));
        let bytes_len = NonZeroUsize::new(body.len().max(1)).unwrap();
        requests.push(OtlpHttpRequest {
            body,
            signal_path: "/v1/logs",
            finalizers: col.finalizers.clone(),
            metadata: builder.with_request_size(bytes_len),
        });
    }

    if !metric_resources.is_empty() {
        let upstream: UpstreamMetricsReq = UpstreamMetricsReq {
            resource_metrics: metric_resources.iter().map(|r| proto_roundtrip(r)).collect(),
        };
        let body = Bytes::from(serde_json::to_vec(&upstream).expect("OTLP JSON serialization"));
        let bytes_len = NonZeroUsize::new(body.len().max(1)).unwrap();
        requests.push(OtlpHttpRequest {
            body,
            signal_path: "/v1/metrics",
            finalizers: col.finalizers.clone(),
            metadata: builder.with_request_size(bytes_len),
        });
    }

    if !trace_resources.is_empty() {
        let upstream: UpstreamTracesReq = UpstreamTracesReq {
            resource_spans: trace_resources.iter().map(|r| proto_roundtrip(r)).collect(),
        };
        let body = Bytes::from(serde_json::to_vec(&upstream).expect("OTLP JSON serialization"));
        let bytes_len = NonZeroUsize::new(body.len().max(1)).unwrap();
        requests.push(OtlpHttpRequest {
            body,
            signal_path: "/v1/traces",
            finalizers: col.finalizers.clone(),
            metadata: builder.with_request_size(bytes_len),
        });
    }

    requests
}
