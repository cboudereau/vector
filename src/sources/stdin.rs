use crate::{
    codecs::{
        BytesParserConfig, DecodingConfig, FramingConfig, NewlineDelimitedDecoderConfig,
        ParserConfig,
    },
    config::{log_schema, DataType, Resource, SourceConfig, SourceContext, SourceDescription},
    internal_events::StdinEventsReceived,
    shutdown::ShutdownSignal,
    sources::util::TcpError,
    Pipeline,
};
use async_stream::stream;
use bytes::Bytes;
use futures::{channel::mpsc, executor, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{io, thread};
use tokio_util::{codec::FramedRead, io::StreamReader};

#[derive(Deserialize, Serialize, Derivative, Debug, Clone)]
#[derivative(Default)]
#[serde(deny_unknown_fields, default)]
pub struct StdinConfig {
    pub max_length: Option<usize>,
    pub host_key: Option<String>,
    pub receive_buffer_bytes: Option<usize>,
    framing: Option<Box<dyn FramingConfig>>,
    #[derivative(Default(value = "Box::new(BytesParserConfig::new())"))]
    decoding: Box<dyn ParserConfig>,
}

inventory::submit! {
    SourceDescription::new::<StdinConfig>("stdin")
}

impl_generate_config_from_default!(StdinConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "stdin")]
impl SourceConfig for StdinConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        stdin_source(
            io::BufReader::new(io::stdin()),
            self.clone(),
            cx.shutdown,
            cx.out,
        )
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "stdin"
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::Stdin]
    }
}

pub fn stdin_source<R>(
    mut stdin: R,
    config: StdinConfig,
    shutdown: ShutdownSignal,
    out: Pipeline,
) -> crate::Result<super::Source>
where
    R: Send + io::BufRead + 'static,
{
    let host_key = config
        .host_key
        .unwrap_or_else(|| log_schema().host_key().to_string());
    let hostname = crate::get_hostname().ok();

    if config.framing.is_some() && config.max_length.is_some() {
        return Err("Using `max_length` is deprecated and does not have any effect when framing is provided. Configure `max_length` on the framing config instead.".into());
    }

    let max_length = config
        .max_length
        .unwrap_or_else(crate::serde::default_max_length);
    let framing = config.framing.unwrap_or_else(|| {
        Box::new(NewlineDelimitedDecoderConfig::new_with_max_length(
            max_length,
        ))
    });
    let decoder = DecodingConfig::new(Some(framing), Some(config.decoding)).build()?;

    let (mut sender, receiver) = mpsc::channel(1024);

    // Spawn background thread with blocking I/O to process stdin.
    //
    // This is recommended by Tokio, as otherwise the process will not shut down
    // until another newline is entered. See
    // https://github.com/tokio-rs/tokio/blob/a73428252b08bf1436f12e76287acbc4600ca0e5/tokio/src/io/stdin.rs#L33-L42
    thread::spawn(move || {
        info!("Capturing STDIN.");

        loop {
            let (buffer, len) = match stdin.fill_buf() {
                Ok(buffer) if buffer.is_empty() => break, // EOF.
                Ok(buffer) => (Ok(Bytes::copy_from_slice(buffer)), buffer.len()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => (Err(error), 0),
            };

            stdin.consume(len);

            if executor::block_on(sender.send(buffer)).is_err() {
                // Receiver has closed so we should shutdown.
                break;
            }
        }
    });

    let receive_buffer_bytes = config.receive_buffer_bytes;

    Ok(Box::pin(async move {
        let mut out =
            out.sink_map_err(|error| error!(message = "Unable to send event to out.", %error));

        let stream = StreamReader::new(receiver);
        let mut stream = if let Some(receive_buffer_bytes) = receive_buffer_bytes {
            FramedRead::with_capacity(stream, decoder, receive_buffer_bytes)
        } else {
            FramedRead::new(stream, decoder)
        }
        .take_until(shutdown);
        let result = stream! {
            loop {
                match stream.next().await {
                    Some(Ok((events, byte_size))) => {
                        emit!(&StdinEventsReceived {
                            byte_size,
                            count: events.len()
                        });

                        for mut event in events {
                            let log = event.as_mut_log();

                            log.insert(log_schema().source_type_key(), Bytes::from("stdin"));

                            if let Some(hostname) = &hostname {
                                log.insert(&host_key, hostname.clone());
                            }

                            yield event;
                        }
                    }
                    Some(Err(error)) => {
                        // Error is logged by `crate::codecs::Decoder`, no
                        // further handling is needed here.
                        if !error.can_continue() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        .map(Ok)
        .forward(&mut out)
        .await;

        info!("Finished sending.");

        let _ = out.flush().await; // Error emitted by sink_map_err.

        result
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_util::trace_init, Pipeline};
    use std::io::Cursor;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<StdinConfig>();
    }

    #[tokio::test]
    async fn stdin_decodes_line() {
        trace_init();

        let (tx, rx) = Pipeline::new_test();
        let config = StdinConfig::default();
        let buf = Cursor::new("hello world\nhello world again");

        stdin_source(buf, config, ShutdownSignal::noop(), tx)
            .unwrap()
            .await
            .unwrap();

        let mut stream = rx;

        let event = stream.next().await;
        assert_eq!(
            Some("hello world".into()),
            event.map(|event| event.as_log()[log_schema().message_key()].to_string_lossy())
        );

        let event = stream.next().await;
        assert_eq!(
            Some("hello world again".into()),
            event.map(|event| event.as_log()[log_schema().message_key()].to_string_lossy())
        );

        let event = stream.next().await;
        assert!(event.is_none());
    }
}
