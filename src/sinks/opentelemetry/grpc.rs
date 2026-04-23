use std::{num::NonZeroUsize, task::{Context, Poll}};

use async_trait::async_trait;
use futures::{StreamExt, TryFutureExt, future::BoxFuture, stream::BoxStream};
use http_1::Uri;
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

use super::load_balancing::LoadBalancingConfig;

/// Configuration for the `opentelemetry` sink's gRPC transport.
#[configurable_component]
#[derive(Clone, Debug)]
pub struct GrpcConfig {
    /// The OTLP gRPC endpoint.
    ///
    /// Must include scheme and port, e.g. `http://localhost:4317`.
    /// Ignored when `load_balancing` is set (resolver provides backends).
    #[configurable(metadata(docs::examples = "http://localhost:4317"))]
    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Load-balancing configuration. When set, events are routed to multiple
    /// backends via consistent hashing. The `endpoint` field is ignored —
    /// backends are discovered via the resolver.
    #[configurable(derived)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_balancing: Option<LoadBalancingConfig>,

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

fn default_endpoint() -> String {
    "http://localhost:4317".to_string()
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
        if let Some(lb_config) = &self.load_balancing {
            return self.build_load_balanced(lb_config.clone()).await;
        }
        self.build_single_endpoint().await
    }

