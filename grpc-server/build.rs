fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = &["proto/content.proto"];
    let include_dirs = &["proto"];
    let out_dir = std::env::var("OUT_DIR").unwrap();

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .file_descriptor_set_path(format!("{}/content_descriptor.bin", out_dir))
        .compile_protos(proto_files, include_dirs)?;

    Ok(())
}
