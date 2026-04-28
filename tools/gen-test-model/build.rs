use std::env;

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary present");
    env::set_var("PROTOC", &protoc);

    println!("cargo:rerun-if-changed=proto/onnx_minimal.proto");
    prost_build::compile_protos(&["proto/onnx_minimal.proto"], &["proto/"])
        .expect("compile onnx_minimal.proto");
}
