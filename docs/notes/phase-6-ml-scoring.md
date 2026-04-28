# Phase 6 — ML scoring + cascade + win-notice hardening

## Goal

Three deliverables, in priority order:

1. **`MLScorer`** — production-shaped ONNX inference with hot-reload, session pooling, batched inputs, and a boot-time parity check.
2. **`CascadeScorer` + `ABTestScorer`** — config-driven scorer composition: cheap stage 1 over all candidates, expensive stage 2 over the top-K survivors; A/B routing for safe model rollouts.
3. **Phase 5 spillover finished** — per-campaign frequency-cap limits read from the catalog (replacing the hardcoded `day=10, hour=3`); win-notice HMAC authentication + Redis `SET NX EX` deduplication (replacing the trust-based `/rtb/win` endpoint).

The phase intentionally lands *before* the data-science integration is final. The bidder runs against working defaults; the contract that locks in the real model shape lives in `docs/SCORING-FEATURES.md`. When the DS team commits the schema, the diff to update the bidder is mechanical: feature struct field order, transform constants, threshold value.

---

## Architecture overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Pipeline                                                                    │
│                                                                              │
│   RequestValidation → UserEnrichment → CandidateRetrieval                   │
│                                                                              │
│   CandidateLimit ──────────► top_k by floor price                           │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  ScoringStage                                                      │    │
│   │     ├─ extract ScoringContext from request once                    │    │
│   │     │  (segments, device, format, hour, user_id, top-market flag)  │    │
│   │     │                                                              │    │
│   │     └─ Scorer::score_all(candidates, &ctx)                         │    │
│   │                                                                    │    │
│   │   Scorer is built recursively from [scoring] config:               │    │
│   │                                                                    │    │
│   │     kind=feature_weighted   ─► FeatureWeightedScorer (cheap)       │    │
│   │     kind=ml                 ─► MLScorer (ONNX via ort)             │    │
│   │     kind=cascade            ─► CascadeScorer { stage1, stage2 }    │    │
│   │     kind=ab_test            ─► ABTestScorer  { control, treatment }│    │
│   │                                                                    │    │
│   │   Decorators (cascade, ab_test) accept only LEAF scorers — one     │    │
│   │   level of nesting; rejects cascade-of-cascade for sanity.         │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   FreqCap reads c.daily_cap_imps / c.hourly_cap_imps PER CANDIDATE          │
│   (loaded from postgres campaign.daily_cap_imps / hourly_cap_imps,          │
│    populated into AdCandidate at retrieval time).                           │
│                                                                              │
│   BudgetPacing → Ranking → ResponseBuild                                     │
│                                                                              │
│   ResponseBuild calls notice_url_builder.build(...) — bidder-server         │
│   supplies an HMAC-signing impl (WinNoticeGateService) that computes        │
│   token=HMAC-SHA256(request_id|imp_id|campaign_id|creative_id|              │
│   clearing_price_micros, secret) and embeds it in the nurl.                 │
└─────────────────────────────────────────────────────────────────────────────┘

  /rtb/win handler (bidder-server)
   1. WinNoticeGateService::check(...)
        ├─ verify HMAC token (constant-time compare)
        └─ SET NX EX v1:winx:<request_id>:<imp_id>  (dedup gate)
   2. record impression freq-cap counter (only if user_id is non-empty)
   3. publish WinEvent to Kafka (fire-and-forget)
   4. return 200 (also returned on duplicate — never 4xx so SSPs don't retry)
```

---

## New modules and files

| Path | What it is |
|---|---|
| `bidder-core/src/scoring/features.rs` | `ScoringFeatures` typed struct + `extract` + `pack_into`. The single place feature transforms live. |
| `bidder-core/src/scoring/ml.rs` | `MLScorer`, `MLScorerConfig`, session pool with `ArcSwap` hot-reload, `notify::PollWatcher`, boot-time parity check. |
| `bidder-core/src/scoring/cascade.rs` | `CascadeScorer` decorator. |
| `bidder-core/src/scoring/ab_test.rs` | `ABTestScorer` decorator. FNV-1a-hashed user-id assignment. |
| `bidder-core/src/notice.rs` | `NoticeUrlBuilder` trait + `NoNoticeUrl` default. Lives in core so `ResponseBuildStage` doesn't depend on HMAC crates. |
| `bidder-core/tests/ml_scorer_integration.rs` | End-to-end test loading the synthetic ONNX through ort and running parity. |
| `bidder-server/src/win_notice.rs` | `WinNoticeGateService` — HMAC sign/verify, Redis `SET NX EX` dedup, `NoticeUrlBuilder` impl. |
| `tools/gen-test-model/` | Standalone Rust crate that emits the synthetic ONNX fixture and matching parity JSONL. No external Python/onnx tooling. |
| `tools/gen-test-model/proto/onnx_minimal.proto` | Vendored subset of the public ONNX schema (wire-compatible). |
| `tools/install-onnxruntime.sh` | Bootstraps `libonnxruntime` into `vendor/onnxruntime/<platform>/`. |
| `tools/setup-ort-env.sh` | Sources `ORT_DYLIB_PATH` for dev/CI shells. |
| `Makefile` | `regen-test-model`, `install-ort`, `dev-env` targets. |
| `migrations/0002_freq_cap_limits.sql` | Adds `daily_cap_imps`, `hourly_cap_imps` columns to `campaign`. |
| `docs/SCORING-FEATURES.md` | Phase-0-style contract doc. Locked-in version is the integration handoff to the DS team. |

---

## Config additions (`config.toml`)

```toml
[scoring]
kind = "feature_weighted"  # or "ml" | "cascade" | "ab_test"

[scoring.ml]
model_path = "tests/fixtures/test_pctr_model.onnx"
parity_path = "tests/fixtures/scoring_parity.jsonl"
input_tensor_name = "features"
output_tensor_name = "pctr"
pool_size = 2
max_batch = 64
batch_pad_to = 8

[scoring.cascade]
stage1 = "feature_weighted"
stage2 = "ml"
top_k = 50
threshold = 0.0

[scoring.ab_test]
control = "feature_weighted"
treatment = "ml"
treatment_share = 0.10
hash_seed = "phase-6-rollout"

[win_notice]
require_auth = true
secret = ""           # set via BIDDER__WIN_NOTICE__SECRET in production
dedup_ttl_secs = 3600
notice_base_url = ""  # https://bid.example.com/rtb/win in production
```

---

## ONNX Runtime — host contract

The bidder uses `ort 2.0.0-rc.10` with the `load-dynamic` feature. There is no static linking and no build-time download of native libraries.

**Production:** the deploy image (or host) provides `libonnxruntime` (Debian/Ubuntu: `libonnxruntime-dev` package once available; otherwise pulled by `tools/install-onnxruntime.sh` in the Dockerfile build stage). `ORT_DYLIB_PATH` is exported to the bidder process pointing at the absolute path of the `.so`.

**Local dev / CI:** run `make install-ort` once per fresh checkout. The script downloads the ONNX Runtime release tarball into `vendor/onnxruntime/<platform>/` and prints the export command. `tools/setup-ort-env.sh` does the export from a sourced shell. CI sources it before running `cargo test`.

**Pinned version:** ONNX Runtime `1.22.x`. The bidder's `ort` crate version requires this; the bootstrap script enforces it. Mismatches surface at `MLScorer::new` with a clear panic message from ort itself.

---

## Self-contained test fixture — design choice

`tools/gen-test-model/` is a Rust binary that emits a real, ort-loadable ONNX model from raw protobuf (via `prost`). It does NOT shell out to `python`, `onnx`, or `pip install` anything. It's a regen tool, not a build step — the generated `.onnx` is committed to `tests/fixtures/`.

The generator vendors a minimal subset of the ONNX 1.18 schema in `proto/onnx_minimal.proto`. The wire format is byte-compatible with the full schema; readers (ort, netron, onnxruntime CLI) accept the output without modification.

The fixture model is `pctr = sigmoid(W · features + b)` with 13 hand-picked weights and a fixed bias. The same constants are used to compute the boot-time parity JSONL — input/output pairs that prove the bidder's feature transform pipeline matches the model. If you change a weight, you regenerate the model AND the parity file in one shot via `make regen-test-model`.

A real production model is dropped in via `config.toml`'s `[scoring.ml] model_path`; the bidder's loader code path is identical.

---

## Hot-reload — design choice

`MLScorer` watches the model file via `notify::PollWatcher` (5s poll interval). When the file changes, the watcher rebuilds the session pool and atomically swaps it in via `ArcSwap`.

**Why PollWatcher and not the recommended fsevent/inotify backend?** macOS test teardown was hitting a `mutex lock failed` SIGABRT in fsevent's native cleanup. `PollWatcher` has no native threads; it's a tokio task that polls file mtimes. The 5s reload latency vs. instant fsevent is irrelevant for a workload that updates the model file once a day at most. The operational benefit (clean shutdown, portable, no native-thread surprises) is worth the latency. Revisit if model rollout cadence ever needs sub-5s.

The session pool is `Vec<tokio::sync::Mutex<Session>>` — each `Session::run` requires `&mut self` per ort's API. We round-robin via an atomic counter and lock per call. Pool size = `num_cpus / 2` typical; configurable per deployment.

---

## Boot-time parity check — the production-correctness lever

Before `MLScorer::new` returns, it loads `parity_path` (a JSONL of `{input, expected_score}` pairs), runs each through the just-loaded ONNX session, and refuses to start if any drift > 1e-4.

**Why this matters:** the most common ML-bidder bug in production is silent feature-pipeline divergence — training computes `is_business_hours` in UTC, bidder computes in local time, scores skew, no alarm. The parity check catches this at boot, before any traffic.

The parity JSONL is generated by `tools/gen-test-model` for the synthetic model; for production models, the DS team's training pipeline emits it alongside the `.onnx` file. Format spec: `docs/SCORING-FEATURES.md` § 11.

---

## Win-notice hardening (Phase 5 spillover)

### HMAC-SHA256 token

Every bid response now embeds a `nurl` of the form:

```
https://<base>/rtb/win?request_id=...&imp_id=...&campaign_id=...&creative_id=...
                       &clearing_price_micros=...&user_id=...&token=<hex>
```

`token = HMAC-SHA256(request_id|imp_id|campaign_id|creative_id|clearing_price_micros, secret)`. The handler recomputes and constant-time-compares (via `subtle::ConstantTimeEq`). Bad token → 401, no Redis, no Kafka.

The secret is loaded from `BIDDER__WIN_NOTICE__SECRET` env var. Empty + `require_auth=true` logs a loud warning at startup and accepts all tokens (degenerate config, intended only for early-stage SSP integration).

### `SET NX EX` dedup

Before any side effect, the handler runs `SET v1:winx:<request_id>:<imp_id> 1 NX EX <ttl>` against Redis. First write wins (returns OK); duplicates return nil and the handler 200s without recording the impression or publishing.

Why 1h TTL by default: matches the shortest freq-cap window. A duplicate notice received within the cap window can never bypass dedup and double-increment the counter inside the same window.

A 50ms timeout on the SET-NX call fails-closed: Redis blip → treat as duplicate → no double-counting. Redis must be very degraded for the win-notice to time out, and at that point the bid path is already shedding load anyway.

### Why GET stays GET

Real SSPs fire `nurl` as a simple GET — `nurl` is a notification, not an API. Switching to POST would break SSP integration. Idempotency is preserved by the dedup gate, not by HTTP verb.

---

## Per-campaign freq-cap (Phase 5 spillover)

Migration `0002_freq_cap_limits.sql` adds `daily_cap_imps INTEGER NOT NULL DEFAULT 10` and `hourly_cap_imps INTEGER NOT NULL DEFAULT 3` to `campaign`. Defaults match the prior hardcoded values exactly — no behaviour change until campaigns set their own limits.

`Campaign` struct, catalog query, and `AdCandidate` carry the cap fields through the pipeline. `RedisFrequencyCapper` reads `c.daily_cap_imps` / `c.hourly_cap_imps` per candidate instead of `>=10 || >=3` hardcoded.

A cap of `0` means "block after first impression" — explicit sentinel rather than a separate boolean.

---

## What is NOT in Phase 6

- **Real per-campaign segment overlap.** `ScoringFeatures::extract` still computes overlap from `campaign_id % 10` as a stable placeholder. Real overlap requires threading `catalog.segment_to_campaigns` into the scorer; deferred to the segment-features pass once DS confirms the schema.
- **`is_weekend` and `geo_top_market` features.** Both default to 0/false because the `ScoringContext` doesn't yet carry day-of-week or a top-market lookup. Wired when DS confirms the values matter; trivial 5-line change.
- **Multi-output models.** `MLScorer` reads a single `[N]` or `[N, 1]` tensor. Multi-objective output (e.g. pCTR + variance) requires a small loader extension.
- **Per-tenant HMAC secrets.** v1 uses one shared secret. Per-SSP / per-tenant secrets are deferred to Phase 7 (multi-exchange phase).
- **Real feature freshness tracking.** Stale-feature handling is policy-only; no per-request "feature_age" telemetry yet. Add when DS supplies the freshness contract (`SCORING-FEATURES.md` § 7).

---

## Rust-specific decisions and surprises

### macOS teardown — `ort` static destructors race with libdispatch
`ort 2.0.0-rc.10`'s C++ static destructors fire at process exit and try to lock a `libdispatch` (Apple GCD) mutex that has already been torn down. Result is a SIGABRT *after* every assertion has passed, which cargo treats as a test-binary failure. Linux is unaffected.

Workarounds we evaluated:

| Approach | Outcome |
|---|---|
| `atexit(fast_exit)` | Fires AFTER C++ destructors on macOS — too late. |
| `_exit(0)` from a Drop guard inside a `#[tokio::test]` | Runs before libtest's summary printer — suppresses test output. |
| **Custom test harness (`harness = false`)** — own `main()`, run tests in `catch_unwind`, print our own pass/fail line, flush stdio, then `libc::_exit(code)` on macOS / `std::process::exit(code)` on Linux | **Adopted.** Test output prints normally, no SIGABRT, exit code reflects pass/fail. ~30 lines in `bidder-core/tests/ml_scorer_integration.rs`. |

The harness is a normal Cargo feature (criterion uses it; lots of FFI test crates use it). Production binaries never reach this code path — they exit via SIGTERM from the orchestrator, not normal end-of-process teardown.

Adding more ML integration tests: append a `("name", run_fn)` tuple to the `tests` slice in `main()`. No further harness work.

### `notify::PollWatcher` over `recommended_watcher`
Detailed above. Tradeoff: 5s reload latency in exchange for clean lifecycle and no native-thread teardown bugs.

### `tokio::sync::Mutex<Session>` pool, not `Arc<Session>`
`Session::run` takes `&mut self` because ort caches `IoBinding` state internally. Pool of mutexed sessions is the standard ort production pattern; pool_size is the parallel-inference cap.

### `ScoringFeatures` is an explicit struct, not a `[f32; 13]`
The cost is 13 named fields and a `pack_into` method. The benefit is that *every* feature-transform bug becomes a compile error or a unit-test failure rather than a silent score corruption. Worth it.

### Workspace layout
`tools/gen-test-model/` is a workspace member with its own `[[bin]]` so `cargo build -p gen-test-model` works, and the generated ONNX-protobuf code lives in `OUT_DIR` (no committed generated file to drift). `tools/` is the convention for codegen; `bidder-core` and `bidder-server` are the runtime artifacts.

### `notice_url_builder` is a trait in `bidder-core`
HMAC and percent-encoding live in `bidder-server`. `bidder-core::ResponseBuildStage` accepts `Arc<dyn NoticeUrlBuilder>` and calls it. Clean separation: core knows nothing about HMAC; the server can swap the impl per-SSP later without touching the pipeline.
