//! The Azure Monitor Logs [`sol_lib::sink::VectorSink`]
//!
//! This module contains the [`sol_lib::sink::VectorSink`] instance that is responsible for
//! taking a stream of [`sol_lib::event::Event`] instances and forwarding them to the Azure
//! Monitor Logs service.

mod config;
mod service;
mod sink;
#[cfg(test)]
mod tests;

pub use config::AzureMonitorLogsConfig;
