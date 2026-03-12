use std::{io::Result, path::PathBuf};

use glob::glob;

fn main() -> Result<()> {
    let proto_root = PathBuf::from("src/proto/opentelemetry-proto");
    let include_path = proto_root.clone();

    let proto_paths: Vec<_> = glob(&format!("{}/**/*.proto", proto_root.display()))
        .expect("Failed to read glob pattern")
        .filter_map(|result| result.ok())
        .collect();

    tonic_build::configure()
        .build_client(false)
        .build_server(false)
        .compile(&proto_paths, &[include_path])?;

    Ok(())
}
