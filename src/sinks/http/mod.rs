//! The HTTP [`sol_lib::sink::VectorSink`].
//!
//! This module contains the [`sol_lib::sink::VectorSink`] instance that is responsible for
//! taking a stream of [`sol_lib::event::Event`]s and forwarding them to an HTTP server.

mod batch;
pub mod config;
mod encoder;
mod request_builder;
mod service;
mod sink;

#[cfg(test)]
mod tests;