    async fn build_single_endpoint(&self) -> crate::Result<(VectorSink, Healthcheck)> {
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

    async fn build_load_balanced(
        &self,
        lb_config: LoadBalancingConfig,
    ) -> crate::Result<(VectorSink, Healthcheck)> {
        use super::load_balancing::start_resolver;

        let compression = self.compression;
        let batch = self
            .batch
            .into_batcher_settings()
            .map_err(|e| format!("invalid batch settings: {e}"))?;

        let (rx, resolver_handle) = start_resolver(lb_config.resolver);

        let sink = LoadBalancedOtlpGrpcSink {
            routing_key: lb_config.routing_key,
            backends_rx: rx,
            resolver_handle,
            compression,
            batch_settings: batch,
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
// Load-balanced sink
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use metrics::{counter, gauge};
use super::load_balancing::{ConsistentHashRing, RoutingKey, extract_routing_key};

/// Parse an endpoint string into a URI, prepending `http://` if no scheme is present.
fn parse_endpoint_uri(ep: &str) -> Option<Uri> {
    ep.parse::<Uri>().ok().or_else(|| {
        format!("http://{ep}").parse::<Uri>().ok()
    })
}

/// A sink that routes events to multiple backends via consistent hashing.
///
/// Mirrors the OTel Collector Contrib `loadbalancingexporter` pattern:
/// one sub-service per backend, consistent hash ring for routing, resolver
/// for dynamic backend discovery.
struct LoadBalancedOtlpGrpcSink {
    routing_key: RoutingKey,
    backends_rx: tokio::sync::watch::Receiver<Vec<String>>,
    resolver_handle: tokio::task::JoinHandle<()>,
    compression: bool,
    batch_settings: BatcherSettings,
}

impl Drop for LoadBalancedOtlpGrpcSink {
    fn drop(&mut self) {
        self.resolver_handle.abort();
    }
}

#[async_trait]
impl StreamSink<Event> for LoadBalancedOtlpGrpcSink {
    async fn run(mut self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        use futures::stream::StreamExt;

        // Wait for the first set of backends from the resolver.
        if self.backends_rx.changed().await.is_err() {
            error!(message = "Resolver channel closed before providing backends.");
            return Err(());
        }

        let mut ring = ConsistentHashRing::new(&self.backends_rx.borrow());
        let mut services: HashMap<String, OtlpGrpcService> = HashMap::new();
        for ep in ring.endpoints() {
            let Some(uri) = parse_endpoint_uri(ep) else {
                warn!(message = "Skipping backend with invalid URI.", endpoint = %ep);
                continue;
            };
            let channel = Channel::builder(uri).connect_lazy();
            services.insert(ep.clone(), OtlpGrpcService::new(channel, ep.clone(), self.compression));
        }
        gauge!("vector_lb_num_backends").set(ring.len() as f64);

        // Collect events, route by key, batch per backend, send.
        let routing_key = self.routing_key.clone();
        let mut input = input.fuse();

        loop {
            // Check for resolver updates (non-blocking).
            if self.backends_rx.has_changed().unwrap_or(false) {
                let new_endpoints = self.backends_rx.borrow_and_update().clone();
                ring = ConsistentHashRing::new(&new_endpoints);
                gauge!("vector_lb_num_backends").set(ring.len() as f64);
                counter!("vector_lb_num_resolutions", "outcome" => "success").increment(1);
                // Add new backends, remove old ones.
                let new_set: std::collections::HashSet<&String> = new_endpoints.iter().collect();
                let old_keys: Vec<String> = services.keys().cloned().collect();
                for key in &old_keys {
                    if !new_set.contains(key) {
                        services.remove(key);
                    }
                }
                for ep in &new_endpoints {
                    if !services.contains_key(ep) {
                        if let Some(uri) = parse_endpoint_uri(ep) {
                            let channel = Channel::builder(uri).connect_lazy();
                            services.insert(
                                ep.clone(),
                                OtlpGrpcService::new(channel, ep.clone(), self.compression),
                            );
                        } else {
                            warn!(message = "Skipping backend with invalid URI.", endpoint = %ep);
                        }
                    }
                }
            }

            // Collect a batch of events.
            let mut per_backend: HashMap<String, EventCollection> = HashMap::new();
            let mut total_events = 0usize;
            let max_events = self.batch_settings.item_limit;

            // Drain available events up to batch limits.
            let batch_timeout = tokio::time::sleep(std::time::Duration::from_millis(
                self.batch_settings.timeout.as_millis() as u64,
            ));
            tokio::pin!(batch_timeout);

            loop {
                tokio::select! {
                    maybe_event = input.next() => {
                        match maybe_event {
                            Some(mut event) => {
                                let key = extract_routing_key(&event, &routing_key);
                                let endpoint = ring
                                    .get(&key)
                                    .unwrap_or_else(|| ring.endpoints().first().map(|s| s.as_str()).unwrap_or(""))
                                    .to_string();

                                let byte_size = event.size_of();
                                let mut json_byte_size = telemetry().create_request_count_byte_size();
                                json_byte_size.add_event(&event, event.estimated_json_encoded_size_of());
                                let finalizers = event.take_finalizers();

                                let col = per_backend.entry(endpoint).or_insert_with(EventCollection::default);
                                col.finalizers.merge(finalizers);
                                col.events.push(event);
                                col.byte_size += byte_size;
                                col.json_byte_size += json_byte_size;

                                total_events += 1;
                                if total_events >= max_events {
                                    break; // batch full
                                }
                            }
                            None => {
                                // Input stream ended — flush remaining and exit.
                                for (endpoint, col) in per_backend {
                                    if let Some(svc) = services.get_mut(&endpoint) {
                                        let req = collection_into_request(col);
                                        let _ = svc.call(req).await;
                                    }
                                }
                                return Ok(());
                            }
                        }
                    }
                    _ = &mut batch_timeout => {
                        break;
                    }
                }
            }

            if total_events == 0 {
                continue;
            }

            // Send batched requests to each backend.
            for (endpoint, col) in per_backend {
                if col.events.is_empty() {
                    continue;
                }
                if let Some(svc) = services.get_mut(&endpoint) {
                    let req = collection_into_request(col);
                    match svc.call(req).await {
                        Ok(_) => {
                            counter!("vector_lb_backend_outcome", "endpoint" => endpoint.clone(), "outcome" => "success").increment(1);
                        }
                        Err(e) => {
                            counter!("vector_lb_backend_outcome", "endpoint" => endpoint.clone(), "outcome" => "error").increment(1);
                            warn!(
                                message = "Load-balanced gRPC send failed.",
                                endpoint = %endpoint,
                                error = ?e,
                            );
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event → OtlpRequest conversion
// ---------------------------------------------------------------------------

/// Reconstruct a `ResourceMetrics` (opentelemetry-proto types) from an
/// `OtelMetric` (otel-proto-types) via protobuf encode→decode.
pub(crate) fn otel_metric_event_to_resource_metrics(
    metric_event: &vector_lib::event::OtelMetric,
) -> vector_lib::opentelemetry::proto::metrics::v1::ResourceMetrics {
    use prost::Message;
    use vector_lib::opentelemetry::proto::{
        common::v1::InstrumentationScope as SinkScope,
        metrics::v1::{Metric as SinkMetric, ResourceMetrics, ScopeMetrics},
        resource::v1::Resource as SinkResource,
    };

    let metric_bytes = metric_event.metric().encode_to_vec();
    let sink_metric = SinkMetric::decode(bytes::Bytes::from(metric_bytes))
        .expect("Metric proto roundtrip");

    let resource = metric_event.resource().map(|r| {
        let b = r.encode_to_vec();
        SinkResource::decode(bytes::Bytes::from(b)).expect("Resource proto roundtrip")
    });

    let scope = metric_event.scope().map(|s| {
        let b = s.encode_to_vec();
        SinkScope::decode(bytes::Bytes::from(b)).expect("Scope proto roundtrip")
    });

    ResourceMetrics {
        resource,
        scope_metrics: vec![ScopeMetrics {
            scope,
            metrics: vec![sink_metric],
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    }
}

/// Reconstruct a `ResourceLogs` (opentelemetry-proto types) from an
/// `OtelLog` (otel-proto-types) via protobuf encode→decode.
pub(crate) fn otel_log_event_to_resource_logs(
    log_event: &vector_lib::event::OtelLog,
) -> vector_lib::opentelemetry::proto::logs::v1::ResourceLogs {
    use prost::Message;
    use vector_lib::opentelemetry::proto::{
        common::v1::InstrumentationScope as SinkScope,
        logs::v1::{LogRecord as SinkLogRecord, ResourceLogs, ScopeLogs},
        resource::v1::Resource as SinkResource,
    };

    let record_bytes = log_event.record().encode_to_vec();
    let sink_record = SinkLogRecord::decode(bytes::Bytes::from(record_bytes))
        .expect("LogRecord proto roundtrip");

    let resource = log_event.resource().map(|r| {
        let b = r.encode_to_vec();
        SinkResource::decode(bytes::Bytes::from(b)).expect("Resource proto roundtrip")
    });

    let scope = log_event.scope().map(|s| {
        let b = s.encode_to_vec();
        SinkScope::decode(bytes::Bytes::from(b)).expect("Scope proto roundtrip")
    });

    ResourceLogs {
        resource,
        scope_logs: vec![ScopeLogs {
            scope,
            log_records: vec![sink_record],
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    }
}

/// Reconstruct a `ResourceSpans` (opentelemetry-proto types) from an
/// `OtelSpan` (otel-proto-types) via protobuf encode→decode.
pub(crate) fn otel_span_event_to_resource_spans(
    span_event: &vector_lib::event::OtelSpan,
) -> vector_lib::opentelemetry::proto::trace::v1::ResourceSpans {
    use prost::Message;
    use vector_lib::opentelemetry::proto::{
        common::v1::InstrumentationScope as SinkScope,
        resource::v1::Resource as SinkResource,
        trace::v1::{ResourceSpans, ScopeSpans, Span as SinkSpan},
    };

    let span_bytes = span_event.span().encode_to_vec();
    let sink_span =
        SinkSpan::decode(bytes::Bytes::from(span_bytes)).expect("Span proto roundtrip");

    let resource = span_event.resource().map(|r| {
        let b = r.encode_to_vec();
        SinkResource::decode(bytes::Bytes::from(b)).expect("Resource proto roundtrip")
    });

    let scope = span_event.scope().map(|s| {
        let b = s.encode_to_vec();
        SinkScope::decode(bytes::Bytes::from(b)).expect("Scope proto roundtrip")
    });

    ResourceSpans {
        resource,
        scope_spans: vec![ScopeSpans {
            scope,
            spans: vec![sink_span],
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    }
}

fn collection_into_request(col: EventCollection) -> OtlpRequest {
    use vector_lib::opentelemetry::proto::{
        collector::{
            logs::v1::ExportLogsServiceRequest,
            metrics::v1::ExportMetricsServiceRequest,
            trace::v1::ExportTraceServiceRequest,
        },
        logs::v1::ResourceLogs,
        trace::v1::ResourceSpans,
    };

    use vector_lib::opentelemetry::proto::metrics::v1::ResourceMetrics;

    let n = col.events.len();
    let mut log_resources: Vec<ResourceLogs> = vec![];
    let mut metric_resources: Vec<ResourceMetrics> = vec![];
    let mut trace_resources: Vec<ResourceSpans> = vec![];

    for event in col.events {
        match event {
            Event::Log(ref log_event) => {
                log_resources.push(otel_log_event_to_resource_logs(log_event));
            }
            Event::Metric(ref metric_event) => {
                metric_resources.push(otel_metric_event_to_resource_metrics(metric_event));
            }
            Event::Trace(ref span_event) => {
                trace_resources.push(otel_span_event_to_resource_spans(span_event));
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
