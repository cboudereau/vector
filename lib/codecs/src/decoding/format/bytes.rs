use bytes::Bytes;
use lookup::OwnedTargetPath;
use serde::{Deserialize, Serialize};
use smallvec::{SmallVec, smallvec};
use vector_core::{
    config::{DataType, LogNamespace, log_schema},
    event::{Event, OtelLog},
    schema,
    schema::meaning,
};
use vrl::value::Kind;

use super::Deserializer;

/// Config used to build a `BytesDeserializer`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BytesDeserializerConfig;

impl BytesDeserializerConfig {
    /// Creates a new `BytesDeserializerConfig`.
    pub const fn new() -> Self {
        Self
    }

    /// Build the `BytesDeserializer` from this configuration.
    pub fn build(&self) -> BytesDeserializer {
        BytesDeserializer
    }

    /// Return the type of event build by this deserializer.
    pub fn output_type(&self) -> DataType {
        DataType::Log
    }

    /// The schema produced by the deserializer.
    pub fn schema_definition(&self, log_namespace: LogNamespace) -> schema::Definition {
        match log_namespace {
            LogNamespace::Legacy => {
                let definition = schema::Definition::empty_legacy_namespace();
                if let Some(message_key) = log_schema().message_key() {
                    return definition.with_event_field(
                        message_key,
                        Kind::bytes(),
                        Some(meaning::MESSAGE),
                    );
                }
                definition
            }
            LogNamespace::Vector => {
                schema::Definition::new_with_default_metadata(Kind::bytes(), [log_namespace])
                    .with_meaning(OwnedTargetPath::event_root(), "message")
            }
        }
    }
}

/// Deserializer that converts bytes to an `OtelLog` event.
///
/// This deserializer can be considered as the no-op action for input where no
/// further decoding has been specified.
#[derive(Debug, Clone)]
pub struct BytesDeserializer;

impl BytesDeserializer {
    /// Deserializes the given bytes, which will always produce a single `OtelLog`.
    pub fn parse_single(&self, bytes: Bytes, _log_namespace: LogNamespace) -> OtelLog {
        OtelLog::from_bytes(bytes)
    }
}

impl Deserializer for BytesDeserializer {
    fn parse(
        &self,
        bytes: Bytes,
        log_namespace: LogNamespace,
    ) -> vector_common::Result<SmallVec<[Event; 1]>> {
        let otel_log = self.parse_single(bytes, log_namespace);
        Ok(smallvec![Event::OtelLog(otel_log)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_bytes_produces_otel_log() {
        let input = Bytes::from("foo");
        let deserializer = BytesDeserializer;

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            let events = deserializer.parse(input.clone(), namespace).unwrap();
            assert_eq!(events.len(), 1);

            let event = &events[0];
            assert!(matches!(event, Event::OtelLog(_)), "expected OtelLog");

            let otel_log = match event {
                Event::OtelLog(log) => log,
                _ => panic!("expected OtelLog"),
            };
            assert_eq!(otel_log.body_string(), "foo");
        }
    }
}
