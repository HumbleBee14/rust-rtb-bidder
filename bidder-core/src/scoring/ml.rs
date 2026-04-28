//! ONNX-Runtime-backed scorer with hot-reload and boot-time parity check.
//!
//! Model is loaded from a file path at startup. A background tokio task watches
//! the file via `notify`; on `Modify(Data)` events (debounced 200ms) the watcher
//! builds a fresh session pool and atomically swaps it in via `ArcSwap`. Existing
//! requests holding the old pool finish on the old model; new requests see the
//! new model. No coordination required.
//!
//! Sessions in `ort 2.0.0-rc.10` require `&mut self` for inference. We keep a
//! pool of N sessions (N = num_cpus by default), each in its own `Mutex`, and
//! pick by atomic round-robin. This trades per-session contention for N-way
//! parallelism — the same trade-off the Redis client pool makes.
//!
//! Boot-time parity check: loads `parity_path` (a JSONL file produced by the
//! training pipeline), runs every input through the loaded session, and refuses
//! to start if any score differs by more than 1e-4 from the expected value.
//! CONTRACT: docs/SCORING-FEATURES.md § 11.

use crate::{
    model::candidate::AdCandidate,
    scoring::{features::FEATURE_COUNT, Scorer, ScoringContext, ScoringFeatures},
};
use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use ndarray::Array2;
use ort::{
    session::{builder::GraphOptimizationLevel, Session, SessionInputValue},
    value::TensorRef,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{info, warn};

/// Configuration for `MLScorer`. Wires into `[scoring.ml]` in config.toml.
///
/// CONTRACT: docs/SCORING-FEATURES.md § Appendix A.
#[derive(Debug, Clone)]
pub struct MLScorerConfig {
    /// Absolute path to the ONNX model file.
    pub model_path: PathBuf,
    /// Optional path to the parity JSONL fixture. When set, the bidder runs the
    /// parity check at startup and refuses to load `MLScorer` if it fails.
    pub parity_path: Option<PathBuf>,
    /// Name of the model's input tensor. Default in the bidder's synthetic
    /// fixture: `"features"`. Real models must declare theirs.
    pub input_tensor_name: String,
    /// Name of the model's output tensor. Default: `"pctr"`.
    pub output_tensor_name: String,
    /// Number of session instances in the pool. Each handles one inference at a
    /// time; pool size sets the maximum concurrent inferences.
    pub pool_size: usize,
    /// Maximum batch size to submit per inference call. Larger batches amortize
    /// the syscall overhead but consume more memory; production tunes this with
    /// the DS team.
    pub max_batch: usize,
    /// Pad inputs up to a multiple of this value. Some ONNX exports prefer fixed
    /// batch sizes; padding to a multiple of 8 is a no-op for dynamic-axis
    /// models and avoids per-call recompilation for fixed-axis ones.
    pub batch_pad_to: usize,
}

impl MLScorerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.pool_size == 0 {
            return Err(anyhow!("MLScorerConfig.pool_size must be >= 1"));
        }
        if self.max_batch == 0 {
            return Err(anyhow!("MLScorerConfig.max_batch must be >= 1"));
        }
        if self.batch_pad_to == 0 {
            return Err(anyhow!("MLScorerConfig.batch_pad_to must be >= 1"));
        }
        Ok(())
    }
}

/// A single immutable model snapshot. The `Mutex<Session>` pool is the only
/// mutable state; swapping snapshots replaces the whole pool atomically.
struct ModelSnapshot {
    sessions: Vec<tokio::sync::Mutex<Session>>,
    /// Round-robin index. `usize::MAX` is fine for wraparound — `% sessions.len()`.
    next: AtomicUsize,
    input_name: String,
    output_name: String,
}

impl ModelSnapshot {
    fn build(cfg: &MLScorerConfig) -> Result<Self> {
        let mut sessions = Vec::with_capacity(cfg.pool_size);
        for _ in 0..cfg.pool_size {
            let session = Session::builder()
                .context("ort Session::builder failed — is libonnxruntime loadable?")?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .context("with_optimization_level")?
                .with_intra_threads(1)
                .context("with_intra_threads")?
                .commit_from_file(&cfg.model_path)
                .with_context(|| format!("commit_from_file {}", cfg.model_path.display()))?;
            sessions.push(tokio::sync::Mutex::new(session));
        }
        Ok(Self {
            sessions,
            next: AtomicUsize::new(0),
            input_name: cfg.input_tensor_name.clone(),
            output_name: cfg.output_tensor_name.clone(),
        })
    }

