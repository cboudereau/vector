use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use snafu::Snafu;
use vector_lib::config::LogNamespace;

use crate::{
    event::{self, Event},
    internal_events::KubernetesLogsDockerFormatParseError,
    transforms::{FunctionTransform, OutputBuffer},
};

pub const MESSAGE_KEY: &str = "log";
pub const STREAM_KEY: &str = "stream";
pub const TIMESTAMP_KEY: &str = "time";

/// Parser for the Docker log format.
///
/// Expects logs to arrive in a JSONLines format with the fields names and
/// contents specific to the implementation of the Docker `json-file` log driver.
///
/// Normalizes parsed data for consistency.
#[derive(Clone, Debug)]
pub(super) struct Docker {
    #[allow(dead_code)]
    log_namespace: LogNamespace,
}

impl Docker {
    pub const fn new(log_namespace: LogNamespace) -> Self {
        Self { log_namespace }
    }
}

impl FunctionTransform for Docker {
    fn transform(&mut self, output: &mut OutputBuffer, mut event: Event) {
        if let Event::Log(ref mut otel_log) = event {
            if let Err(err) = parse_json_otel(otel_log) {
                emit!(KubernetesLogsDockerFormatParseError { error: &err });
                return;
            }
            if let Err(err) = normalize_event_otel(otel_log) {
                emit!(KubernetesLogsDockerFormatParseError { error: &err });
                return;
            }
        }
        output.push(event);
    }
}

fn parse_json_otel(otel_log: &mut crate::event::OtelLog) -> Result<(), ParsingError> {
    let body = otel_log.body_string();
    if body.is_empty() {
        return Err(ParsingError::NoMessageField);
    }

    match serde_json::from_str::<JsonValue>(&body) {
        Ok(JsonValue::Object(object)) => {
            let mut found_log = false;
            for (key, value) in object {
                match key.as_str() {
                    MESSAGE_KEY => {
                        found_log = true;
                        let s = match value {
                            JsonValue::String(s) => s,
                            _ => return Err(ParsingError::MessageFieldNotInBytes),
                        };
                        otel_log.set_body(crate::event::string_value(s));
                    }
                    STREAM_KEY => {
                        let s = match value {
                            JsonValue::String(s) => s,
                            other => other.to_string(),
                        };
                        otel_log.set_attribute(STREAM_KEY.to_string(), crate::event::string_value(s));
                    }
                    TIMESTAMP_KEY => {
                        let s = match value {
                            JsonValue::String(s) => s,
                            other => other.to_string(),
                        };
                        otel_log.set_attribute(TIMESTAMP_KEY.to_string(), crate::event::string_value(s));
                    }
                    _ => {}
                };
            }
            if !found_log {
                return Err(ParsingError::NoMessageField);
            }
            Ok(())
        }
        Ok(_) => Err(ParsingError::NotAnObject {
            message: Bytes::from(body),
        }),
        Err(err) => Err(ParsingError::InvalidJson {
            source: err,
            message: Bytes::from(body),
        }),
    }
}

const DOCKER_MESSAGE_SPLIT_THRESHOLD: usize = 16 * 1024; // 16 Kib

fn normalize_event_otel(otel_log: &mut crate::event::OtelLog) -> Result<(), NormalizationError> {
    let time_attr = otel_log
        .remove_attribute(TIMESTAMP_KEY)
        .ok_or(NormalizationError::TimeFieldMissing)?;
    let time_str = match time_attr.value {
        Some(crate::event::OtelValueKind::StringValue(s)) if !s.is_empty() => s,
        _ => return Err(NormalizationError::TimeValueUnexpectedType),
    };
    let dt = DateTime::parse_from_rfc3339(&time_str)
        .map_err(|source| NormalizationError::TimeParsing { source })?;
    otel_log.record_mut().time_unix_nano = dt.with_timezone(&Utc).timestamp_nanos_opt().unwrap_or(0) as u64;

    let body = otel_log.body_string();
    if body.is_empty() {
        return Err(NormalizationError::LogFieldMissing);
    }
    let mut message = Bytes::from(body);
    let mut is_partial = message.len() == DOCKER_MESSAGE_SPLIT_THRESHOLD;
    if message.last().map(|&b| b as char == '\n').unwrap_or(false) {
        message.truncate(message.len() - 1);
        is_partial = false;
    }
    otel_log.set_body(crate::event::string_value(
        String::from_utf8_lossy(&message),
    ));

    if is_partial {
        otel_log.set_attribute(
            event::PARTIAL.to_string(),
            opentelemetry_proto::tonic::common::v1::AnyValue {
                value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::BoolValue(true)),
            },
        );
    }

    Ok(())
}

#[derive(Debug, Snafu)]
enum ParsingError {
    NoMessageField,
    MessageFieldNotInBytes,
    #[snafu(display(
        "Could not parse json: {} in message {:?}",
        source,
        String::from_utf8_lossy(message)
    ))]
    InvalidJson {
        source: serde_json::Error,
        message: Bytes,
    },
    #[snafu(display("Message was not an object: {:?}", String::from_utf8_lossy(message)))]
    NotAnObject {
        message: Bytes,
    },
}

#[derive(Debug, Snafu)]
#[allow(dead_code)]
enum NormalizationError {
    TimeFieldMissing,
    TimeValueUnexpectedType,
    TimeParsing { source: chrono::ParseError },
    LogFieldMissing,
    LogValueUnexpectedType,
}

