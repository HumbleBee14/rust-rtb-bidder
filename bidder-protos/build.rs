fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().unwrap();
    std::env::set_var("PROTOC", protoc);
    prost_build::compile_protos(&["proto/events.proto", "proto/adx.proto"], &["proto/"]).unwrap();
}
