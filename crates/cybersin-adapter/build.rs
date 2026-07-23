fn main() {
    // Use a vendored protoc binary so `cargo build` works with no
    // separately-installed C toolchain / system protoc step (mirrors the
    // static-linkable bar the spec sets for sqlite-vec at §15.5).
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/adapter.proto"], &["proto"])
        .expect("compile adapter.proto");
}
