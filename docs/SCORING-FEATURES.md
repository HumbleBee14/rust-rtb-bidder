# Scoring — Feature Contract (DRAFT)

**Status:** Phase 6 working draft. Sections marked **TBD-DS** are placeholders that the data science team must lock down before the production model ships. Until they do, the bidder runs on the documented `bidder default` values, clearly tagged.

This is the integration contract between the model training pipeline and the bidder's `MLScorer`. Every disagreement between training and serving — feature order, normalization, missing-value sentinel, calibration baseline — silently corrupts bid quality without any error signal. Lock everything in this doc, version it, and use it as the authoritative source on both sides.

---

## 1. Objective and label

**What does the model predict?**

| Field | Value |
|---|---|
| Output type | **TBD-DS** (e.g. pCTR, pCVR, eCPM, blended utility) |
| Bidder default | pCTR (probability of click within 30 minutes of impression) |
| Score range | **TBD-DS** (sigmoid 0–1 / log-odds / raw logit / calibrated probability) |
| Bidder default | sigmoid 0–1 |
| Higher = better? | Yes |

**Label used in training:**

| Field | Value |
|---|---|
| Positive label | **TBD-DS** (e.g. clicked, converted, viewable-for-50%-1s) |
| Attribution window | **TBD-DS** |
| Negative sampling | **TBD-DS** (downsampled / full negatives / hard-mined) |

**Calibration:**

| Field | Value |
|---|---|
| Calibrated against | **TBD-DS** (production traffic baseline / held-out test set / uncalibrated) |
| Calibration method | **TBD-DS** (isotonic regression / Platt scaling / none) |
| Calibration drift policy | **TBD-DS** (monitored how / alert at what threshold) |

What the bidder does with the score (locked):

- The configured `Scorer` returns a per-candidate score in `AdCandidate.score`.
- `RankingStage` picks the highest-scoring candidate per impression.
- `BudgetPacingStage` may multiply the score by a pacing factor before ranking (separate concern).
- The score is **not** treated as a literal expected revenue — it's a relative ranking signal within a single bid request. Cross-request comparisons are explicitly out of scope.

---

## 2. Feature schema (the artifact)

**Required deliverable from DS team:** a versioned `features.schema.json` artifact that the training pipeline emits and the bidder loads at startup. The bidder fails-fast at boot if the loaded ONNX model's expected input shape doesn't match this schema.

```json
{
  "schema_version": "1.0.0",
  "model_input_name": "features",
  "model_output_name": "pctr",
  "feature_count": 13,
  "features": [
    {"index": 0, "name": "bid_price_norm",            "dtype": "f32", "transform": "minmax_train",  "missing_default": 0.0},
    {"index": 1, "name": "segment_overlap_ratio",     "dtype": "f32", "transform": "identity",      "missing_default": 0.0},
    {"index": 2, "name": "segment_count_log1p",       "dtype": "f32", "transform": "log1p",         "missing_default": 0.0},
    {"index": 3, "name": "device_desktop",            "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 4, "name": "device_mobile",             "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 5, "name": "device_tablet",             "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 6, "name": "device_ctv",                "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 7, "name": "hour_of_day_norm",          "dtype": "f32", "transform": "minmax_train",  "missing_default": 0.5},
    {"index": 8, "name": "is_weekend",                "dtype": "f32", "transform": "indicator",     "missing_default": 0.0},
    {"index": 9, "name": "geo_top_market",            "dtype": "f32", "transform": "indicator",     "missing_default": 0.0},
    {"index": 10, "name": "format_banner",            "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 11, "name": "format_video",             "dtype": "f32", "transform": "onehot",        "missing_default": 0.0},
    {"index": 12, "name": "format_native",            "dtype": "f32", "transform": "onehot",        "missing_default": 0.0}
  ]
}
```

**TBD-DS:** confirm or replace. The bidder default schema above is the one the bidder builds against today. Every feature added or removed requires a coordinated schema-version bump on both sides.

### 2.1 Feature pipeline ownership

Question to DS: **does the ONNX graph include preprocessing (normalization, one-hot expansion, embedding lookups) baked in, or is the bidder responsible?**

| Field | Value |
|---|---|
| Preprocessing baked into ONNX? | **TBD-DS** |
| Bidder default | No — bidder applies the transforms listed in `features[].transform` before invoking the model |
| If baked in, raw feature schema link | **TBD-DS** |

### 2.2 Per-feature transforms

Definitions for the `transform` values in the schema:

| Transform | Computation | Notes |
|---|---|---|
| `identity` | value as-is | f32 in `[0, 1]` already; bidder must confirm before passing |
| `minmax_train` | `clip((x - min_train) / (max_train - min_train), 0, 1)` | `min_train`/`max_train` constants must be in the schema; **TBD-DS** to provide |
| `log1p` | `log1p(max(x, 0))` | Then min-max if upstream training applied it; check |
| `onehot` | feature is the indicator for one category from a categorical field | Bidder produces multiple features from one source |
| `indicator` | 0 or 1 boolean | e.g. is_weekend, geo_top_market |

