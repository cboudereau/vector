//! The Honeycomb [`sol_lib::sink::VectorSink`].
//!
//! This module contains the [`sol_lib::sink::VectorSink`] instance that is responsible for
//! taking a stream of [`sol_lib::event::Event`]s and forwarding them to the Honeycomb service.

mod config;
mod encoder;
mod request_builder;
mod service;
mod sink;

#[cfg(test)]
mod tests;