    fn pick(&self) -> &tokio::sync::Mutex<Session> {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        &self.sessions[i]
    }
}

pub struct MLScorer {
    cfg: MLScorerConfig,
    snapshot: Arc<ArcSwap<ModelSnapshot>>,
    /// Holds the file watcher alive for the scorer's lifetime. Dropping it
    /// cleanly shuts down the OS-level fsevent/inotify subscription so the
    /// background reload task exits without leaking native handles.
    _watcher: Option<Box<dyn notify::Watcher + Send + Sync>>,
    /// Optional fallback scorer used on inference failure. CONTRACT:
    /// docs/SCORING-FEATURES.md § 8 — when ONNX inference errors out (Redis
    /// timeout for user features in future, NaN output, ort error), bypass
    /// MLScorer entirely and delegate the whole batch to this scorer rather
    /// than emit half-scored output that would corrupt RankingStage.
    ///
    /// `None` is only acceptable in tests; production wiring always supplies
    /// `FeatureWeightedScorer` as the fallback so a degraded ML path still
    /// produces ranked bids.
    fallback: Option<Arc<dyn Scorer>>,
}

impl MLScorer {
    /// Construct, load, parity-verify, and start the file-watch task. Returns
    /// an error (and refuses to start) if any step fails — better than serving
    /// silently corrupted scores.
    ///
    /// **Threading note:** must be called once at startup, before any code
    /// path can trigger a model-file event that would race the watcher's
    /// background reload task. In practice that means: call from `main()`
    /// before the HTTP server starts. Calling concurrently with another
    /// `MLScorer::new` against the same `cfg.model_path` is undefined.
    pub fn new(cfg: MLScorerConfig) -> Result<Self> {
        Self::new_with_fallback(cfg, None)
    }

    /// Construct with an explicit fallback scorer used on inference failure.
    /// Production wiring should always pass a fallback (typically
    /// `FeatureWeightedScorer`) so a degraded model path produces ranked bids
    /// rather than zero-scored ones. See CONTRACT § 8.
    pub fn new_with_fallback(
        cfg: MLScorerConfig,
        fallback: Option<Arc<dyn Scorer>>,
    ) -> Result<Self> {
        cfg.validate()?;
        let snapshot = ModelSnapshot::build(&cfg)
            .context("MLScorer initial model load failed (libonnxruntime missing? run tools/install-onnxruntime.sh)")?;
        let snapshot = Arc::new(ArcSwap::from_pointee(snapshot));

        // Build the watcher synchronously before returning so its destructor
        // runs in the caller's scope when MLScorer is dropped, not at process
        // teardown alongside an arbitrary tokio task.
        let watcher = Self::start_watcher(&cfg, Arc::clone(&snapshot));
        let scorer = Self {
            cfg,
            snapshot: Arc::clone(&snapshot),
            _watcher: watcher,
            fallback,
        };

        if let Some(parity_path) = scorer.cfg.parity_path.clone() {
            scorer.verify_parity(&parity_path).with_context(|| {
                format!("scoring parity check against {}", parity_path.display())
            })?;
        }

        info!(
            model = %scorer.cfg.model_path.display(),
            pool_size = scorer.cfg.pool_size,
            "MLScorer initialised"
        );
        Ok(scorer)
    }

