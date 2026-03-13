use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::Encoder;
use vector_core::{config::DataType, event::Event, schema};

use crate::encoding::format::common::get_serializer_schema_requirement;

/// Config used to build a `RawMessageSerializer`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RawMessageSerializerConfig;

impl RawMessageSerializerConfig {
    /// Creates a new `RawMessageSerializerConfig`.
    pub const fn new() -> Self {
        Self
    }

    /// Build the `RawMessageSerializer` from this configuration.
    pub const fn build(&self) -> RawMessageSerializer {
        RawMessageSerializer
    }

    /// The data type of events that are accepted by `RawMessageSerializer`.
    pub fn input_type(&self) -> DataType {
        DataType::Log
    }

    /// The schema required by the serializer.
    pub fn schema_requirement(&self) -> schema::Requirement {
        get_serializer_schema_requirement()
    }
}

/// Serializer that converts an `Event` to bytes by extracting the message key.
#[derive(Debug, Clone)]
pub struct RawMessageSerializer;

impl Encoder<Event> for RawMessageSerializer {
    type Error = vector_common::Error;

    fn encode(&mut self, event: Event, buffer: &mut BytesMut) -> Result<(), Self::Error> {
        match &event {
            Event::Log(log) => {
                if let Some(bytes) = log.get_message().map(|value| value.coerce_to_bytes()) {
                    buffer.put(bytes);
                }
            }
            Event::OtelLog(otel_log) => {
                let s = otel_log.body_string();
                if !s.is_empty() {
                    buffer.put(s.as_bytes());
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};
    use vector_core::event::LogEvent;

    use super::*;

    #[test]
    fn serialize_bytes() {
        let input = Event::from(LogEvent::from_str_legacy("foo"));
        let mut serializer = RawMessageSerializer;

        let mut buffer = BytesMut::new();
        serializer.encode(input, &mut buffer).unwrap();

        assert_eq!(buffer.freeze(), Bytes::from("foo"));
    }

    #[test]
    fn serialize_otel_log() {
        use otel_proto_types::common::v1::AnyValue;
        use vector_core::event::OtelLogEvent;

        let event = Event::OtelLog(OtelLogEvent::new(
            otel_proto_types::logs::v1::LogRecord {
                body: Some(AnyValue {
                    value: Some(
                        otel_proto_types::common::v1::any_value::Value::StringValue(
                            "otel raw message".into(),
                        ),
                    ),
                }),
                ..Default::default()
            },
        ));

        let mut serializer = RawMessageSerializer;
        let mut buffer = BytesMut::new();
        serializer.encode(event, &mut buffer).unwrap();

        assert_eq!(buffer.freeze(), Bytes::from("otel raw message"));
    }
}
