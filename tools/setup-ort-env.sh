#!/usr/bin/env bash
# Source this from a shell or CI step to point ORT_DYLIB_PATH at the vendored
# ONNX Runtime library. Idempotent. Required before running tests / launching
# the bidder in dev unless the system has libonnxruntime installed globally
# (Linux: apt/yum, macOS: brew install onnxruntime).
#
# Usage:
#   source tools/setup-ort-env.sh
#
# Exit status is irrelevant when sourced; non-fatal warning if not yet installed.
ORT_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
ORT_VENDOR_DIR="$ORT_REPO_ROOT/vendor/onnxruntime"

uname_os="$(uname -s)"
uname_arch="$(uname -m)"
case "$uname_os" in
    Linux)
        case "$uname_arch" in
            x86_64)  ort_platform="linux-x64" ;;
            aarch64) ort_platform="linux-aarch64" ;;
            *) ort_platform="" ;;
        esac
        ort_libname="libonnxruntime.so"
        ;;
    Darwin)
        case "$uname_arch" in
            arm64)  ort_platform="osx-arm64" ;;
            x86_64) ort_platform="osx-x86_64" ;;
            *) ort_platform="" ;;
        esac
        ort_libname="libonnxruntime.dylib"
        ;;
    *) ort_platform="" ;;
esac

if [ -z "$ort_platform" ]; then
    echo "[setup-ort-env] Unsupported platform $uname_os/$uname_arch" 1>&2
    return 1 2>/dev/null || exit 1
fi

ort_lib_dir="$ORT_VENDOR_DIR/$ort_platform/lib"
ort_lib_path="$(ls "$ort_lib_dir"/${ort_libname}* 2>/dev/null | head -n1)"

if [ -z "$ort_lib_path" ]; then
    echo "[setup-ort-env] No libonnxruntime found at $ort_lib_dir" 1>&2
    echo "[setup-ort-env] Run: bash tools/install-onnxruntime.sh" 1>&2
    return 1 2>/dev/null || exit 1
fi

export ORT_DYLIB_PATH="$ort_lib_path"
echo "[setup-ort-env] ORT_DYLIB_PATH=$ORT_DYLIB_PATH"
