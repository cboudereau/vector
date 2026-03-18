use vector_lib::{
    config::LogNamespace,
    event::Event,
};
use warp::http::{HeaderMap, HeaderValue};

use crate::{
    event::string_value,
    sources::http_server::HttpConfigParamKind,
};

pub fn add_headers(
    events: &mut [Event],
    headers_config: &[HttpConfigParamKind],
    headers: &HeaderMap,
    _log_namespace: LogNamespace,
    _source_name: &'static str,
) {
    for h in headers_config {
        match h {
            // Add each non-wildcard containing header that was specified
            // in the `headers` config option to the event if an exact match
            // is found.
            HttpConfigParamKind::Exact(header_name) => {
                let value = headers.get(header_name).map(HeaderValue::as_bytes);

                for event in events.iter_mut() {
                    match event {
                        Event::Log(otel_log) => {
                            if let Some(v) = value {
                                otel_log.set_attribute(
                                    format!("http.header.{header_name}"),
                                    string_value(String::from_utf8_lossy(v).into_owned()),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            HttpConfigParamKind::Glob(header_pattern) => {
                for header_name in headers.keys() {
                    if header_pattern
                        .matches_with(header_name.as_str(), glob::MatchOptions::default())
                    {
                        let value = headers.get(header_name).map(HeaderValue::as_bytes);

                        for event in events.iter_mut() {
                            match event {
                                Event::Log(otel_log) => {
                                    if let Some(v) = value {
                                        otel_log.set_attribute(
                                            format!("http.header.{}", header_name.as_str()),
                                            string_value(
                                                String::from_utf8_lossy(v).into_owned(),
                                            ),
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use vector_lib::config::LogNamespace;
    use vrl::{path, value};
    use warp::http::HeaderMap;

    use crate::{
        event::LogEvent,
        sources::{http_server::HttpConfigParamKind, util::add_headers},
    };

    #[test]
    fn multiple_headers() {
        let header_names = [
            HttpConfigParamKind::Exact("Content-Type".into()),
            HttpConfigParamKind::Exact("User-Agent".into()),
        ];
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", "application/x-protobuf".parse().unwrap());
        headers.insert("User-Agent", "Test".parse().unwrap());
        headers.insert("Content-Encoding", "gzip".parse().unwrap());

        let mut base_log = [LogEvent::from(value!({})).into()];
        add_headers(
            &mut base_log,
            &header_names,
            &headers,
            LogNamespace::Legacy,
            "test",
        );
        let mut namespaced_log = [LogEvent::from(value!({})).into()];
        add_headers(
            &mut namespaced_log,
            &header_names,
            &headers,
            LogNamespace::Vector,
            "test",
        );

        assert_eq!(
            base_log[0].as_log().value(),
            namespaced_log[0]
                .metadata()
                .value()
                .get(path!("test", "headers"))
                .unwrap()
                .clone()
        );
    }

    #[test]
    fn multiple_headers_wildcard() {
        let header_names = [HttpConfigParamKind::Glob(
            glob::Pattern::new("Content-*").unwrap(),
        )];
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", "application/x-protobuf".parse().unwrap());
        headers.insert("User-Agent", "Test".parse().unwrap());
        headers.insert("Content-Encoding", "gzip".parse().unwrap());

        let mut base_log = [LogEvent::from(value!({})).into()];
        add_headers(
            &mut base_log,
            &header_names,
            &headers,
            LogNamespace::Legacy,
            "test",
        );
        let mut namespaced_log = [LogEvent::from(value!({})).into()];
        add_headers(
            &mut namespaced_log,
            &header_names,
            &headers,
            LogNamespace::Vector,
            "test",
        );

        let log = base_log[0].as_log();
        assert_eq!(
            log.value(),
            namespaced_log[0]
                .metadata()
                .value()
                .get(path!("test", "headers"))
                .unwrap()
                .clone(),
            "Checking legacy and namespaced log contain headers string"
        );
        assert_eq!(
            log.get("content-type").unwrap(),
            "application/x-protobuf".into(),
            "Checking log contains Content-Type header"
        );
        assert!(
            !log.contains("user-agent"),
            "Checking log does not contain User-Agent header"
        );
        assert_eq!(
            log.get("content-encoding").unwrap(),
            "gzip".into(),
            "Checking log contains Content-Encoding header"
        );
    }
}
