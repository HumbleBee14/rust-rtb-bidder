//! End-to-end test: load `tests/fixtures/test_pctr_model.onnx` through ort,
//! run the parity check, and exercise `score_all` over a real batch.
//!
//! Skipped automatically when ORT_DYLIB_PATH is unset (fresh clone without
//! `make install-ort`). CI runs `source tools/setup-ort-env.sh` before tests.
//!
//! Lives as its own integration test binary so that ONNX Runtime's native
//! process-lifetime teardown doesn't interfere with other tests. On macOS,
//! ort 2.0.0-rc.10 emits a SIGABRT during process exit due to a mutex teardown
//! race in fsevent / dispatch_sync; this is a known upstream artifact and is
//! benign — assertions pass cleanly before the abort. On Linux production this
//! does not occur (different fsevent → inotify path, different teardown order).

use bidder_core::{
    model::candidate::AdCandidate,
    scoring::{MLScorer, MLScorerConfig, Scorer, ScoringContext},
};
use std::path::PathBuf;

fn fixture_paths() -> (PathBuf, PathBuf) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let repo_root = std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .to_path_buf();
    (
        repo_root.join("tests/fixtures/test_pctr_model.onnx"),
        repo_root.join("tests/fixtures/scoring_parity.jsonl"),
    )
}

fn cfg_or_skip() -> Option<MLScorerConfig> {
    if std::env::var("ORT_DYLIB_PATH").is_err() {
        eprintln!(
            "[ml_integration] ORT_DYLIB_PATH not set; skipping. \
             Run `make install-ort` then `source tools/setup-ort-env.sh`."
        );
        return None;
    }
    let (model_path, parity_path) = fixture_paths();
    if !model_path.exists() {
        eprintln!(
            "[ml_integration] {} missing; run `make regen-test-model`.",
            model_path.display()
        );
        return None;
    }
    Some(MLScorerConfig {
        model_path,
        parity_path: Some(parity_path),
        input_tensor_name: "features".to_string(),
        output_tensor_name: "pctr".to_string(),
        pool_size: 2,
        max_batch: 32,
        batch_pad_to: 8,
    })
}

#[tokio::test]
async fn loads_fixture_and_passes_parity_then_scores_batch() {
    let Some(cfg) = cfg_or_skip() else { return };
    let scorer = MLScorer::new(cfg).expect("MLScorer::new with valid fixture");

    let mut candidates = vec![
        AdCandidate {
            campaign_id: 1,
            creative_id: 1,
            bid_price_cents: 500,
            score: 0.0,
            daily_cap_imps: u32::MAX,
            hourly_cap_imps: u32::MAX,
        };
        5
    ];
    let ctx = ScoringContext {
        segment_ids: &[],
        device_type: None,
        ad_format: None,
        hour_of_day: 12,
        user_id: "",
        is_top_market: false,
    };
    scorer.score_all(&mut candidates, &ctx).await;
    for c in &candidates {
        assert!(
            c.score >= 0.0 && c.score <= 1.0,
            "ML score must be in [0, 1] for sigmoid output, got {}",
            c.score
        );
    }
}
