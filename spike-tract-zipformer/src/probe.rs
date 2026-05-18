//! Load encoder/decoder/joiner ONNX and dump their I/O facts. Used to
//! confirm tract-onnx can ingest the streaming Zipformer graphs before
//! committing to the full prototype.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tract_onnx::prelude::*;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long, default_value_t = false)]
    int8: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let suffix = if args.int8 { ".int8.onnx" } else { ".onnx" };
    for stem in [
        "encoder-epoch-99-avg-1-chunk-16-left-128",
        "decoder-epoch-99-avg-1-chunk-16-left-128",
        "joiner-epoch-99-avg-1-chunk-16-left-128",
    ] {
        let path = args.model_dir.join(format!("{stem}{suffix}"));
        println!("\n=== {} ===", path.display());
        probe(&path)?;
    }
    Ok(())
}

fn probe(path: &std::path::Path) -> Result<()> {
    // Just parse — don't optimize — so we can see the original symbolic
    // dim names tract recovered.
    let model = tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("load {}", path.display()))?;

    println!("inputs ({}):", model.inputs.len());
    for (i, &id) in model.inputs.iter().enumerate() {
        let node = &model.nodes()[id.node];
        let fact = model
            .outlet_fact(id)
            .with_context(|| format!("outlet fact for input {i}"))?;
        println!("  [{i}] {}  fact={fact:?}", node.name);
    }
    println!("outputs ({}):", model.outputs.len());
    for (i, &id) in model.outputs.iter().enumerate() {
        let node = &model.nodes()[id.node];
        let fact = model
            .outlet_fact(id)
            .with_context(|| format!("outlet fact for output {i}"))?;
        println!("  [{i}] {}  fact={fact:?}", node.name);
    }
    Ok(())
}
