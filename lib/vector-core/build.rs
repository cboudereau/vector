fn main() {
    println!("cargo:rerun-if-changed=proto/event.proto");
    println!("cargo:rerun-if-changed=proto/otlp_buffer.proto");

    let _otel_proto_root = "../../lib/opentelemetry-proto/src/proto/opentelemetry-proto";

    prost_build::Config::new()
        .protoc_arg("--experimental_allow_proto3_optional")
        .btree_map(["."])
        .bytes(["raw_bytes"])
        .compile_protos(
            &["proto/event.proto"],
            &["proto", "../../proto/third-party", "../../proto/vector"],
        )
        .unwrap();

    // OtlpBufferBatch is implemented as a hand-written prost::Message in ser.rs
    // to avoid a circular dependency (opentelemetry-proto depends on vector-core).
}
