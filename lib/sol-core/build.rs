fn main() {
    println!("cargo:rerun-if-changed=proto/otlp_buffer.proto");
}
