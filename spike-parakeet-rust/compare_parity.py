#!/usr/bin/env python3
# Spike #871: compare MLX reference vs candle outputs at the FFN1 sub-path
# of conformer block 0. Reports MAE and max abs error per checkpoint.
# Throwaway.

from pathlib import Path
import numpy as np

REF = Path("parity/reference")
CAN = Path("parity/candle")

checkpoints = [
    ("02_block0_norm_ff1", "after LayerNorm (norm_feed_forward1)"),
    ("03_block0_ff1_out", "after FeedForward (linear1 + SiLU + linear2)"),
    ("04_block0_after_ff1_residual", "after residual: x + 0.5 * ff1_out"),
]

print(f"{'checkpoint':50} {'shape':16} {'mean_abs':>11} {'max_abs':>11} {'rel':>11}")
print("-" * 102)
for name, desc in checkpoints:
    rp = REF / f"{name}.npy"
    cp = CAN / f"{name}.npy"
    if not rp.exists() or not cp.exists():
        print(f"  {name}: MISSING ({rp.exists()=}, {cp.exists()=})")
        continue
    ref = np.load(rp)
    can = np.load(cp)
    if ref.shape != can.shape:
        print(f"  {name}: SHAPE MISMATCH ref={ref.shape} can={can.shape}")
        continue
    diff = ref.astype(np.float64) - can.astype(np.float64)
    mae = float(np.mean(np.abs(diff)))
    max_ae = float(np.max(np.abs(diff)))
    ref_abs_mean = float(np.mean(np.abs(ref)))
    rel = mae / (ref_abs_mean + 1e-12)
    print(f"{name:50} {str(ref.shape):16} {mae:11.4e} {max_ae:11.4e} {rel:11.4e}")
    # Print descriptive note
    print(f"  -> {desc}")
print("-" * 102)
print("rel = MAE / mean(|ref|)")
