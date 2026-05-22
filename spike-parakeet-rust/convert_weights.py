#!/usr/bin/env python3
# Spike #871: convert mlx-community/parakeet-tdt-0.6b-v2 safetensors -> candle-friendly safetensors.
# Throwaway. Not production.

import argparse
import glob
import json
import os
from pathlib import Path

import numpy as np
from safetensors import safe_open
from safetensors.numpy import save_file


def find_source() -> Path:
    matches = glob.glob(
        os.path.expanduser(
            "~/.cache/huggingface/hub/models--mlx-community--parakeet-tdt-0.6b-v2/snapshots/*/model.safetensors"
        )
    )
    if not matches:
        raise SystemExit("Source model not found in HF cache. Run `huggingface-cli download mlx-community/parakeet-tdt-0.6b-v2` first.")
    return Path(matches[0])


# Conv weight axis convention: MLX uses (out, kH..., in); candle/PyTorch uses (out, in, kH...).
# Permute by moving the last axis to position 1.
def conv_to_candle(arr: np.ndarray) -> np.ndarray:
    if arr.ndim == 3:
        # Conv1d (out, k, in) -> (out, in, k)
        return arr.transpose(0, 2, 1).copy()
    if arr.ndim == 4:
        # Conv2d (out, kH, kW, in) -> (out, in, kH, kW)
        return arr.transpose(0, 3, 1, 2).copy()
    return arr


CONV_KEY_SUFFIXES = (
    "encoder.pre_encode.conv.",        # 5 conv2d layers
    ".conv.depthwise_conv.weight",
    ".conv.pointwise_conv1.weight",
    ".conv.pointwise_conv2.weight",
)


def is_conv_weight(key: str) -> bool:
    if key.startswith("encoder.pre_encode.conv.") and key.endswith(".weight"):
        return True
    return any(key.endswith(s) for s in CONV_KEY_SUFFIXES if s.startswith("."))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="candle_weights.safetensors")
    ap.add_argument("--mapping-out", default="weight_mapping.json")
    args = ap.parse_args()

    src = find_source()
    print(f"source: {src}  ({src.stat().st_size / 1e6:.1f} MB)")

    mapping = []  # one entry per tensor
    out_tensors = {}

    with safe_open(src, framework="numpy") as f:
        keys = list(f.keys())
        print(f"reading {len(keys)} tensors...")
        for k in keys:
            t = f.get_tensor(k)
            dst_name = k  # identity rename — MLX names already match what we'd give the candle module tree
            transform = "identity"
            t_out = t
            if is_conv_weight(k):
                t_out = conv_to_candle(t)
                transform = f"conv_permute {tuple(t.shape)} -> {tuple(t_out.shape)}"
            elif k.endswith("running_mean") or k.endswith("running_var"):
                # BatchNorm running stats — keep as-is
                transform = "identity (batch_norm running stat)"
            mapping.append({
                "src": k,
                "dst": dst_name,
                "src_shape": list(t.shape),
                "dst_shape": list(t_out.shape),
                "dtype": str(t.dtype),
                "transform": transform,
            })
            out_tensors[dst_name] = t_out

    save_file(out_tensors, args.out)
    with open(args.mapping_out, "w") as f:
        json.dump(mapping, f, indent=2)
    out_size = Path(args.out).stat().st_size / 1e6
    print(f"wrote {len(out_tensors)} tensors to {args.out} ({out_size:.1f} MB)")
    print(f"wrote mapping to {args.mapping_out}")

    # Categorical summary
    cats = {}
    for m in mapping:
        cats.setdefault(m["transform"].split()[0], 0)
        cats[m["transform"].split()[0]] += 1
    print("transform summary:")
    for k, v in sorted(cats.items()):
        print(f"  {v:4d}  {k}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
