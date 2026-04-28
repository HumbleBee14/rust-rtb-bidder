//! Generates `scoring_parity.jsonl` — the boot-time parity fixture the bidder
//! validates `MLScorer` against. Computed from the same `WEIGHTS` and `BIAS`
//! the ONNX file embeds, so any divergence in the bidder's feature transform
//! pipeline (wrong order, wrong normalization, off-by-one) is caught at boot.
//!
//! Each record:
//!   {"input": [f32; 13], "expected_score": f32}

use crate::{BIAS, FEATURE_COUNT, WEIGHTS};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn score(features: &[f32; FEATURE_COUNT]) -> f32 {
    let mut s = BIAS;
    for i in 0..FEATURE_COUNT {
        s += features[i] * WEIGHTS[i];
    }
    sigmoid(s)
}

fn fixture_inputs() -> Vec<[f32; FEATURE_COUNT]> {
    vec![
        // All zeros
        [0.0; FEATURE_COUNT],
        // All ones
        [1.0; FEATURE_COUNT],
        // First feature only
        {
            let mut v = [0.0; FEATURE_COUNT];
            v[0] = 1.0;
            v
        },
        // Last feature only
        {
            let mut v = [0.0; FEATURE_COUNT];
            v[FEATURE_COUNT - 1] = 1.0;
            v
        },
        // Mid-range realistic values per feature contract
        [
            0.5, 0.3, 0.7, 0.0, 1.0, 0.0, 0.0, 0.5, 0.0, 1.0, 1.0, 0.0, 0.0,
        ],
        [
            0.9, 0.8, 0.4, 1.0, 0.0, 0.0, 0.0, 0.75, 1.0, 0.0, 0.0, 1.0, 0.0,
        ],
        [
            0.1, 0.05, 0.0, 0.0, 1.0, 0.0, 0.0, 0.25, 0.0, 0.0, 1.0, 0.0, 0.0,
        ],
    ]
}

pub fn write_parity_jsonl(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create parity output directory")?;
    }
    let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut w = BufWriter::new(f);
    for input in fixture_inputs() {
        let expected = score(&input);
        // Emit as a JSON array of 13 floats — feature order matches schema indices.
        write!(w, "{{\"input\":[")?;
        for (i, v) in input.iter().enumerate() {
            if i > 0 {
                write!(w, ",")?;
            }
            write!(w, "{}", v)?;
        }
        writeln!(w, "],\"expected_score\":{}}}", expected)?;
    }
    w.flush()?;
    Ok(())
}
