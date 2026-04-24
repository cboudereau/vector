//! The `vector` source. See [VectorConfig].
use std::net::SocketAddr;

use futures::TryFutureExt;
use tonic::{codec::CompressionEncoding, service::RoutesBuilder};
use vector_lib::{
    codecs::BytesDeserializerConfig,
    config::LogNamespace,
    configurable::configurable_component,
    opentelemetry::proto::collector::{
        logs::v1::logs_service_server::LogsServiceServer,
        metrics::v1::metrics_service_server::MetricsServiceServer,
        trace::v1::trace_service_server::TraceServiceServer,
    },
};

use crate::{
    config::{
        DataType, GenerateConfig, Resource, SourceAcknowledgementsConfig, SourceConfig,
        SourceContext, SourceOutput,
    },
    internal_events::EventsReceived,
    serde::bool_or_struct,
    sources::{
        Source,
        opentelemetry::grpc::{LOGS, METRICS, TRACES, Service},
        util::grpc::run_grpc_server_with_routes,
    },
    tls::{MaybeTlsSettings, TlsEnableableConfig},
};

/// Marker type for version two of the configuration for the `vector` source.
#[configurable_component]
#[derive(Clone, Debug)]
enum VectorConfigVersion {
    /// Marker value for version two.
    #[serde(rename = "2")]
    V2,
}

/// Configuration for the `vector` source.
#[configurable_component(source("vector", "Collect observability data from a Vector instance."))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct VectorConfig {
    /// Version of the configuration.
    version: Option<VectorConfigVersion>,

    /// The socket address to listen for connections on.
    ///
    /// It _must_ include a port.
    pub address: SocketAddr,

    #[configurable(derived)]
    #[serde(default)]
    tls: Option<TlsEnableableConfig>,

    #[configurable(derived)]
    #[serde(default, deserialize_with = "bool_or_struct")]
    acknowledgements: SourceAcknowledgementsConfig,

}

impl VectorConfig {
    /// Creates a `VectorConfig` with the given address.
    pub fn from_address(addr: SocketAddr) -> Self {
        Self {
            address: addr,
            ..Default::default()
        }
    }
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            version: None,
            address: "0.0.0.0:6000".parse().unwrap(),
            tls: None,
            acknowledgements: Default::default(),
        }
    }
}

impl GenerateConfig for VectorConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(VectorConfig::default()).unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "vector")]
impl SourceConfig for VectorConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<Source> {
        let tls_settings = MaybeTlsSettings::from_config(self.tls.as_ref(), true)?;
        let acknowledgements = cx.do_acknowledgements(self.acknowledgements);
        let events_received = register!(EventsReceived);

        let service = Service {
            pipeline: cx.out,
            acknowledgements,
            events_received,
        };

        let log_service = LogsServiceServer::new(service.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(usize::MAX);

        let metrics_service = MetricsServiceServer::new(service.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(usize::MAX);

        let trace_service = TraceServiceServer::new(service)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(usize::MAX);

        let mut builder = RoutesBuilder::default();
        builder
            .add_service(log_service)
            .add_service(metrics_service)
            .add_service(trace_service);

        let source = run_grpc_server_with_routes(
            self.address,
            tls_settings,
            builder.routes(),
            cx.shutdown,
        )
        .map_err(|error| {
            error!(message = "Source future failed.", %error);
        });

        Ok(Box::pin(source))
    }

    fn outputs(&self) -> Vec<SourceOutput> {
        let log_namespace = LogNamespace::Vector;

        let schema_definition = BytesDeserializerConfig
            .schema_definition(log_namespace)
            .with_standard_vector_source_metadata();

        vec![
            SourceOutput::new_maybe_logs(DataType::all_bits(), schema_definition).with_port(LOGS),
            SourceOutput::new_metrics().with_port(METRICS),
            SourceOutput::new_traces().with_port(TRACES),
        ]
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::tcp(self.address)]
    }

    fn can_acknowledge(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<super::VectorConfig>();
    }
}

#[cfg(feature = "sinks-vector")]
#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use futures::stream::into_event_stream;
    use vector_lib::event::EventStatus;

    use super::*;
    use crate::{
        SourceSender,
        config::{SinkConfig as _, SinkContext},
        sinks::vector::VectorConfig as SinkConfig,
        test_util,
    };

    async fn run_test(vector_source_config_str: &str, addr: SocketAddr) {
        let config = format!(r#"address = "{addr}""#);
        let source: VectorConfig = toml::from_str(&config).unwrap();

        let (mut tx, recv) = SourceSender::new_test_finalize(EventStatus::Delivered);
        let logs_output = tx
            .add_outputs(EventStatus::Delivered, LOGS.to_string())
            .flat_map(into_event_stream);

        let server = source
            .build(SourceContext::new_test(tx, None))
            .await
            .unwrap();
        tokio::spawn(server);
        test_util::wait_for_tcp(addr).await;

        let sink: SinkConfig = toml::from_str(vector_source_config_str).unwrap();
        let cx = SinkContext::default();
        let (sink, _) = sink.build(cx).await.unwrap();

        let (events, stream) = test_util::random_events_with_stream(100, 100, None);
        sink.run(stream).await.unwrap();

        let output = test_util::collect_ready(logs_output).await;
        // Drop unused default output receiver
        drop(recv);
        assert_eq!(events.len(), output.len());
    }

    #[tokio::test]
    async fn receive_message() {
        let (_guard, addr) = test_util::addr::next_addr();

        let config = format!(r#"address = "{addr}""#);
        run_test(&config, addr).await;
    }

    #[tokio::test]
    async fn receive_compressed_message() {
        let (_guard, addr) = test_util::addr::next_addr();

        let config = format!(
            r#"address = "{addr}"
            compression=true"#
        );
        run_test(&config, addr).await;
    }
}