The bidder applies the transform listed per feature and refuses to bid (drops to FeatureWeighted scorer) if any required transform constant (`min_train`, `max_train`) is missing from the schema.

### 2.3 Categorical encoding

For categorical fields encoded by the bidder:

| Field | Cardinality | Encoding | Hash family if applicable |
|---|---|---|---|
| `device_type` | 5 (desktop/mobile/tablet/ctv/other) | one-hot, 4 features (other = all-zeros) | n/a |
| `ad_format` | 4 (banner/video/native/audio) | one-hot, 3 features (audio = all-zeros) | n/a |
| `geo` | high — top-market only | indicator: top-10 metros = 1, else 0 | n/a |
| Future high-cardinality fields | **TBD-DS** | **TBD-DS** | If hash-bucket, name the hash and bucket count |

Bidder default for new high-cardinality categoricals (e.g. app bundle, exchange seat): hash-bucket via `xxhash3 % 1024`. **DS team to confirm if/when these are added.**

### 2.4 Cross features

Question to DS: **does the model compute crosses internally (DeepFM / wide-and-deep), or are pre-computed cross features expected at the input?**

| Field | Value |
|---|---|
| Model handles crosses internally? | **TBD-DS** |
| Bidder default | Yes — no pre-computed crosses; model is responsible |
| If pre-computed, list of crosses | **TBD-DS** |

---

## 3. User-history features

If the model uses any feature that requires looking up the user's past behavior (per-user CTR, last-N campaigns served, recency, etc.):

| Field | Value |
|---|---|
| User-history features used? | **TBD-DS** |
| Bidder default | No — model uses only request-time features |
| Read pattern | **TBD-DS** (one Redis key per user with packed vector / multiple keys / online compute from event log) |
| Redis key family | **TBD-DS** (must be added to `docs/REDIS-KEYS.md` as `v1:hist:` family) |
| Per-feature staleness tolerance | **TBD-DS** (see § 7) |
| Read latency budget | **TBD-DS** (the bid path has 5ms total Redis budget; user-history must fit) |

If user-history features are added, the bidder gains a new pipeline stage (`UserFeatureFetchStage`) between user enrichment and scoring. The Redis schema and latency contract get added to `REDIS-KEYS.md` as a hard precondition.

---

## 4. Output contract

| Field | Value |
|---|---|
| Output tensor shape | **TBD-DS** — bidder default: `[N]` (one f32 per candidate) |
| Multi-objective output? | **TBD-DS** — bidder default: no |
| If multi-output, fields and bidder behavior | **TBD-DS** |

---

## 5. Cascade configuration

When the configured scorer is `cascade`:

| Field | Value |
|---|---|
| Stage 1 scorer | **TBD-DS** — bidder default: `feature_weighted` |
| Stage 2 scorer | **TBD-DS** — bidder default: `ml` |
| Top-K passed from stage 1 to stage 2 | **TBD-DS** — bidder default: 50 |
| Threshold value | **TBD-DS** — bidder default: 0.0 (top-K only, no threshold) |
| Threshold owner | **TBD-DS** (DS team / campaign managers / config-driven per-campaign override) |

The bidder reads these from the `[scoring.cascade]` config section. Operations changes the values without redeploy.

---

## 6. Model lifecycle

| Field | Value |
|---|---|
| Refresh cadence | **TBD-DS** — bidder default: file-watch via `notify`, reload on `Modify(Data)` event with 200ms debounce |
| Version-rollout strategy | **TBD-DS** (instant cutover / A/B by user hash via `ABTestScorer` / gradual % ramp) |
| Concurrent versions supported | Up to two (current + experimental) via `ABTestScorer` config |
| Model file location (production) | `[scoring.ml] model_path` config — operationally controlled, never embedded in the binary |
| Model file location (CI/tests) | `tests/fixtures/test_pctr_model.onnx` — generated by `cargo run -p gen-test-model` from the synthetic generator |

---

## 7. Feature freshness

For features that originate outside the bid request itself (campaign features pre-computed at catalog load, user-history features in Redis):

| Source | Acceptable staleness | Action when stale |
|---|---|---|
| Campaign features (catalog) | 60s (matches catalog refresh interval) | **TBD-DS** — bidder default: serve, alert |
| User-history features (Redis) | **TBD-DS** | **TBD-DS** — bidder default: serve with `missing_default`, increment `bidder.scoring.feature_stale` counter |

If staleness violates calibration assumptions enough to cause a measurable revenue impact, the policy is `bypass to FeatureWeighted` (see § 8).

---

## 8. Missing-feature fallback policy

When the bidder cannot compute a feature for the production-call path:

| Failure mode | Policy | Metric |
|---|---|---|
| One feature missing (Redis miss, non-required field absent in request) | Use `missing_default` from schema; score normally | `bidder.scoring.feature_default_used{feature}` |
| Multiple features missing (>**TBD-DS**% of feature vector) | **TBD-DS** — bidder default: bypass `MLScorer`, fall back to `FeatureWeightedScorer` for this request | `bidder.scoring.ml_bypassed{reason}` |
| Model inference error (ONNX session failure, NaN output) | Bypass to `FeatureWeightedScorer`; do not drop the candidate | `bidder.scoring.ml_inference_error` |
| Required user-history Redis call timeout | **TBD-DS** — bidder default: bypass to `FeatureWeightedScorer` | `bidder.scoring.user_feature_timeout` |

The bidder never silently emits a low-quality bid because of feature corruption. It either falls through to a known-good scorer (FeatureWeighted) or, if even that can't run, returns NoBid with reason `INVALID_REQUEST`.

---

## 9. Inference batching

| Field | Value |
|---|---|
| Trained batch size | **TBD-DS** |
| Bidder default | Dynamic batch axis; pad to next multiple of 8, max 64 |
| ONNX export uses dynamic batch axis? | **TBD-DS** |
| If fixed batch size N | Bidder pads sub-N candidate sets with zero rows and discards padded outputs |

---

## 10. Performance targets

| Field | Value |
|---|---|
| Target inference p99 (per batch of 50 candidates) | **TBD-DS** |
| Bidder allocated budget | 4 ms (out of the 5 ms scoring budget — leaves 1 ms for feature pack/unpack) |
| Hardware DS team benchmarked on | **TBD-DS** (M1 / Linux x86_64 AVX2 / Linux x86_64 AVX-512) |
| Hardware bidder runs on (production) | Linux x86_64, AVX2 minimum |
| ONNX Runtime version | 1.18+ (locked at deploy time, see `ort` crate version) |
| ONNX Runtime providers enabled | CPU only (CoreML on dev macOS for parity check) |

If DS benchmarked on Apple Silicon, halve the latency for the production x86 estimate.

---

## 11. Online/offline parity verification

**The single biggest reason ML-scored bidders bid wrong in production: training and serving compute features differently and no one notices.**

The bidder runs a parity check at boot:

| Step | Action |
|---|---|
| 1 | Load `tests/fixtures/scoring_parity.jsonl` — a list of `(raw_feature_input, expected_score)` pairs |
| 2 | For each pair, run the full feature transform pipeline + ONNX inference |
| 3 | Assert `|computed_score - expected_score| < 1e-4` |
| 4 | If any pair fails, log loudly and refuse to start with `MLScorer` enabled |

**TBD-DS deliverable:** the JSONL file. DS pipeline emits it alongside the model file. Format:

```jsonl
{"input": {"bid_price_norm": 0.5, "segment_overlap_ratio": 0.3, ...}, "expected_score": 0.0734}
{"input": {"bid_price_norm": 0.0, "segment_overlap_ratio": 0.0, ...}, "expected_score": 0.0119}
```

10–50 pairs is enough to catch every non-trivial parity bug. Without this artifact, parity is not verifiable.

---

## 12. Schema versioning

When the schema changes (new feature, removed feature, transform change):

1. DS team bumps `schema_version` (semver).
2. New ONNX file ships alongside new schema file with the same version.
3. Bidder loads both; if `schema_version` major doesn't match what the bidder was built to support, `MLScorer` refuses to load and the bidder runs on `FeatureWeightedScorer` only. Alert fires.
4. Minor and patch bumps are tolerated; major requires a coordinated bidder release.

---

## Appendix A — Bidder-side defaults (for reference)

These are the values the bidder uses today. Every "TBD-DS" above replaces one of these once the DS team confirms.

```toml
[scoring]
# Active scorer. Options: "feature_weighted", "ml", "cascade", "ab_test"
kind = "feature_weighted"

[scoring.ml]
model_path = "/etc/bidder/models/pctr.onnx"
parity_file = "/etc/bidder/models/scoring_parity.jsonl"
input_tensor_name = "features"
output_tensor_name = "pctr"
batch_pad_to = 8
max_batch = 64
inference_timeout_ms = 4

[scoring.cascade]
# Names of the two registered scorers.
stage1 = "feature_weighted"
stage2 = "ml"
top_k = 50
threshold = 0.0

[scoring.ab_test]
control = "feature_weighted"
treatment = "ml"
treatment_share = 0.10  # 10% of users routed to treatment
hash_seed = "phase-6-rollout"
```

## Appendix B — Definition of done for this contract

The contract is "locked" when every TBD-DS placeholder has a real value, the schema artifact path is committed, and the parity-test JSONL is generated by the DS pipeline. Until then the bidder operates with the documented defaults and `MLScorer` is config-disabled in production.