    /// Spin up a `notify` watcher whose Send + Sync handle is owned by the
    /// returned Box. The watcher pumps events into an unbounded channel; a
    /// detached tokio task reads them and rebuilds the model snapshot. When the
    /// returned Watcher is dropped, the channel closes and the task exits.
    ///
    /// Uses `PollWatcher` rather than the OS-native (fsevent/inotify) backend.
    /// Tradeoff: a 5s reload latency vs. instant. ML model files don't churn at
    /// sub-second cadence — the operational benefit (no native threads, clean
    /// shutdown, no fsevent teardown crashes) is worth the latency. Revisit if
    /// model rollouts ever need sub-5s propagation.
    fn start_watcher(
        cfg: &MLScorerConfig,
        snapshot: Arc<ArcSwap<ModelSnapshot>>,
    ) -> Option<Box<dyn notify::Watcher + Send + Sync>> {
        use notify::{
            Config as NotifyConfig, Event, EventKind, PollWatcher, RecursiveMode, Watcher,
        };
        let path = cfg.model_path.clone();
        let cfg_clone = cfg.clone();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let poll_interval = Duration::from_secs(5);
        let notify_cfg = NotifyConfig::default()
            .with_poll_interval(poll_interval)
            .with_compare_contents(false);
        let mut watcher = match PollWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(ev) = res {
                    let _ = tx.send(ev);
                }
            },
            notify_cfg,
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "MLScorer file watcher init failed; hot-reload disabled");
                return None;
            }
        };
        let watch_target = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if let Err(e) = watcher.watch(&watch_target, RecursiveMode::NonRecursive) {
            warn!(error = %e, path = %watch_target.display(), "watcher.watch failed");
            return None;
        }

        tokio::spawn(async move {
            let debounce = Duration::from_millis(200);
            while let Some(ev) = rx.recv().await {
                let touches_model = ev.paths.iter().any(|p| p == &path);
                if !touches_model {
                    continue;
                }
                let interesting = matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_));
                if !interesting {
                    continue;
                }
                tokio::time::sleep(debounce).await;
                while rx.try_recv().is_ok() {}
                match ModelSnapshot::build(&cfg_clone) {
                    Ok(new) => {
                        snapshot.store(Arc::new(new));
                        metrics::counter!("bidder.scoring.ml.reload_total", "result" => "ok")
                            .increment(1);
                        info!(model = %path.display(), "MLScorer hot-reloaded");
                    }
                    Err(e) => {
                        metrics::counter!("bidder.scoring.ml.reload_total", "result" => "error")
                            .increment(1);
                        warn!(error = %e, "MLScorer hot-reload failed; staying on previous model");
                    }
                }
            }
        });

        Some(Box::new(watcher))
    }

    fn verify_parity(&self, parity_path: &Path) -> Result<()> {
        let raw = std::fs::read_to_string(parity_path)
            .with_context(|| format!("read {}", parity_path.display()))?;
        let mut count = 0usize;
        for (line_no, line) in raw.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let pair: ParityPair = serde_json::from_str(trimmed).with_context(|| {
                format!(
                    "parse parity line {} in {}",
                    line_no + 1,
                    parity_path.display()
                )
            })?;
            if pair.input.len() != FEATURE_COUNT {
                return Err(anyhow!(
                    "parity line {}: input has {} features, expected {}",
                    line_no + 1,
                    pair.input.len(),
                    FEATURE_COUNT
                ));
            }
            // Run sync (no async runtime guaranteed at construction time).
            let snap = self.snapshot.load();
            let mut session_guard = snap.sessions[0]
                .try_lock()
                .map_err(|_| anyhow!("session unavailable during parity check"))?;
            let mut row = [0f32; FEATURE_COUNT];
            row.copy_from_slice(&pair.input);
            let mut input_array = Array2::<f32>::zeros((1, FEATURE_COUNT));
            for (i, v) in row.iter().enumerate() {
                input_array[[0, i]] = *v;
            }
            let input_view =
                TensorRef::from_array_view(&input_array).context("TensorRef::from_array_view")?;
            let inputs: Vec<(&str, SessionInputValue)> = vec![(
                snap.input_name.as_str(),
                SessionInputValue::from(input_view),
            )];
            let outputs = session_guard
                .run(inputs)
                .context("Session::run failed during parity check")?;
            let out = outputs
                .get(snap.output_name.as_str())
                .ok_or_else(|| anyhow!("missing output tensor {}", snap.output_name))?;
            let (_shape, data) = out
                .try_extract_tensor::<f32>()
                .context("extract f32 tensor from parity output")?;
            let computed = data
                .first()
                .copied()
                .ok_or_else(|| anyhow!("empty output"))?;
            let drift = (computed - pair.expected_score).abs();
            if drift > 1e-4 {
                return Err(anyhow!(
                    "parity drift on line {}: computed={} expected={} drift={}",
                    line_no + 1,
                    computed,
                    pair.expected_score,
                    drift
                ));
            }
            count += 1;
        }
        if count == 0 {
            return Err(anyhow!(
                "parity file {} contained zero records",
                parity_path.display()
            ));
        }
        info!(records = count, "MLScorer parity check passed");
        Ok(())
    }

    async fn score_batch(
        &self,
        candidates: &mut [AdCandidate],
        ctx: &ScoringContext<'_>,
    ) -> Result<()> {
        let n = candidates.len();
        if n == 0 {
            return Ok(());
        }
        let snap = self.snapshot.load();

        // Pad to a multiple of batch_pad_to so fixed-axis models don't need
        // recompilation per batch. Padded rows are zero; their scores are
        // discarded.
        let padded_n = n.div_ceil(self.cfg.batch_pad_to) * self.cfg.batch_pad_to;
        let mut input = Array2::<f32>::zeros((padded_n, FEATURE_COUNT));
        for (i, candidate) in candidates.iter().enumerate() {
            let f = ScoringFeatures::extract(candidate, ctx);
            let mut row = [0f32; FEATURE_COUNT];
            f.pack_into(&mut row);
            for (j, v) in row.iter().enumerate() {
                input[[i, j]] = *v;
            }
        }

        let session_mutex = snap.pick();
        let mut guard = session_mutex.lock().await;
        let input_view =
            TensorRef::from_array_view(&input).context("TensorRef::from_array_view")?;
        let inputs: Vec<(&str, SessionInputValue)> = vec![(
            snap.input_name.as_str(),
            SessionInputValue::from(input_view),
        )];
        let outputs = guard.run(inputs).context("ONNX inference failed")?;
        let out = outputs
            .get(snap.output_name.as_str())
            .ok_or_else(|| anyhow!("missing output tensor {}", snap.output_name))?;
        let (shape, data) = out
            .try_extract_tensor::<f32>()
            .context("extract f32 tensor")?;

        // Output layout: [N] or [N, 1]. Either way, take the first n values.
        if (data.len() as i64) < n as i64 {
            return Err(anyhow!(
                "ONNX output has {} elements but expected at least {} (shape={:?})",
                data.len(),
                n,
                shape
            ));
        }

        for (i, candidate) in candidates.iter_mut().enumerate() {
            candidate.score = data[i];
        }
        Ok(())
    }
}

