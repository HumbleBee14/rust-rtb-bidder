.PHONY: regen-test-model install-ort dev-env

# Regenerate the synthetic ONNX test fixture and its parity JSONL. The output
# files are committed to the repo; this target is for when the generator's
# constants change, not part of the build.
regen-test-model:
	cargo run -p gen-test-model -- tests/fixtures/test_pctr_model.onnx

# Vendor ONNX Runtime native lib into ./vendor/onnxruntime/<platform>/.
# Run once per fresh checkout. Idempotent.
install-ort:
	bash tools/install-onnxruntime.sh

# Print the ORT_DYLIB_PATH export for the current platform. Eval to apply:
#   eval $(make dev-env)
dev-env:
	@bash tools/setup-ort-env.sh
