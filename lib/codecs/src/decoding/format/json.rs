use bytes::Bytes;
use derivative::Derivative;
use lookup::owned_value_path;
use smallvec::{SmallVec, smallvec};
use vector_config::configurable_component;
use vector_core::{
    config::{DataType, LogNamespace},
    event::{Event, OtelLog},
    schema,
};
use vrl::value::Kind;

use super::{Deserializer, default_lossy};

/// Config used to build a `JsonDeserializer`.
#[configurable_component]
#[derive(Debug, Clone, Default)]
pub struct JsonDeserializerConfig {
    /// JSON-specific decoding options.
    #[serde(default, skip_serializing_if = "vector_core::serde::is_default")]
    pub json: JsonDeserializerOptions,
}

impl JsonDeserializerConfig {
    /// Creates a new `JsonDeserializerConfig`.
    pub fn new(options: JsonDeserializerOptions) -> Self {
        Self { json: options }
    }

    /// Build the `JsonDeserializer` from this configuration.
    pub fn build(&self) -> JsonDeserializer {
        Into::<JsonDeserializer>::into(self)
    }

    /// Return the type of event build by this deserializer.
    pub fn output_type(&self) -> DataType {
        DataType::Log
    }

    /// The schema produced by the deserializer.
    pub fn schema_definition(&self, log_namespace: LogNamespace) -> schema::Definition {
        match log_namespace {
            LogNamespace::Legacy => {
                let mut definition =
                    schema::Definition::empty_legacy_namespace().unknown_fields(Kind::json());

                {
                    let timestamp_key = owned_value_path!("time_unix_nano");
                    definition = definition.try_with_field(
                        &timestamp_key,
                        Kind::json(),
                        Some("timestamp"),
                    );
                }
                definition
            }
            LogNamespace::Vector => {
                schema::Definition::new_with_default_metadata(Kind::json(), [log_namespace])
            }
        }
    }
}

/// JSON-specific decoding options.
#[configurable_component]
#[derive(Debug, Clone, PartialEq, Eq, Derivative)]
#[derivative(Default)]
pub struct JsonDeserializerOptions {
    /// Determines whether to replace invalid UTF-8 sequences instead of failing.
    ///
    /// When true, invalid UTF-8 sequences are replaced with the [`U+FFFD REPLACEMENT CHARACTER`][U+FFFD].
    ///
    /// [U+FFFD]: https://en.wikipedia.org/wiki/Specials_(Unicode_block)#Replacement_character
    #[serde(
        default = "default_lossy",
        skip_serializing_if = "vector_core::serde::is_default"
    )]
    #[derivative(Default(value = "default_lossy()"))]
    pub lossy: bool,
}

/// Deserializer that builds `Event`s from a byte frame containing JSON.
#[derive(Debug, Clone, Derivative)]
#[derivative(Default)]
pub struct JsonDeserializer {
    #[derivative(Default(value = "default_lossy()"))]
    lossy: bool,
}

impl JsonDeserializer {
    /// Creates a new `JsonDeserializer`.
    pub fn new(lossy: bool) -> Self {
        Self { lossy }
    }
}

impl Deserializer for JsonDeserializer {
    fn parse(
        &self,
        bytes: Bytes,
        _log_namespace: LogNamespace,
    ) -> vector_common::Result<SmallVec<[Event; 1]>> {
        if bytes.is_empty() {
            return Ok(smallvec![]);
        }

        let json: serde_json::Value = match self.lossy {
            true => serde_json::from_str(&String::from_utf8_lossy(&bytes)),
            false => serde_json::from_slice(&bytes),
        }
        .map_err(|error| format!("Error parsing JSON: {error:?}"))?;

        let events = match json {
            serde_json::Value::Array(values) => values
                .into_iter()
                .map(|json| Event::Log(OtelLog::from_json_value(json)))
                .collect::<SmallVec<[Event; 1]>>(),
            _ => smallvec![Event::Log(OtelLog::from_json_value(json))],
        };

        Ok(events)
    }
}

impl From<&JsonDeserializerConfig> for JsonDeserializer {
    fn from(config: &JsonDeserializerConfig) -> Self {
        Self {
            lossy: config.json.lossy,
        }
    }
}

#[cfg(test)]
mod tests {
    use vector_core::event::Value;

    use super::*;

    fn get_attribute_value(event: &Event, key: &str) -> Option<Value> {
        match event {
            Event::Log(otel_log) => otel_log.get(lookup::event_path!(key)),
            _ => None,
        }
    }

    #[test]
    fn deserialize_json_produces_otel_log() {
        let input = Bytes::from(r#"{ "foo": 123 }"#);
        let deserializer = JsonDeserializer::default();

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            let events = deserializer.parse(input.clone(), namespace).unwrap();
            assert_eq!(events.len(), 1);

            let event = &events[0];
            assert!(matches!(event, Event::Log(_)), "expected Log(OtelLog)");

            let val = get_attribute_value(event, "foo");
            assert_eq!(val, Some(Value::Integer(123)));
        }
    }

    #[test]
    fn deserialize_json_array_produces_otel_logs() {
        let input = Bytes::from(r#"[{ "foo": 123 }, { "bar": 456 }]"#);
        let deserializer = JsonDeserializer::default();

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            let events = deserializer.parse(input.clone(), namespace).unwrap();
            assert_eq!(events.len(), 2);

            assert!(matches!(&events[0], Event::Log(_)));
            assert!(matches!(&events[1], Event::Log(_)));

            let foo = get_attribute_value(&events[0], "foo");
            assert_eq!(foo, Some(Value::Integer(123)));

            let bar = get_attribute_value(&events[1], "bar");
            assert_eq!(bar, Some(Value::Integer(456)));
        }
    }

    #[test]
    fn deserialize_skip_empty() {
        let input = Bytes::from("");
        let deserializer = JsonDeserializer::default();

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            let events = deserializer.parse(input.clone(), namespace).unwrap();
            assert!(events.is_empty());
        }
    }

    #[test]
    fn deserialize_error_invalid_json() {
        let input = Bytes::from("{ foo");
        let deserializer = JsonDeserializer::default();

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            assert!(deserializer.parse(input.clone(), namespace).is_err());
        }
    }

    #[test]
    fn deserialize_non_lossy_error_invalid_utf8() {
        let input = Bytes::from(b"{ \"foo\": \"Hello \xF0\x90\x80World\" }".as_slice());
        let deserializer = JsonDeserializer::new(false);

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            assert!(deserializer.parse(input.clone(), namespace).is_err());
        }
    }

    #[test]
    fn deserialize_lossy_replace_invalid_utf8() {
        let input = Bytes::from(b"{ \"foo\": \"Hello \xF0\x90\x80World\" }".as_slice());
        let deserializer = JsonDeserializer::new(true);

        for namespace in [LogNamespace::Legacy, LogNamespace::Vector] {
            let events = deserializer.parse(input.clone(), namespace).unwrap();
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], Event::Log(_)));
        }
    }
}
