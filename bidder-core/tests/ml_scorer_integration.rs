//! End-to-end test: load `tests/fixtures/test_pctr_model.onnx` through ort,
//! run the parity check, and exercise `score_all` over a real batch.
//!
//! Skipped automatically when `ORT_DYLIB_PATH` is unset (fresh clone without
//! `make install-ort`). CI runs `source tools/setup-ort-env.sh` before tests.
//!
//! ## Custom test harness — why
//!
//! This test binary is declared with `harness = false` in `bidder-core/Cargo.toml`,
//! which means we own `main()` instead of inheriting libtest's runtime. The
//! reason is a macOS-only ONNX Runtime teardown race:
//!
//! `ort 2.0.0-rc.10`'s C++ static destructors fire at process exit and lock a
//! `libdispatch` (Apple GCD) mutex that has already been torn down. Result is
//! a SIGABRT *after* every assertion has passed. Cargo treats SIGABRT as
//! "test binary failed" even when the tests inside it succeeded.
//!
//! Workarounds we tried that failed:
//!   - `atexit(fast_exit)` — fires AFTER C++ destructors on macOS, too late.
//!   - `_exit(0)` from a Drop guard inside a libtest test fn — runs before
//!     libtest's summary printer, suppressing test output.
//!
//! The custom harness is the clean fix:
//!   1. Print test name, run the test body inside `catch_unwind`.
//!   2. Print pass/fail line.
//!   3. Flush stdio.
//!   4. On macOS: `libc::_exit(code)` — skips C++ destructors, no SIGABRT.
//!   5. On Linux: `std::process::exit(code)` — normal teardown, destructors
//!      run cleanly there (the race is macOS-specific to libdispatch).
//!
//! Production bidder binaries never reach this code path — they exit via
//! SIGTERM from the orchestrator, not normal end-of-process teardown.
//!
//! Adding more tests: append a `run_test("name", run_<name>)` line to `main()`.

use bidder_core::{
    model::candidate::AdCandidate,
    scoring::{MLScorer, MLScorerConfig, Scorer, ScoringContext},
};
use std::io::Write;
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

/// Test body: load ONNX, parity-check, score a batch. Panics on failure —
/// `main` catches and converts to a non-zero exit.
fn run_loads_fixture_and_passes_parity_then_scores_batch() {
    let Some(cfg) = cfg_or_skip() else {
        // Treat skip as success so a fresh checkout doesn't fail CI before
        // bootstrap. The eprintln in cfg_or_skip explains why.
        return;
    };
    // tokio runtime managed manually since we don't have #[tokio::test].
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    rt.block_on(async {
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
    });
}

/// Run one test, print libtest-style result line, return whether it passed.
fn run_test(name: &str, body: fn()) -> bool {
    print!("test {} ... ", name);
    let _ = std::io::stdout().flush();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    match result {
        Ok(()) => {
            println!("ok");
            true
        }
        Err(payload) => {
            println!("FAILED");
            // Surface the panic message if it's a string.
            if let Some(s) = payload.downcast_ref::<&str>() {
                eprintln!("    panic: {}", s);
            } else if let Some(s) = payload.downcast_ref::<String>() {
                eprintln!("    panic: {}", s);
            }
            false
        }
    }
}

fn fast_exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // macOS: skip C++ static destructors via _exit to dodge the
    // ort/libdispatch teardown race. Status reflects test outcome.
    #[cfg(target_os = "macos")]
    unsafe {
        libc::_exit(code);
    }

    // Linux + others: standard exit. Destructors run cleanly.
    #[cfg(not(target_os = "macos"))]
    {
        std::process::exit(code);
    }
}

fn main() {
    println!();
    println!("running ml_scorer_integration tests");

    let mut passed = 0u32;
    let mut failed = 0u32;

    // Add tests by appending to this list.
    let tests: &[(&str, fn())] = &[(
        "loads_fixture_and_passes_parity_then_scores_batch",
        run_loads_fixture_and_passes_parity_then_scores_batch,
    )];

    for (name, body) in tests {
        if run_test(name, *body) {
            passed += 1;
        } else {
            failed += 1;
        }
    }

    println!();
    if failed == 0 {
        println!(
            "test result: ok. {} passed; {} failed; 0 ignored; 0 measured",
            passed, failed
        );
    } else {
        println!(
            "test result: FAILED. {} passed; {} failed; 0 ignored; 0 measured",
            passed, failed
        );
    }

    let exit_code = if failed == 0 { 0 } else { 1 };
    fast_exit(exit_code);
}