#[async_trait]
impl Scorer for MLScorer {
    /// Score all candidates by ML inference. On any chunk failure:
    ///   1. Reset every score in `candidates` to 0.0 (no half-scored leak).
    ///   2. If `fallback` is set, delegate the entire batch to it.
    ///   3. Otherwise leave scores at 0.0 and increment the error metric;
    ///      RankingStage will tie-break by price.
    ///
    /// CONTRACT: docs/SCORING-FEATURES.md § 8 — never emit a partial score.
    async fn score_all(&self, candidates: &mut Vec<AdCandidate>, ctx: &ScoringContext<'_>) {
        let max = self.cfg.max_batch;
        let mut start = 0usize;
        while start < candidates.len() {
            let end = (start + max).min(candidates.len());
            let chunk = &mut candidates[start..end];
            if let Err(e) = self.score_batch(chunk, ctx).await {
                metrics::counter!("bidder.scoring.ml.inference_error_total").increment(1);
                tracing::warn!(error = %e, "MLScorer inference failed; resetting batch and falling back");
                // Wipe any partial scores written by earlier successful chunks.
                for c in candidates.iter_mut() {
                    c.score = 0.0;
                }
                if let Some(fallback) = self.fallback.as_ref() {
                    metrics::counter!("bidder.scoring.ml.fallback_invoked_total").increment(1);
                    fallback.score_all(candidates, ctx).await;
                } else {
                    metrics::counter!("bidder.scoring.ml.fallback_unavailable_total").increment(1);
                    tracing::error!(
                        "MLScorer inference failed and no fallback configured — \
                         all candidates left at score=0.0; production wiring should \
                         always supply FeatureWeightedScorer as the fallback"
                    );
                }
                return;
            }
            start = end;
        }
    }
}

/// One row of the parity JSONL file.
#[derive(serde::Deserialize)]
struct ParityPair {
    input: Vec<f32>,
    expected_score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // The end-to-end ort load + parity test lives in
    // bidder-core/tests/ml_scorer_integration.rs as an integration test.
    // It runs in its own process, so ort's process-lifetime teardown can't
    // crash the unit-test binary.

    #[tokio::test]
    async fn rejects_zero_pool_size() {
        let cfg = MLScorerConfig {
            model_path: PathBuf::from("/dev/null"),
            parity_path: None,
            input_tensor_name: "features".to_string(),
            output_tensor_name: "pctr".to_string(),
            pool_size: 0,
            max_batch: 8,
            batch_pad_to: 8,
        };
        assert!(MLScorer::new(cfg).is_err());
    }
}
