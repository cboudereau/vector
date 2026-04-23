#[cfg(all(test, feature = "opentelemetry-integration-tests"))]
mod integration_tests;
#[cfg(test)]
mod tests;

#[cfg(feature = "sources-opentelemetry")]
pub mod config;
pub(crate) mod grpc;
#[cfg(feature = "sources-opentelemetry")]
mod http;
#[cfg(feature = "sources-opentelemetry")]
mod reply;
#[cfg(feature = "sources-opentelemetry")]
mod status;
