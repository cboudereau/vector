use std::{num::NonZeroUsize, task::{Context, Poll}};

use async_trait::async_trait;
use futures::{StreamExt, TryFutureExt, future::BoxFuture, stream::BoxStream};
use http::Uri;
use prost::Message as _;
use tonic::{IntoRequest, transport::Channel};
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
    internal_events::EndpointBytesSent,
    sinks::{
        Healthcheck, VectorSink,
        util::{
            BatchConfig, RealtimeEventBasedDefaultBatchSettings, ServiceBuilderExt,
            SinkBuilderExt, StreamSink, TowerRequestConfig, metadata::RequestMetadataBuilder,
            retries::RetryLogic,
        },
    },
    tls::TlsEnableableConfig,
};

#[derive(Debug, Snafu)]
pub enum OtlpGrpcError {
    #[snafu(display("gRPC request failed: {source}"))]
    GrpcRequest { source: tonic::Status },
}

use vector_lib::opentelemetry::proto::collector::{
    logs::v1::{ExportLogsServiceRequest, logs_service_client::LogsServiceClient},
    metrics::v1::{ExportMetricsServiceRequest, metrics_service_client::MetricsServiceClient},
    trace::v1::{ExportTraceServiceRequest, trace_service_client::TraceServiceClient},
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the `opentelemetry` sink's gRPC transport.
#[configurable_component]
#[derive(Clone, Debug)]
pub struct GrpcConfig {
    /// The OTLP gRPC endpoint.
    ///
    /// Must include scheme and port, e.g. `http://localhost:4317`.
    #[configurable(metadata(docs::examples = "http://localhost:4317"))]
    pub endpoint: String,

    /// Compress outgoing gRPC payloads with gzip.
    #[serde(default)]
    pub compression: bool,

    #[configurable(derived)]
    #[serde(default)]
    pub batch: BatchConfig<RealtimeEventBasedDefaultBatchSettings>,

    #[configurable(derived)]
    #[serde(default)]
    pub request: TowerRequestConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub tls: Option<TlsEnableableConfig>,

    #[configurable(derived)]
    #[serde(
        default,
        deserialize_with = "crate::serde::bool_or_struct",
        skip_serializing_if = "crate::serde::is_default"
    )]
    pub acknowledgements: AcknowledgementsConfig,
}

impl GenerateConfig for GrpcConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(r#"endpoint = "http://localhost:4317""#).unwrap()
    }
}

