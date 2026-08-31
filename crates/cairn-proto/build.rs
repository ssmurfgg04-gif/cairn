fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protoc is vendored (no system dependency, see docs: protoc unavailable in build env)
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // PROTOC must be set before prost-build reads the env; std marks set_var unsafe on newer
// toolchains — this is a build-script-only, single-threaded mutation (safe in practice).
#[allow(unsafe_code)]
unsafe {
    std::env::set_var("PROTOC", protoc);
};

    let proto = "../../proto/cairn/v4/cairn.proto";
    let include = "../../proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[include])?;
    Ok(())
}
