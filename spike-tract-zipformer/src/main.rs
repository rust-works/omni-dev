//! Spike: tract-onnx + streaming Zipformer prototype — minimal viable
//! probe.
//!
//! Status: PARTIAL. The streaming Zipformer encoder has 99 inputs (audio
//! features + 98 state tensors across 17 sub-layers) and 99 outputs
//! (encoded features + 98 updated states). A working streaming
//! prototype requires:
//!   - Initialising all 98 state tensors to zeros of the correct shape
//!     (extracted per-layer from the ONNX graph)
//!   - Cycling state outputs back to inputs each encoder call
//!   - Greedy RNN-T decoding loop (decoder + joiner)
//!   - BPE detokenisation (handling `▁` word-start markers)
//!
//! That wiring is realistically several days of careful work and is out
//! of the 2-day spike time-box. This binary verifies the things the
//! spike CAN cheaply validate:
//!   1. tract-onnx ingests the model graph (encoder + decoder + joiner)
//!   2. The optimisation pass succeeds (or surfaces specific op gaps)
//!   3. Time-to-load and steady-state RSS while the model is in memory
//!   4. C++-freeness of the dep graph
//!
//! Together with the probe binary's I/O dump, those are enough to
//! support the SPIKE.md decision: feasibility yes, prototype scope no.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use tract_onnx::prelude::*;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    /// Use the int8-quantised weights (~70 MB encoder vs ~260 MB f32).
    #[arg(long, default_value_t = false)]
    int8: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let suffix = if args.int8 { ".int8.onnx" } else { ".onnx" };

    eprintln!("loading models (suffix={suffix}) ...");
    let t0 = Instant::now();

    let encoder_path = args
        .model_dir
        .join(format!("encoder-epoch-99-avg-1-chunk-16-left-128{suffix}"));
    let decoder_path = args
        .model_dir
        .join(format!("decoder-epoch-99-avg-1-chunk-16-left-128{suffix}"));
    let joiner_path = args
        .model_dir
        .join(format!("joiner-epoch-99-avg-1-chunk-16-left-128{suffix}"));

    let t_enc_load = Instant::now();
    let encoder_inf = tract_onnx::onnx()
        .model_for_path(&encoder_path)
        .with_context(|| format!("ingest {}", encoder_path.display()))?;
    let n_inputs = encoder_inf.inputs.len();
    let n_outputs = encoder_inf.outputs.len();
    let enc_ingest_ms = t_enc_load.elapsed().as_millis();
    eprintln!(
        "encoder ingest OK ({} inputs, {} outputs, {} ms)",
        n_inputs, n_outputs, enc_ingest_ms
    );

    // Attempt optimisation. With symbolic batch dim N this may fail —
    // streaming Zipformer typically needs N=1 fixed before .into_optimized().
    let t_opt = Instant::now();
    let opt_result = encoder_inf.into_optimized();
    match &opt_result {
        Ok(_) => eprintln!(
            "encoder into_optimized() OK ({} ms)",
            t_opt.elapsed().as_millis()
        ),
        Err(e) => eprintln!(
            "encoder into_optimized() FAILED ({} ms): {e}",
            t_opt.elapsed().as_millis()
        ),
    }

    let t_dec_load = Instant::now();
    let decoder_inf = tract_onnx::onnx()
        .model_for_path(&decoder_path)
        .with_context(|| format!("ingest {}", decoder_path.display()))?;
    let dec_ingest_ms = t_dec_load.elapsed().as_millis();
    eprintln!(
        "decoder ingest OK ({} inputs, {} outputs, {} ms)",
        decoder_inf.inputs.len(),
        decoder_inf.outputs.len(),
        dec_ingest_ms
    );

    let t_join_load = Instant::now();
    let joiner_inf = tract_onnx::onnx()
        .model_for_path(&joiner_path)
        .with_context(|| format!("ingest {}", joiner_path.display()))?;
    let join_ingest_ms = t_join_load.elapsed().as_millis();
    eprintln!(
        "joiner ingest OK ({} inputs, {} outputs, {} ms)",
        joiner_inf.inputs.len(),
        joiner_inf.outputs.len(),
        join_ingest_ms
    );

    let total_ms = t0.elapsed().as_millis();
    eprintln!("\nspike-tract-zipformer probe summary:");
    eprintln!(
        "  encoder ingest: {} ms ({} inputs, {} outputs)",
        enc_ingest_ms, n_inputs, n_outputs
    );
    eprintln!("  decoder ingest: {} ms", dec_ingest_ms);
    eprintln!("  joiner ingest:  {} ms", join_ingest_ms);
    eprintln!(
        "  encoder optimize: {}",
        if opt_result.is_ok() {
            "OK"
        } else {
            "FAILED (symbolic dim N would need fixing first)"
        }
    );
    eprintln!("  total: {} ms", total_ms);
    eprintln!();
    eprintln!("see SPIKE.md candidate-2 section for the full prototype scope analysis.");
    Ok(())
}
