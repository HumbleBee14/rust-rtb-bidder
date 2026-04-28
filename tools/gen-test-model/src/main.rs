//! Generates a deterministic synthetic ONNX model used as a test fixture.
//!
//! The model matches the feature contract defined in `docs/SCORING-FEATURES.md`:
//! 13 f32 features in, scalar pCTR out, sigmoid of linear combination.
//!
//!   pctr = sigmoid(W · features + b)
//!
//! Weights are fixed in source so the generated `.onnx` is byte-identical across
//! machines and across regenerations. The expected (input → output) pairs that
//! the bidder boot-time parity check validates are derived from the same
//! constants — see `tools/gen-test-model/src/parity.rs`.
//!
//! Run with: `cargo run -p gen-test-model -- <output_path>`
//! or via:   `make regen-test-model`

mod parity;

pub mod onnx {
    include!(concat!(env!("OUT_DIR"), "/onnx.rs"));
}

use anyhow::{Context, Result};
use onnx::{
    tensor_proto::DataType, tensor_shape_proto::dimension::Value as DimValue,
    tensor_shape_proto::Dimension, type_proto::Tensor as TypeProtoTensor,
    type_proto::Value as TypeValue, AttributeProto, GraphProto, ModelProto, NodeProto,
    OperatorSetIdProto, TensorProto, TensorShapeProto, TypeProto, ValueInfoProto,
};
use prost::Message;
use std::path::PathBuf;

/// Number of input features. Matches `docs/SCORING-FEATURES.md` § 2.
pub const FEATURE_COUNT: usize = 13;

/// Fixed weights. Chosen so each feature contributes a small non-zero amount;
/// no symmetry, no zeros (so a feature-order swap by the bidder produces a
/// detectable score divergence in the parity test).
pub const WEIGHTS: [f32; FEATURE_COUNT] = [
    0.45, 0.85, 0.30, 0.10, 0.20, 0.15, 0.05, 0.40, 0.25, 0.35, 0.18, 0.22, 0.12,
];

pub const BIAS: f32 = -2.5;

const INPUT_NAME: &str = "features";
const OUTPUT_NAME: &str = "pctr";
const WEIGHTS_NAME: &str = "W";
const BIAS_NAME: &str = "b";
const PRE_SIGMOID_NAME: &str = "logit";

fn dim(value: i64) -> Dimension {
    Dimension {
        value: Some(DimValue::DimValue(value)),
        denotation: String::new(),
    }
}

fn dyn_dim(name: &str) -> Dimension {
    Dimension {
        value: Some(DimValue::DimParam(name.to_string())),
        denotation: String::new(),
    }
}

fn tensor_type(elem_type: DataType, dims: Vec<Dimension>) -> TypeProto {
    TypeProto {
        value: Some(TypeValue::TensorType(TypeProtoTensor {
            elem_type: elem_type as i32,
            shape: Some(TensorShapeProto { dim: dims }),
        })),
        denotation: String::new(),
    }
}

fn float_initializer(name: &str, dims: Vec<i64>, data: Vec<f32>) -> TensorProto {
    TensorProto {
        dims,
        data_type: DataType::Float as i32,
        float_data: data,
        int32_data: vec![],
        string_data: vec![],
        int64_data: vec![],
        name: name.to_string(),
        doc_string: String::new(),
        raw_data: vec![],
        double_data: vec![],
        uint64_data: vec![],
    }
}

pub fn build_model() -> ModelProto {
    // Inputs: features [batch, FEATURE_COUNT]
    let input = ValueInfoProto {
        name: INPUT_NAME.to_string(),
        r#type: Some(tensor_type(
            DataType::Float,
            vec![dyn_dim("batch"), dim(FEATURE_COUNT as i64)],
        )),
        doc_string: String::new(),
    };

    // Outputs: pctr [batch, 1]
    let output = ValueInfoProto {
        name: OUTPUT_NAME.to_string(),
        r#type: Some(tensor_type(DataType::Float, vec![dyn_dim("batch"), dim(1)])),
        doc_string: String::new(),
    };

    // Initializers: W [FEATURE_COUNT, 1], b [1]
    let weights_tensor = float_initializer(
        WEIGHTS_NAME,
        vec![FEATURE_COUNT as i64, 1],
        WEIGHTS.to_vec(),
    );
    let bias_tensor = float_initializer(BIAS_NAME, vec![1], vec![BIAS]);

    // Nodes: MatMul(features, W) -> logit_pre; Add(logit_pre, b) -> logit; Sigmoid(logit) -> pctr
    let matmul = NodeProto {
        input: vec![INPUT_NAME.to_string(), WEIGHTS_NAME.to_string()],
        output: vec!["logit_pre".to_string()],
        name: "matmul".to_string(),
        op_type: "MatMul".to_string(),
        domain: String::new(),
        attribute: vec![],
        doc_string: String::new(),
    };
    let add = NodeProto {
        input: vec!["logit_pre".to_string(), BIAS_NAME.to_string()],
        output: vec![PRE_SIGMOID_NAME.to_string()],
        name: "add_bias".to_string(),
        op_type: "Add".to_string(),
        domain: String::new(),
        attribute: vec![],
        doc_string: String::new(),
    };
    let sigmoid = NodeProto {
        input: vec![PRE_SIGMOID_NAME.to_string()],
        output: vec![OUTPUT_NAME.to_string()],
        name: "sigmoid".to_string(),
        op_type: "Sigmoid".to_string(),
        domain: String::new(),
        attribute: Vec::<AttributeProto>::new(),
        doc_string: String::new(),
    };

    let graph = GraphProto {
        node: vec![matmul, add, sigmoid],
        name: "pctr_graph".to_string(),
        initializer: vec![weights_tensor, bias_tensor],
        doc_string: String::new(),
        input: vec![input],
        output: vec![output],
        value_info: vec![],
    };

    ModelProto {
        ir_version: 9, // ONNX 1.18 IR version
        opset_import: vec![OperatorSetIdProto {
            domain: String::new(),
            version: 18,
        }],
        producer_name: "rust-rtb-bidder/gen-test-model".to_string(),
        producer_version: env!("CARGO_PKG_VERSION").to_string(),
        domain: String::new(),
        model_version: 1,
        doc_string: "Synthetic test pCTR model — fixed weights, sigmoid(W·x + b)".to_string(),
        graph: Some(graph),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "tests/fixtures/test_pctr_model.onnx".to_string());
    let out_path = PathBuf::from(out_path);

    let model = build_model();
    let mut buf = Vec::with_capacity(model.encoded_len());
    model.encode(&mut buf).context("encode ModelProto")?;

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).context("create output directory")?;
    }
    std::fs::write(&out_path, &buf).with_context(|| format!("write {}", out_path.display()))?;

    println!("Wrote {} bytes to {}", buf.len(), out_path.display());

    // Also emit the parity fixture file so the bidder's boot parity check can
    // load it. Computed from the same fixed constants.
    let parity_path = out_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("scoring_parity.jsonl");
    parity::write_parity_jsonl(&parity_path)?;
    println!("Wrote parity fixtures to {}", parity_path.display());

    Ok(())
}
