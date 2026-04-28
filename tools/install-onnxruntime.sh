#!/usr/bin/env bash
# Bootstraps the ONNX Runtime dynamic library used by the bidder's MLScorer.
#
# The bidder links against ONNX Runtime via `ort` with the `load-dynamic` feature
# — there is no static binding, no download at cargo build time. Either the host
# already has libonnxruntime installed (system package, brew, container image),
# or this script vendors it into ./vendor/onnxruntime/<platform>/.
#
# Run once per fresh checkout (and re-run when ORT_VERSION below changes).
#
# Output: ./vendor/onnxruntime/<platform>/lib/libonnxruntime.{so,dylib}
# Set:    ORT_DYLIB_PATH to the absolute path of that file when launching the bidder
#         in dev. The Dockerfile sets it explicitly in the runtime stage.
set -euo pipefail

ORT_VERSION="1.22.0"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/vendor/onnxruntime"

uname_os="$(uname -s)"
uname_arch="$(uname -m)"

case "$uname_os" in
    Linux)
        case "$uname_arch" in
            x86_64)  platform="linux-x64";  archive_ext="tgz" ;;
            aarch64) platform="linux-aarch64"; archive_ext="tgz" ;;
            *) echo "Unsupported Linux arch: $uname_arch"; exit 1 ;;
        esac
        ;;
    Darwin)
        case "$uname_arch" in
            arm64)  platform="osx-arm64"; archive_ext="tgz" ;;
            x86_64) platform="osx-x86_64"; archive_ext="tgz" ;;
            *) echo "Unsupported macOS arch: $uname_arch"; exit 1 ;;
        esac
        ;;
    *) echo "Unsupported OS: $uname_os"; exit 1 ;;
esac

archive_name="onnxruntime-${platform}-${ORT_VERSION}.${archive_ext}"
download_url="https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/${archive_name}"
target_dir="$VENDOR_DIR/$platform"

if [ -d "$target_dir/lib" ] && ls "$target_dir"/lib/libonnxruntime.* >/dev/null 2>&1; then
    echo "ONNX Runtime already vendored at $target_dir/lib"
    exit 0
fi

echo "Downloading ONNX Runtime $ORT_VERSION for $platform..."
mkdir -p "$VENDOR_DIR"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl -fsSL --retry 3 -o "$tmp_dir/$archive_name" "$download_url"
tar -xzf "$tmp_dir/$archive_name" -C "$tmp_dir"

extracted_dir="$(find "$tmp_dir" -maxdepth 1 -mindepth 1 -type d | head -n1)"
if [ -z "$extracted_dir" ]; then
    echo "Tarball extraction produced no directory"
    exit 1
fi

mkdir -p "$target_dir"
mv "$extracted_dir/lib" "$target_dir/lib"
mv "$extracted_dir/include" "$target_dir/include" 2>/dev/null || true

lib_file="$(ls "$target_dir/lib"/libonnxruntime.* 2>/dev/null | head -n1)"
if [ -z "$lib_file" ]; then
    echo "Expected libonnxruntime.* in $target_dir/lib but found none"
    exit 1
fi

echo
echo "ONNX Runtime $ORT_VERSION installed at: $target_dir/lib"
echo
echo "When launching the bidder in dev, set:"
echo "  export ORT_DYLIB_PATH=\"$lib_file\""
echo
echo "The production Dockerfile sets this automatically. CI sources tools/setup-ort-env.sh."