impl GrpcConfig {
    pub async fn build(
        &self,
        _cx: SinkContext,
    ) -> crate::Result<(VectorSink, Healthcheck)> {
        let endpoint: Uri = self
            .endpoint
            .parse()
            .map_err(|e| format!("invalid endpoint URI: {e}"))?;

        let channel = Channel::builder(endpoint).connect_lazy();
        let service = OtlpGrpcService::new(channel, self.endpoint.clone(), self.compression);
        let batch = self
            .batch
            .into_batcher_settings()
            .map_err(|e| format!("invalid batch settings: {e}"))?;

        let tower_svc = ServiceBuilder::new()
            .settings(self.request.into_settings(), OtlpRetryLogic)
            .service(service);

        let sink = OtlpGrpcSink {
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
// Batch accumulator
// ---------------------------------------------------------------------------

/// Events collected during one batch window.
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

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

/// One batch of OTLP signal payloads ready to be sent over gRPC.
#[derive(Clone)]
pub struct OtlpRequest {
    pub logs: Option<ExportLogsServiceRequest>,
    pub metrics: Option<ExportMetricsServiceRequest>,
    pub traces: Option<ExportTraceServiceRequest>,
    pub finalizers: EventFinalizers,
    pub metadata: RequestMetadata,
    pub encoded_bytes: usize,
}

impl Finalizable for OtlpRequest {
    fn take_finalizers(&mut self) -> EventFinalizers {
        self.finalizers.take_finalizers()
    }
}

impl MetaDescriptive for OtlpRequest {
    fn get_metadata(&self) -> &RequestMetadata {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut RequestMetadata {
        &mut self.metadata
    }
}

pub struct OtlpResponse {
    events_byte_size: GroupedCountByteSize,
}

impl DriverResponse for OtlpResponse {
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
pub struct OtlpGrpcService {
    logs_client: LogsServiceClient<Channel>,
    metrics_client: MetricsServiceClient<Channel>,
    traces_client: TraceServiceClient<Channel>,
    protocol: String,
    endpoint: String,
}

impl OtlpGrpcService {
    pub fn new(channel: Channel, endpoint: String, compression: bool) -> Self {
        let mut logs = LogsServiceClient::new(channel.clone());
        let mut metrics = MetricsServiceClient::new(channel.clone());
        let mut traces = TraceServiceClient::new(channel);
        if compression {
            logs = logs.send_compressed(tonic::codec::CompressionEncoding::Gzip);
            metrics = metrics.send_compressed(tonic::codec::CompressionEncoding::Gzip);
            traces = traces.send_compressed(tonic::codec::CompressionEncoding::Gzip);
        }
        let protocol = endpoint
            .split("://")
            .next()
            .unwrap_or("http")
            .to_string();
        Self {
            logs_client: logs,
            metrics_client: metrics,
            traces_client: traces,
            protocol,
            endpoint,
        }
    }
}

impl Service<OtlpRequest> for OtlpGrpcService {
    type Response = OtlpResponse;
    type Error = OtlpGrpcError;
    type Future = BoxFuture<'static, Result<OtlpResponse, OtlpGrpcError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), OtlpGrpcError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: OtlpRequest) -> Self::Future {
        let mut svc = self.clone();
        let byte_size = req.encoded_bytes;
        let metadata = std::mem::take(req.metadata_mut());
        let events_byte_size = metadata.into_events_estimated_json_encoded_byte_size();

        Box::pin(async move {
            if let Some(logs_req) = req.logs {
                svc.logs_client
                    .export(logs_req.into_request())
                    .map_err(|source| OtlpGrpcError::GrpcRequest { source })
                    .await?;
            }
            if let Some(metrics_req) = req.metrics {
                svc.metrics_client
                    .export(metrics_req.into_request())
                    .map_err(|source| OtlpGrpcError::GrpcRequest { source })
                    .await?;
            }
            if let Some(traces_req) = req.traces {
                svc.traces_client
                    .export(traces_req.into_request())
                    .map_err(|source| OtlpGrpcError::GrpcRequest { source })
                    .await?;
            }

            emit!(EndpointBytesSent {
                byte_size,
                protocol: &svc.protocol,
                endpoint: &svc.endpoint,
            });

            Ok(OtlpResponse { events_byte_size })
        })
    }
}

// ---------------------------------------------------------------------------
// Retry logic
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct OtlpRetryLogic;

impl RetryLogic for OtlpRetryLogic {
    type Error = OtlpGrpcError;
    type Request = OtlpRequest;
    type Response = OtlpResponse;

    fn is_retriable_error(&self, _error: &Self::Error) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

struct OtlpGrpcSink<S> {
    batch_settings: BatcherSettings,
    service: S,
}

impl<S> OtlpGrpcSink<S>
where
    S: Service<OtlpRequest, Response = OtlpResponse> + Send + 'static,
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
            .map(|col| collection_into_request(col))
            .into_driver(self.service)
            .run()
            .await
    }
}

#[async_trait]
impl<S> StreamSink<Event> for OtlpGrpcSink<S>
where
    S: Service<OtlpRequest, Response = OtlpResponse> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: std::fmt::Debug + Into<crate::Error> + Send,
{
    async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.run_inner(input).await
    }
}

// ---------------------------------------------------------------------------
// Event → OtlpRequest conversion
// ---------------------------------------------------------------------------

fn collection_into_request(col: EventCollection) -> OtlpRequest {
    use vector_lib::opentelemetry::{
        metrics::metric_to_export_request,
        proto::{
            collector::{
                logs::v1::ExportLogsServiceRequest,
                metrics::v1::ExportMetricsServiceRequest,
                trace::v1::ExportTraceServiceRequest,
            },
            common::v1::{AnyValue, any_value},
            logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
            trace::v1::{ResourceSpans, ScopeSpans, Span},
        },
    };
    use vrl::{event_path, value::Value};

    use vector_lib::opentelemetry::proto::metrics::v1::ResourceMetrics;

    let n = col.events.len();
    let mut log_resources: Vec<ResourceLogs> = vec![];
    let mut metric_resources: Vec<ResourceMetrics> = vec![];
    let mut trace_resources: Vec<ResourceSpans> = vec![];

    for event in col.events {
        match event {
            Event::Log(log) => {
                let body = match log.value().clone() {
                    Value::Bytes(b) => any_value::Value::StringValue(
                        String::from_utf8_lossy(&b).into_owned(),
                    ),
                    other => any_value::Value::StringValue(format!("{other:?}")),
                };
                log_resources.push(ResourceLogs {
                    resource: None,
                    scope_logs: vec![ScopeLogs {
                        scope: None,
                        log_records: vec![LogRecord {
                            time_unix_nano: 0,
                            observed_time_unix_nano: 0,
                            severity_number: 0,
                            severity_text: String::new(),
                            body: Some(AnyValue { value: Some(body) }),
                            attributes: vec![],
                            dropped_attributes_count: 0,
                            flags: 0,
                            trace_id: vec![],
                            span_id: vec![],
                        }],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                });
            }
            Event::Metric(ref m) => {
                let req = metric_to_export_request(&m);
                metric_resources.extend(req.resource_metrics);
            }
            Event::Trace(trace) => {
                let name = trace
                    .get(event_path!("name"))
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();

                trace_resources.push(ResourceSpans {
                    resource: None,
                    scope_spans: vec![ScopeSpans {
                        scope: None,
                        spans: vec![Span {
                            trace_id: vec![],
                            span_id: vec![],
                            trace_state: String::new(),
                            parent_span_id: vec![],
                            name,
                            kind: 0,
                            start_time_unix_nano: 0,
                            end_time_unix_nano: 0,
                            attributes: vec![],
                            dropped_attributes_count: 0,
                            events: vec![],
                            dropped_events_count: 0,
                            links: vec![],
                            dropped_links_count: 0,
                            status: None,
                        }],
                        schema_url: String::new(),
                    }],
                    schema_url: String::new(),
                });
            }
        }
    }

    let logs = if log_resources.is_empty() {
        None
    } else {
        Some(ExportLogsServiceRequest {
            resource_logs: log_resources,
        })
    };
    let metrics = if metric_resources.is_empty() {
        None
    } else {
        Some(ExportMetricsServiceRequest {
            resource_metrics: metric_resources,
        })
    };
    let traces = if trace_resources.is_empty() {
        None
    } else {
        Some(ExportTraceServiceRequest {
            resource_spans: trace_resources,
        })
    };

    let encoded_bytes = logs.as_ref().map_or(0, |r| r.encoded_len())
        + metrics.as_ref().map_or(0, |r| r.encoded_len())
        + traces.as_ref().map_or(0, |r| r.encoded_len());

    let bytes_len = NonZeroUsize::new(encoded_bytes.max(1)).unwrap();
    let builder = RequestMetadataBuilder::new(n, col.byte_size, col.json_byte_size);

    OtlpRequest {
        logs,
        metrics,
        traces,
        finalizers: col.finalizers,
        metadata: builder.with_request_size(bytes_len),
        encoded_bytes,
    }
}
