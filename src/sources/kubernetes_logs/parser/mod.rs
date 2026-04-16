mod cri;
mod docker;
mod test_util;

use vector_lib::config::LogNamespace;

use crate::{
    event::{Event, Value},
    internal_events::KubernetesLogsFormatPickerEdgeCase,
    transforms::{FunctionTransform, OutputBuffer},
};

/// For OtelLog events, convert to a temporary LogEvent for parsing,
/// then transfer the parsed fields back as OtelLog attributes.
fn transform_otel_event(parser: &mut ParserState, output: &mut OutputBuffer, mut event: Event) {
    if let Event::Log(ref otel_log) = event {
        let body = otel_log.body_string();
        if body.is_empty() {
            return;
        }
        let bytes = bytes::Bytes::from(body);
        let tmp_otel = crate::event::OtelLog::from_bytes(bytes.clone());
        let tmp_event = Event::Log(tmp_otel);
        let mut tmp_output = OutputBuffer::with_capacity(1);
        match parser {
            ParserState::Docker(t) => t.transform(&mut tmp_output, tmp_event),
            ParserState::Cri(t) => t.transform(&mut tmp_output, tmp_event),
            _ => return,
        }
        for parsed in tmp_output.into_events() {
            if let Event::Log(ref parsed_otel) = parsed {
                if let Event::Log(ref mut otel_log) = event {
                    if let Some(msg) = parsed_otel.get_body() {
                        otel_log.set_body(crate::event::string_value(
                            msg.as_str().unwrap_or_default(),
                        ));
                    }
                    if let Some(stream) = parsed_otel.parse_path_and_get_value(".stream").ok().flatten() {
                        otel_log.set_attribute(
                            "stream".to_string(),
                            crate::event::string_value(stream.as_str().unwrap_or_default()),
                        );
                    }
                    if let Some(Value::Boolean(true)) = parsed_otel.parse_path_and_get_value("._partial").ok().flatten() {
                        otel_log.set_attribute(
                            "_partial".to_string(),
                            opentelemetry_proto::tonic::common::v1::AnyValue {
                                value: Some(opentelemetry_proto::tonic::common::v1::any_value::Value::BoolValue(true)),
                            },
                        );
                    }
                    if let Some(Value::Timestamp(ts)) = parsed_otel.parse_path_and_get_value(".timestamp").ok().flatten() {
                        otel_log.record_mut().time_unix_nano =
                            ts.timestamp_nanos_opt().unwrap_or(0) as u64;
                    }
                }
            }
            output.push(event.clone());
            return;
        }
    }
}

#[derive(Clone, Debug)]
enum ParserState {
    /// Runtime has not yet been detected.
    Uninitialized,

    /// Docker runtime is being used.
    Docker(docker::Docker),

    /// CRI is being used.
    Cri(cri::Cri),
}

#[derive(Clone, Debug)]
pub struct Parser {
    state: ParserState,
    log_namespace: LogNamespace,
}

impl Parser {
    pub const fn new(log_namespace: LogNamespace) -> Self {
        Self {
            state: ParserState::Uninitialized,
            log_namespace,
        }
    }
}

impl FunctionTransform for Parser {
    fn transform(&mut self, output: &mut OutputBuffer, event: Event) {
        let bytes_for_detection = if let Event::Log(ref otel_log) = event {
            let s = otel_log.body_string();
            if s.is_empty() {
                emit!(KubernetesLogsFormatPickerEdgeCase {
                    what: "got an OtelLog event without a body"
                });
                return;
            }
            bytes::Bytes::from(s)
        } else {
            emit!(KubernetesLogsFormatPickerEdgeCase {
                what: "got a non-log event"
            });
            return;
        };

        match &mut self.state {
            ParserState::Uninitialized => {
                self.state = if bytes_for_detection.len() > 1 && bytes_for_detection[0] == b'{' {
                    ParserState::Docker(docker::Docker::new(self.log_namespace))
                } else {
                    ParserState::Cri(cri::Cri::new(self.log_namespace))
                };
                self.transform(output, event)
            }
            ParserState::Docker(t) => {
                transform_otel_event(&mut ParserState::Docker(t.clone()), output, event);
            }
            ParserState::Cri(t) => {
                transform_otel_event(&mut ParserState::Cri(t.clone()), output, event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use vector_lib::lookup::event_path;
    use vrl::value;

    use super::*;
    use crate::{
        event::{Event, LogEvent},
        test_util::trace_init,
    };

    /// Picker has to work for all test cases for underlying parsers.
    fn valid_cases(log_namespace: LogNamespace) -> Vec<(Bytes, Vec<Event>)> {
        let mut valid_cases = vec![];
        valid_cases.extend(docker::tests::valid_cases(log_namespace));
        valid_cases.extend(cri::tests::valid_cases(log_namespace));
        valid_cases
    }

    fn invalid_cases() -> Vec<Bytes> {
        let mut invalid_cases = vec![];
        invalid_cases.extend(docker::tests::invalid_cases());
        invalid_cases
    }

    #[test]
    fn test_parsing_valid_vector_namespace() {
        trace_init();
        test_util::test_parser(
            || Parser::new(LogNamespace::Vector),
            |bytes| Event::Log(OtelLog::from_log_event(LogEvent::from(value!(bytes)))),
            valid_cases(LogNamespace::Vector),
        );
    }

    #[test]
    fn test_parsing_valid_legacy_namespace() {
        trace_init();
        test_util::test_parser(
            || Parser::new(LogNamespace::Legacy),
            |bytes| Event::Log(OtelLog::from_log_event(LogEvent::from(bytes))),
            valid_cases(LogNamespace::Legacy),
        );
    }

    #[test]
    fn test_parsing_invalid_legacy_namespace() {
        trace_init();

        let cases = invalid_cases();

        for bytes in cases {
            let mut parser = Parser::new(LogNamespace::Legacy);
            let input = LogEvent::from(bytes);
            let mut output = OutputBuffer::default();
            parser.transform(&mut output, input.into());

            assert!(output.is_empty(), "Expected no events: {output:?}");
        }
    }

    #[test]
    fn test_parsing_invalid_non_standard_events() {
        trace_init();

        let cases = vec![
            // No `message` field.
            (LogEvent::default(), LogNamespace::Legacy),
            // Non-bytes `message` field.
            (LogEvent::from(value!(123)), LogNamespace::Vector),
            (
                {
                    let mut input = LogEvent::default();
                    input.insert(event_path!("body"), 123);
                    input
                },
                LogNamespace::Legacy,
            ),
        ];

        for (input, log_namespace) in cases {
            let mut parser = Parser::new(log_namespace);
            let mut output = OutputBuffer::default();
            parser.transform(&mut output, input.into());

            assert!(output.is_empty(), "Expected no events: {output:?}");
        }
    }
}
