mod cri;
mod docker;
mod test_util;

use vector_lib::config::LogNamespace;

use crate::{
    event::{Event, Value},
    internal_events::KubernetesLogsFormatPickerEdgeCase,
    sources::kubernetes_logs::transform_utils::get_message_path,
    transforms::{FunctionTransform, OutputBuffer},
};

/// For OtelLog events, convert to a temporary LogEvent for parsing,
/// then transfer the parsed fields back as OtelLog attributes.
fn transform_otel_event(parser: &mut ParserState, output: &mut OutputBuffer, mut event: Event) {
    if let Event::OtelLog(ref otel_log) = event {
        let body = otel_log.body_string();
        if body.is_empty() {
            return;
        }
        let bytes = bytes::Bytes::from(body);
        let tmp_log = crate::event::LogEvent::from(Value::Bytes(bytes));
        let tmp_event = Event::Log(tmp_log);
        let mut tmp_output = OutputBuffer::with_capacity(1);
        match parser {
            ParserState::Docker(t) => t.transform(&mut tmp_output, tmp_event),
            ParserState::Cri(t) => t.transform(&mut tmp_output, tmp_event),
            _ => return,
        }
        for parsed in tmp_output.into_events() {
            if let Event::Log(ref parsed_log) = parsed {
                if let Event::OtelLog(ref mut otel_log) = event {
                    if let Some(msg) = parsed_log.get(".message") {
                        otel_log.set_body(crate::event::string_value(
                            msg.as_str().unwrap_or_default(),
                        ));
                    }
                    if let Some(stream) = parsed_log.get(".stream") {
                        otel_log.set_attribute(
                            "stream".to_string(),
                            crate::event::string_value(stream.as_str().unwrap_or_default()),
                        );
                    }
                    if let Some(Value::Boolean(true)) = parsed_log.get("._partial") {
                        otel_log.set_attribute(
                            "_partial".to_string(),
                            crate::event::string_value("true"),
                        );
                    }
                    if let Some(Value::Timestamp(ts)) = parsed_log.get(".timestamp") {
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
        let is_otel = matches!(event, Event::OtelLog(_));

        let bytes_for_detection = if is_otel {
            if let Event::OtelLog(ref otel_log) = event {
                let s = otel_log.body_string();
                if s.is_empty() {
                    emit!(KubernetesLogsFormatPickerEdgeCase {
                        what: "got an OtelLog event without a body"
                    });
                    return;
                }
                Some(bytes::Bytes::from(s))
            } else {
                None
            }
        } else {
            None
        };

        match &mut self.state {
            ParserState::Uninitialized => {
                let bytes = if let Some(ref b) = bytes_for_detection {
                    b.clone()
                } else {
                    let message_field = get_message_path(self.log_namespace);
                    let message = match event.as_log().get(&message_field) {
                        Some(message) => message,
                        None => {
                            emit!(KubernetesLogsFormatPickerEdgeCase {
                                what: "got an event without a message"
                            });
                            return;
                        }
                    };

                    match message {
                        Value::Bytes(bytes) => bytes.clone(),
                        _ => {
                            emit!(KubernetesLogsFormatPickerEdgeCase {
                                what: "got an event with non-bytes message"
                            });
                            return;
                        }
                    }
                };

                self.state = if bytes.len() > 1 && bytes[0] == b'{' {
                    ParserState::Docker(docker::Docker::new(self.log_namespace))
                } else {
                    ParserState::Cri(cri::Cri::new(self.log_namespace))
                };
                self.transform(output, event)
            }
            ParserState::Docker(t) => {
                if is_otel {
                    transform_otel_event(&mut ParserState::Docker(t.clone()), output, event);
                } else {
                    t.transform(output, event);
                }
            }
            ParserState::Cri(t) => {
                if is_otel {
                    transform_otel_event(&mut ParserState::Cri(t.clone()), output, event);
                } else {
                    t.transform(output, event);
                }
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
            |bytes| Event::Log(LogEvent::from(value!(bytes))),
            valid_cases(LogNamespace::Vector),
        );
    }

    #[test]
    fn test_parsing_valid_legacy_namespace() {
        trace_init();
        test_util::test_parser(
            || Parser::new(LogNamespace::Legacy),
            |bytes| Event::Log(LogEvent::from(bytes)),
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
                    input.insert(event_path!("message"), 123);
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
