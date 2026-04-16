use std::collections::HashMap;

use vector_lib::{
    codecs::{JsonSerializerConfig, TextSerializerConfig},
    event::{Event, OtelLog, Metric, MetricKind, MetricValue, OtelMetric},
    request_metadata::GroupedCountByteSize,
};

use super::{config::RedisSinkConfig, request_builder::encode_event};
use crate::{
    codecs::{Encoder, Transformer},
    config::log_schema,
};

#[test]
fn generate_config() {
    crate::test_util::test_generate_config::<RedisSinkConfig>();
}

#[test]
fn redis_log_event_json() {
    let msg = "hello_world".to_owned();
    let mut byte_size = GroupedCountByteSize::new_untagged();
    let mut evt = OtelLog::from(msg.clone());
    evt.insert("key", "value");
    let result = encode_event(
        evt.into(),
        "key".to_string(),
        None,
        &Default::default(),
        &mut Encoder::<()>::new(JsonSerializerConfig::default().build().into()),
        &mut byte_size,
    )
    .unwrap()
    .value;
    let map: HashMap<String, String> = serde_json::from_slice(&result[..]).unwrap();
    assert_eq!(msg, map[&log_schema().message_key().unwrap().to_string()]);
}

#[test]
fn redis_log_event_text() {
    let msg = "hello_world".to_owned();
    let evt = OtelLog::from(msg.clone());
    let mut byte_size = GroupedCountByteSize::new_untagged();
    let event = encode_event(
        evt.into(),
        "key".to_string(),
        None,
        &Default::default(),
        &mut Encoder::<()>::new(TextSerializerConfig::default().build().into()),
        &mut byte_size,
    )
    .unwrap()
    .value;
    assert_eq!(event, Vec::from(msg));
}

#[test]
fn redis_log_encode_event() {
    let msg = "hello_world";
    let mut evt = OtelLog::from(msg);
    let mut byte_size = GroupedCountByteSize::new_untagged();
    evt.insert("key", "value");

    let result = encode_event(
        evt.into(),
        "key".to_string(),
        None,
        &Transformer::new(None, Some(vec!["key".into()]), None).unwrap(),
        &mut Encoder::<()>::new(JsonSerializerConfig::default().build().into()),
        &mut byte_size,
    )
    .unwrap()
    .value;

    let map: HashMap<String, String> = serde_json::from_slice(&result[..]).unwrap();
    assert!(!map.contains_key("key"));
}

#[test]
fn redis_metric_encode_event() {
    let mut byte_size = GroupedCountByteSize::new_untagged();
    let metric = Metric::new(
        "test_counter",
        MetricKind::Absolute,
        MetricValue::Counter { value: 42.0 },
    );

    let result = encode_event(
        Event::Metric(OtelMetric::from_legacy_metric(metric)),
        "metrics.counter".to_string(),
        None,
        &Default::default(),
        &mut Encoder::<()>::new(JsonSerializerConfig::default().build().into()),
        &mut byte_size,
    )
    .unwrap()
    .value;

    let json: serde_json::Value = serde_json::from_slice(&result).unwrap();

    assert_eq!(json["name"], "test_counter");
    // OTLP-native JSON format: counter is Sum with isMonotonic=true
    assert!(json["sum"].is_object(), "expected sum field in OTLP format");
    assert_eq!(json["sum"]["dataPoints"][0]["asDouble"], 42.0);
}

#[test]
fn redis_log_scoring() {
    let msg = "hello_world";
    let mut evt = OtelLog::from(msg);
    let mut byte_size = GroupedCountByteSize::new_untagged();
    evt.insert("key", "value");

    let result = encode_event(
        evt.into(),
        "key".to_string(),
        Some(64),
        &Default::default(),
        &mut Encoder::<()>::new(JsonSerializerConfig::default().build().into()),
        &mut byte_size,
    )
    .unwrap()
    .score;

    assert_eq!(result, Some(64));
}
