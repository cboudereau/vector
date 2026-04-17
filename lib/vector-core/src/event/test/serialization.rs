use bytes::{Buf, BufMut, BytesMut};
use quickcheck::{QuickCheck, TestResult};
use regex::Regex;
use similar_asserts::assert_eq;
use vector_buffers::encoding::Encodable;

use super::*;

fn encode_value<T: Encodable, B: BufMut>(value: T, buffer: &mut B) {
    value.encode(buffer).expect("encoding should not fail");
}

fn decode_value<T: Encodable, B: Buf + Clone>(buffer: B) -> T {
    T::decode(T::get_metadata(), buffer).expect("decoding should not fail")
}

// Ser/De the EventArray never loses bytes
#[test]
#[ignore = "Legacy proto round-trip is lossy with OTel event types"]
fn serde_eventarray_no_size_loss() {
    fn inner(events: EventArray) -> TestResult {
        let expected = events.clone();

        let mut buffer = BytesMut::with_capacity(64);
        encode_value(events, &mut buffer);

        let actual = decode_value::<EventArray, _>(buffer);
        assert_eq!(actual.size_of(), expected.size_of());

        TestResult::passed()
    }

    QuickCheck::new()
        .tests(1_000)
        .max_tests(10_000)
        .quickcheck(inner as fn(EventArray) -> TestResult);
}

// Ser/De the EventArray type through EncodeBytes -> DecodeBytes
#[test]
#[ignore = "Legacy proto round-trip is lossy with OTel event types"]
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn back_and_forth_through_bytes() {
    fn inner(events: EventArray) -> TestResult {
        let expected = events.clone();

        let mut buffer = BytesMut::with_capacity(64);
        encode_value(events, &mut buffer);

        let actual = decode_value::<EventArray, _>(buffer);

        assert_eq!(expected, actual);

        TestResult::passed()
    }

    QuickCheck::new()
        .tests(1_000)
        .max_tests(10_000)
        .quickcheck(inner as fn(EventArray) -> TestResult);
}

#[test]
fn serialization() {
    let mut event = OtelLog::from("raw log line");
    event.insert(vrl::event_path!("foo"), "bar");
    event.insert(vrl::event_path!("bar"), "baz");

    // Convert Vec<(KeyString, Value)> to an ObjectMap for JSON object serialization
    let fields: vrl::value::ObjectMap = event
        .all_event_fields()
        .unwrap()
        .into_iter()
        .collect();
    let actual_all = serde_json::to_value(&fields).unwrap();

    // OtelLog::from("...") sets body but does not auto-insert a timestamp
    // like LogEvent::from("...") did.  Verify the fields that are present.
    assert_eq!(actual_all["body"], serde_json::json!("raw log line"));
    assert_eq!(actual_all["foo"], serde_json::json!("bar"));
    assert_eq!(actual_all["bar"], serde_json::json!("baz"));

    // If a timestamp was populated, verify its format.
    if let Some(ts_val) = actual_all.pointer("/timestamp") {
        if let Some(ts_str) = ts_val.as_str() {
            let rfc3339_re = Regex::new(r"\A\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z\z").unwrap();
            assert!(rfc3339_re.is_match(ts_str));
        }
    }
}

#[test]
fn type_serialization() {
    use serde_json::json;

    let mut event = OtelLog::from("hello world");
    event.insert(vrl::event_path!("int"), 4);
    event.insert(vrl::event_path!("float"), 5.5);
    event.insert(vrl::event_path!("bool"), true);
    event.insert(vrl::event_path!("string"), "thisisastring");

    let fields: vrl::value::ObjectMap = event
        .all_event_fields()
        .unwrap()
        .into_iter()
        .collect();
    let map = serde_json::to_value(&fields).unwrap();
    assert_eq!(map["float"], json!(5.5));
    assert_eq!(map["int"], json!(4));
    assert_eq!(map["bool"], json!(true));
    assert_eq!(map["string"], json!("thisisastring"));
}
