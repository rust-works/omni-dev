// Spike #871: Parakeet Rust port feasibility probe.
// Throwaway — not production code.

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use clap::{Parser, Subcommand};
use ndarray::Array3;

#[derive(Parser)]
#[command(name = "spike-parakeet-rust")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the candle build environment (sanity check).
    Env,
    /// Load the converted safetensors and report a known tensor's shape.
    LoadCheck {
        #[arg(long, default_value = "candle_weights.safetensors")]
        path: String,
    },
    /// Run the block-0 FFN1 sub-path on the reference input and dump candle outputs.
    ParityFfn1 {
        #[arg(long, default_value = "candle_weights.safetensors")]
        weights: String,
        #[arg(long, default_value = "parity/reference/01_pre_encode_out.npy")]
        input: String,
        #[arg(long, default_value = "parity/candle")]
        out_dir: String,
    },
}

fn load_npy_3d(path: &str) -> Result<Array3<f32>> {
    let arr: Array3<f32> =
        ndarray_npy::read_npy(path).with_context(|| format!("read npy {path}"))?;
    Ok(arr)
}

fn save_npy_3d(path: &str, arr: &Array3<f32>) -> Result<()> {
    ndarray_npy::write_npy(path, arr).with_context(|| format!("write npy {path}"))?;
    Ok(())
}

fn tensor_to_array3(t: &Tensor) -> Result<Array3<f32>> {
    let dims = t.dims();
    assert_eq!(dims.len(), 3);
    let v: Vec<f32> = t.to_dtype(DType::F32)?.flatten_all()?.to_vec1::<f32>()?;
    Ok(Array3::from_shape_vec((dims[0], dims[1], dims[2]), v)?)
}

fn array3_to_tensor(arr: &Array3<f32>, dev: &Device) -> Result<Tensor> {
    let shape = arr.shape();
    let (d0, d1, d2) = (shape[0], shape[1], shape[2]);
    let v: Vec<f32> = arr.as_slice().expect("contiguous").to_vec();
    Ok(Tensor::from_vec(v, (d0, d1, d2), dev)?)
}

fn parity_ffn1(weights: &str, input: &str, out_dir: &str) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let dev = Device::Cpu;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DType::F32, &dev)? };

    // Input: (1, T, d_model) = (1, 63, 1024)
    let input_arr = load_npy_3d(input)?;
    println!("input shape: {:?}", input_arr.shape());
    let x = array3_to_tensor(&input_arr, &dev)?;

    // LayerNorm: norm_feed_forward1 (gamma, beta), eps=1e-5 default
    let gamma = vb.get((1024,), "encoder.layers.0.norm_feed_forward1.weight")?;
    let beta = vb.get((1024,), "encoder.layers.0.norm_feed_forward1.bias")?;
    // Manual LayerNorm over the last dim. MLX's formula: (x - E[x]) / (sqrt(Var[x]) + eps).
    // (Note: eps OUTSIDE sqrt — MLX-specific; PyTorch puts eps inside sqrt.)
    let mean = x.mean_keepdim(2)?;
    let centered = x.broadcast_sub(&mean)?;
    let var = centered.sqr()?.mean_keepdim(2)?;
    let eps = Tensor::new(1e-5f32, &dev)?;
    let denom = (var.sqrt()? + eps.broadcast_as(mean.shape())?)?;
    let normed = centered.broadcast_div(&denom)?;
    let normed = normed.broadcast_mul(&gamma)?.broadcast_add(&beta)?;
    let norm_out_arr = tensor_to_array3(&normed)?;
    save_npy_3d(&format!("{out_dir}/02_block0_norm_ff1.npy"), &norm_out_arr)?;
    println!("saved norm_ff1 output");

    // FeedForward: linear1 (4096, 1024) -> SiLU -> linear2 (1024, 4096), no bias
    let w1 = vb.get(
        (4096, 1024),
        "encoder.layers.0.feed_forward1.linear1.weight",
    )?;
    let w2 = vb.get(
        (1024, 4096),
        "encoder.layers.0.feed_forward1.linear2.weight",
    )?;

    // Linear: y = x @ W^T  (since W is stored (out, in))
    let h = normed.broadcast_matmul(&w1.t()?)?;
    // SiLU: x * sigmoid(x)
    let h_silu = (&h * candle_nn::ops::sigmoid(&h)?)?;
    let ff_out = h_silu.broadcast_matmul(&w2.t()?)?;
    let ff_out_arr = tensor_to_array3(&ff_out)?;
    save_npy_3d(&format!("{out_dir}/03_block0_ff1_out.npy"), &ff_out_arr)?;
    println!("saved ff1_out");

    // After residual: x + 0.5 * ff1_out
    let half = Tensor::new(0.5f32, &dev)?;
    let after = (x + ff_out.broadcast_mul(&half)?)?;
    let after_arr = tensor_to_array3(&after)?;
    save_npy_3d(
        &format!("{out_dir}/04_block0_after_ff1_residual.npy"),
        &after_arr,
    )?;
    println!("saved after_ff1_residual");

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Env => {
            let dev = candle_core::Device::Cpu;
            let t = candle_core::Tensor::zeros((2, 3), candle_core::DType::F32, &dev)?;
            println!("tensor: {:?}", t.shape());
        }
        Cmd::LoadCheck { path } => {
            use candle_core::{DType, Device};
            use candle_nn::VarBuilder;
            let dev = Device::Cpu;
            let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[&path], DType::F32, &dev)? };
            // Spot-check three tensors: a conv (was permuted), a Linear (identity),
            // and a LayerNorm (identity).
            let probes = [
                ("encoder.pre_encode.conv.0.weight", vec![256, 1, 3, 3]),
                (
                    "encoder.layers.0.self_attn.linear_q.weight",
                    vec![1024, 1024],
                ),
                ("encoder.layers.0.norm_self_att.weight", vec![1024]),
                (
                    "encoder.layers.0.conv.depthwise_conv.weight",
                    vec![1024, 1, 9],
                ),
                (
                    "encoder.layers.0.conv.pointwise_conv1.weight",
                    vec![2048, 1024, 1],
                ),
                ("decoder.prediction.embed.weight", vec![1025, 640]),
                ("joint.joint_net.2.weight", vec![1030, 640]),
            ];
            for (name, want) in probes {
                let t = vb.get(want.clone(), name)?;
                let got: Vec<usize> = t.dims().to_vec();
                let want_us: Vec<usize> = want.iter().map(|x| *x as usize).collect();
                let ok = got == want_us;
                println!(
                    "{:50} shape={:?} {}",
                    name,
                    got,
                    if ok { "OK" } else { "MISMATCH" }
                );
            }
        }
        Cmd::ParityFfn1 {
            weights,
            input,
            out_dir,
        } => {
            parity_ffn1(&weights, &input, &out_dir)?;
        }
    }
    Ok(())
}
