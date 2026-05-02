use std::path::PathBuf;

use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use vector_lib::{
    codecs::{
        NewlineDelimitedDecoder,
        decoding::{Deserializer, Framer},
    },
    configurable::configurable_component,
};

use super::{ConversionUnit, StatsdDeserializer, default_convert_to, default_sanitize};
use crate::{
    SourceSender,
    codecs::Decoder,
    shutdown::ShutdownSignal,
    sources::{Source, util::build_unix_stream_source},
};

/// Unix domain socket configuration for the `statsd` source.
#[configurable_component]
#[derive(Clone, Debug)]
pub struct UnixConfig {
    /// The Unix socket path.
    ///
    /// This should be an absolute path.
    #[configurable(metadata(docs::examples = "/path/to/socket"))]
    pub path: PathBuf,

    #[serde(default = "default_sanitize")]
    #[configurable(derived)]
    pub sanitize: bool,

    #[serde(default = "default_convert_to")]
    #[configurable(derived)]
    pub convert_to: ConversionUnit,
}

pub fn statsd_unix(
    config: UnixConfig,
    shutdown: ShutdownSignal,
    out: SourceSender,
    resource: Resource,
    scope: InstrumentationScope,
) -> crate::Result<Source> {
    let decoder = Decoder::new(
        Framer::NewlineDelimited(NewlineDelimitedDecoder::new()),
        Deserializer::Boxed(Box::new(StatsdDeserializer::unix_with_otel(
            config.sanitize,
            config.convert_to,
            resource,
            scope,
        ))),
    );

    build_unix_stream_source(
        config.path,
        None,
        decoder,
        |_events, _host| {},
        shutdown,
        out,
    )
}
