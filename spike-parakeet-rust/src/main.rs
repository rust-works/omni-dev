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
    /// Time a synthetic 24-block Conformer encoder forward pass on CPU.
    /// Uses random weights with the production op shapes (no safetensors load).
    /// Reports wall-clock and RTF.
    BenchEncoder {
        /// Sequence length after pre-encode 8x subsampling. T=750 ≈ 1 min audio, T=3750 ≈ 5 min audio.
        #[arg(long, default_value_t = 750)]
        t: usize,
        /// Number of Conformer blocks (production model is 24).
        #[arg(long, default_value_t = 24)]
        blocks: usize,
        /// Warm-up iterations (not timed).
        #[arg(long, default_value_t = 1)]
        warmup: usize,
        /// Timed iterations.
        #[arg(long, default_value_t = 3)]
        iters: usize,
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

// Layer norm matching MLX's `(x - E[x]) / (sqrt(Var[x]) + eps) * gamma + beta`.
fn layer_norm(x: &Tensor, gamma: &Tensor, beta: &Tensor, eps: f32) -> Result<Tensor> {
    let mean = x.mean_keepdim(2)?;
    let centered = x.broadcast_sub(&mean)?;
    let var = centered.sqr()?.mean_keepdim(2)?;
    let eps_t = Tensor::new(eps, x.device())?;
    let denom = (var.sqrt()? + eps_t.broadcast_as(mean.shape())?)?;
    let normed = centered.broadcast_div(&denom)?;
    Ok(normed.broadcast_mul(gamma)?.broadcast_add(beta)?)
}

// FFN sub-path: LayerNorm + Linear(d_model->d_ff) + SiLU + Linear(d_ff->d_model) + 0.5*residual.
// Matches `feed_forward1` / `feed_forward2` in parakeet_mlx/conformer.py.
fn ffn_sub_path(
    x: &Tensor,
    gamma_norm: &Tensor,
    beta_norm: &Tensor,
    w_l1: &Tensor,
    w_l2: &Tensor,
) -> Result<Tensor> {
    let normed = layer_norm(x, gamma_norm, beta_norm, 1e-5)?;
    let h = normed.broadcast_matmul(&w_l1.t()?)?;
    let h_silu = (&h * candle_nn::ops::sigmoid(&h)?)?;
    let ff_out = h_silu.broadcast_matmul(&w_l2.t()?)?;
    let half = Tensor::new(0.5f32, x.device())?;
    Ok((x + ff_out.broadcast_mul(&half)?)?)
}

// Self-attention sub-path: LayerNorm + Q/K/V/Out projections + SDPA (skipping rel-pos bias for
// the bench — rel-pos adds another O(T*d_model) matmul but the dominant attention cost is the
// O(T^2*d_head*n_heads) scoring, which IS included).
fn attn_sub_path(
    x: &Tensor,
    gamma_norm: &Tensor,
    beta_norm: &Tensor,
    w_q: &Tensor,
    w_k: &Tensor,
    w_v: &Tensor,
    w_o: &Tensor,
    n_heads: usize,
) -> Result<Tensor> {
    let normed = layer_norm(x, gamma_norm, beta_norm, 1e-5)?;
    let (b, t, d) = normed.dims3()?;
    let d_head = d / n_heads;

    let q = normed.broadcast_matmul(&w_q.t()?)?;
    let k = normed.broadcast_matmul(&w_k.t()?)?;
    let v = normed.broadcast_matmul(&w_v.t()?)?;

    // Reshape to (b, n_heads, t, d_head).
    let q = q
        .reshape((b, t, n_heads, d_head))?
        .transpose(1, 2)?
        .contiguous()?;
    let k = k
        .reshape((b, t, n_heads, d_head))?
        .transpose(1, 2)?
        .contiguous()?;
    let v = v
        .reshape((b, t, n_heads, d_head))?
        .transpose(1, 2)?
        .contiguous()?;

    // Scaled dot-product attention.
    let scale = Tensor::new((d_head as f32).powf(-0.5), x.device())?;
    let scores = q.matmul(&k.transpose(2, 3)?.contiguous()?)?;
    let scores = scores.broadcast_mul(&scale)?;
    let probs = candle_nn::ops::softmax_last_dim(&scores)?;
    let attn = probs.matmul(&v)?;

    // Reshape back.
    let attn = attn.transpose(1, 2)?.contiguous()?.reshape((b, t, d))?;
    let out = attn.broadcast_matmul(&w_o.t()?)?;
    Ok((x + out)?)
}

// Conv module sub-path: LayerNorm + pointwise_conv1 (d->2d) + GLU + (skip depthwise + BN — cheap)
// + SiLU + pointwise_conv2 (d->d). The pointwise convs dominate (kernel=1, so it's just a matmul
// via the channel dimension); the depthwise (kernel=9, groups=channels) is ~1% of pointwise cost.
fn conv_sub_path(
    x: &Tensor,
    gamma_norm: &Tensor,
    beta_norm: &Tensor,
    w_pw1: &Tensor, // shape (2*d, d)
    w_pw2: &Tensor, // shape (d, d)
) -> Result<Tensor> {
    let normed = layer_norm(x, gamma_norm, beta_norm, 1e-5)?;
    // pointwise_conv1 (kernel=1) expressed as Linear over channel dim
    let h = normed.broadcast_matmul(&w_pw1.t()?)?; // (b, t, 2*d)
                                                   // GLU on the channel dim: split into a, b; output = a * sigmoid(b)
    let (b, t, two_d) = h.dims3()?;
    let d = two_d / 2;
    let a = h.narrow(2, 0, d)?;
    let g = h.narrow(2, d, d)?;
    let glu = (&a * candle_nn::ops::sigmoid(&g)?)?;
    // (skip the depthwise k=9 + BatchNorm — together <5% of cost; representative enough)
    let glu_silu = (&glu * candle_nn::ops::sigmoid(&glu)?)?;
    let out = glu_silu.broadcast_matmul(&w_pw2.t()?)?;
    let _ = (b, t);
    Ok((x + out)?)
}

fn bench_encoder(t: usize, blocks: usize, warmup: usize, iters: usize) -> Result<()> {
    let dev = Device::Cpu;
    let d_model = 1024usize;
    let d_ff = 4096usize;
    let n_heads = 8usize;

    // Random weights — matmul cost depends only on shape, not values. Using a single
    // shared set of weights across blocks; arithmetic is identical to loading per-block.
    let w_ffn_l1 = Tensor::randn(0f32, 0.02f32, (d_ff, d_model), &dev)?;
    let w_ffn_l2 = Tensor::randn(0f32, 0.02f32, (d_model, d_ff), &dev)?;
    let w_q = Tensor::randn(0f32, 0.02f32, (d_model, d_model), &dev)?;
    let w_k = Tensor::randn(0f32, 0.02f32, (d_model, d_model), &dev)?;
    let w_v = Tensor::randn(0f32, 0.02f32, (d_model, d_model), &dev)?;
    let w_o = Tensor::randn(0f32, 0.02f32, (d_model, d_model), &dev)?;
    let w_pw1 = Tensor::randn(0f32, 0.02f32, (2 * d_model, d_model), &dev)?;
    let w_pw2 = Tensor::randn(0f32, 0.02f32, (d_model, d_model), &dev)?;
    let gamma = Tensor::ones((d_model,), DType::F32, &dev)?;
    let beta = Tensor::zeros((d_model,), DType::F32, &dev)?;

    let input = Tensor::randn(0f32, 1f32, (1, t, d_model), &dev)?;

    let one_pass = |x_in: &Tensor| -> Result<Tensor> {
        let mut x = x_in.clone();
        for _ in 0..blocks {
            x = ffn_sub_path(&x, &gamma, &beta, &w_ffn_l1, &w_ffn_l2)?;
            x = attn_sub_path(&x, &gamma, &beta, &w_q, &w_k, &w_v, &w_o, n_heads)?;
            x = conv_sub_path(&x, &gamma, &beta, &w_pw1, &w_pw2)?;
            x = ffn_sub_path(&x, &gamma, &beta, &w_ffn_l1, &w_ffn_l2)?;
            x = layer_norm(&x, &gamma, &beta, 1e-5)?;
        }
        Ok(x)
    };

    for _ in 0..warmup {
        let _ = one_pass(&input)?;
    }

    let mut samples_secs: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = std::time::Instant::now();
        let out = one_pass(&input)?;
        // Force materialisation
        let _ = out.sum_all()?.to_scalar::<f32>()?;
        samples_secs.push(start.elapsed().as_secs_f64());
    }

    let audio_secs = (t as f64) / 12.5; // 12.5 tokens/sec after 8x subsampling at 100 frames/sec
    let mean = samples_secs.iter().sum::<f64>() / samples_secs.len() as f64;
    let min = samples_secs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples_secs
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);

    println!("encoder forward (T={t}, blocks={blocks})");
    println!("  audio represented:   {audio_secs:.2}s");
    println!("  wall-clock min/mean/max: {min:.3} / {mean:.3} / {max:.3}s  (n={iters})");
    println!("  RTF (mean):          {:.3}", mean / audio_secs);
    println!("  RTF (min):           {:.3}", min / audio_secs);

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
        Cmd::BenchEncoder {
            t,
            blocks,
            warmup,
            iters,
        } => {
            bench_encoder(t, blocks, warmup, iters)?;
        }
    }
    Ok(())
}