#[cfg(test)]
pub mod tests {
    use vrl::value;

    use super::{super::test_util, *};
    use crate::{event::LogEvent, test_util::trace_init};

    fn make_long_string(base: &str, len: usize) -> String {
        base.chars().cycle().take(len).collect()
    }

    /// Shared test cases.
    pub fn valid_cases(log_namespace: LogNamespace) -> Vec<(Bytes, Vec<Event>)> {
        vec![
            (
                Bytes::from(
                    r#"{"log": "The actual log line\n", "stream": "stderr", "time": "2016-10-05T00:00:30.082640485Z"}"#,
                ),
                vec![test_util::make_log_event(
                    value!("The actual log line"),
                    "2016-10-05T00:00:30.082640485Z",
                    "stderr",
                    false,
                    log_namespace,
                )],
            ),
            (
                Bytes::from(
                    r#"{"log": "A line without newline char at the end", "stream": "stdout", "time": "2016-10-05T00:00:30.082640485Z"}"#,
                ),
                vec![test_util::make_log_event(
                    value!("A line without newline char at the end"),
                    "2016-10-05T00:00:30.082640485Z",
                    "stdout",
                    false,
                    log_namespace,
                )],
            ),
            // Partial message due to message length.
            (
                Bytes::from(
                    [
                        r#"{"log": ""#,
                        make_long_string("partial ", 16 * 1024).as_str(),
                        r#"", "stream": "stdout", "time": "2016-10-05T00:00:30.082640485Z"}"#,
                    ]
                    .join(""),
                ),
                vec![test_util::make_log_event(
                    value!(make_long_string("partial ", 16 * 1024)),
                    "2016-10-05T00:00:30.082640485Z",
                    "stdout",
                    true,
                    log_namespace,
                )],
            ),
            // Non-partial message, because message length matches but
            // the message also ends with newline.
            (
                Bytes::from(
                    [
                        r#"{"log": ""#,
                        make_long_string("non-partial ", 16 * 1024 - 1).as_str(),
                        r"\n",
                        r#"", "stream": "stdout", "time": "2016-10-05T00:00:30.082640485Z"}"#,
                    ]
                    .join(""),
                ),
                vec![test_util::make_log_event(
                    value!(make_long_string("non-partial ", 16 * 1024 - 1)),
                    "2016-10-05T00:00:30.082640485Z",
                    "stdout",
                    false,
                    log_namespace,
                )],
            ),
        ]
    }

    pub fn invalid_cases() -> Vec<Bytes> {
        vec![
            // Empty string.
            Bytes::from(""),
            // Incomplete.
            Bytes::from("{"),
            // Random non-JSON text.
            Bytes::from("hello world"),
            // Random JSON non-object.
            Bytes::from("123"),
            // Empty JSON object.
            Bytes::from("{}"),
            // No timestamp.
            Bytes::from(r#"{"log": "Hello world", "stream": "stdout"}"#),
            // Timestamp not a string.
            Bytes::from(r#"{"log": "Hello world", "stream": "stdout", "time": 123}"#),
            // Empty timestamp.
            Bytes::from(r#"{"log": "Hello world", "stream": "stdout", "time": ""}"#),
            // Invalid timestamp.
            Bytes::from(r#"{"log": "Hello world", "stream": "stdout", "time": "qwerty"}"#),
            // No log field.
            Bytes::from(r#"{"stream": "stderr", "time": "2016-10-05T00:00:30.082640485Z"}"#),
            // Log is not a string.
            Bytes::from(
                r#"{"log": 123, "stream": "stderr", "time": "2016-10-05T00:00:30.082640485Z"}"#,
            ),
        ]
    }

    #[test]
    fn test_parsing_valid_vector_namespace() {
        trace_init();

        test_util::test_parser(
            || Docker {
                log_namespace: LogNamespace::Vector,
            },
            |bytes| Event::Log(OtelLog::from_log_event(LogEvent::from(value!(bytes)))),
            valid_cases(LogNamespace::Vector),
        );
    }

    #[test]
    fn test_parsing_valid_legacy_namespace() {
        trace_init();

        test_util::test_parser(
            || Docker {
                log_namespace: LogNamespace::Legacy,
            },
            |bytes| Event::Log(OtelLog::from_log_event(LogEvent::from(bytes))),
            valid_cases(LogNamespace::Legacy),
        );
    }

    #[test]
    fn test_parsing_invalid_vector_namespace() {
        trace_init();

        let cases = invalid_cases();

        for bytes in cases {
            let mut parser = Docker::new(LogNamespace::Vector);
            let input = LogEvent::from(value!(bytes));
            let mut output = OutputBuffer::default();
            parser.transform(&mut output, input.into());

            assert!(output.is_empty(), "Expected no events: {output:?}");
        }
    }

    #[test]
    fn test_parsing_invalid_legacy_namespace() {
        trace_init();

        let cases = invalid_cases();

        for bytes in cases {
            let mut parser = Docker::new(LogNamespace::Legacy);
            let input = LogEvent::from(bytes);
            let mut output = OutputBuffer::default();
            parser.transform(&mut output, input.into());

            assert!(output.is_empty(), "Expected no events: {output:?}");
        }
    }
}
