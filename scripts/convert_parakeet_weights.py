#!/usr/bin/env python3
# Converts mlx-community/parakeet-tdt-0.6b-v2 safetensors into a candle-
# friendly safetensors file.
#
# MLX stores Conv1d weights as (out, k, in) and Conv2d as (out, kH, kW, in);
# candle/PyTorch expect (out, in, k) and (out, in, kH, kW). All other
# tensors are identity-renamed: the MLX checkpoint key paths already match
# the natural candle module tree (proved by the #871 feasibility spike).
#
# Lift of spike-parakeet-rust/convert_weights.py from the #871 branch,
# polished into a stable script for `omni-dev voice install-model
# parakeet-tdt-0.6b-v2`. Production deltas vs spike:
#   * argparse for --src / --out / --mapping-out (no HF cache glob)
#   * SHA-256 of output written to {out}.sha256
#   * structured stdout: every line prefixed `PARAKEET-CONVERT:` so
#     install-model can grep-log progress without parsing the rest
#   * atomic write via .part + rename
#   * expected-tensor-count assertion (697 total, 77 conv permutes) so a
#     future upstream re-shape fails loudly rather than silently
#
# Requires: python3 >= 3.9, numpy, safetensors.

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

import numpy as np
from safetensors import safe_open
from safetensors.numpy import save_file

EXPECTED_TENSOR_COUNT = 697
EXPECTED_CONV_PERMUTE_COUNT = 77
LOG_PREFIX = "PARAKEET-CONVERT:"


def log(msg: str) -> None:
    print(f"{LOG_PREFIX} {msg}", flush=True)


def conv_to_candle(arr: np.ndarray) -> np.ndarray:
    if arr.ndim == 3:
        # Conv1d (out, k, in) -> (out, in, k)
        return arr.transpose(0, 2, 1).copy()
    if arr.ndim == 4:
        # Conv2d (out, kH, kW, in) -> (out, in, kH, kW)
        return arr.transpose(0, 3, 1, 2).copy()
    return arr


def is_conv_weight(key: str) -> bool:
    if key.startswith("encoder.pre_encode.conv.") and key.endswith(".weight"):
        return True
    return key.endswith((
        ".conv.depthwise_conv.weight",
        ".conv.pointwise_conv1.weight",
        ".conv.pointwise_conv2.weight",
    ))


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", required=True, type=Path,
                    help="Path to the upstream model.safetensors")
    ap.add_argument("--out", required=True, type=Path,
                    help="Path to write the candle-friendly safetensors")
    ap.add_argument("--mapping-out", type=Path, default=None,
                    help="Optional path to write the per-tensor mapping JSON")
    args = ap.parse_args()

    if not args.src.is_file():
        log(f"error: source file not found: {args.src}")
        return 2

    src_size_mb = args.src.stat().st_size / 1e6
    log(f"reading source: {args.src} ({src_size_mb:.1f} MB)")

    mapping = []
    out_tensors = {}
    conv_permute_count = 0

    with safe_open(args.src, framework="numpy") as f:
        keys = list(f.keys())
        log(f"reading {len(keys)} tensors")
        for k in keys:
            t = f.get_tensor(k)
            transform = "identity"
            t_out = t
            if is_conv_weight(k):
                t_out = conv_to_candle(t)
                transform = f"conv_permute {tuple(t.shape)} -> {tuple(t_out.shape)}"
                conv_permute_count += 1
            elif k.endswith(("running_mean", "running_var")):
                transform = "identity (batch_norm running stat)"
            mapping.append({
                "src": k,
                "dst": k,
                "src_shape": list(t.shape),
                "dst_shape": list(t_out.shape),
                "dtype": str(t.dtype),
                "transform": transform,
            })
            out_tensors[k] = t_out

    if len(out_tensors) != EXPECTED_TENSOR_COUNT:
        log(f"error: expected {EXPECTED_TENSOR_COUNT} tensors, got {len(out_tensors)} — upstream model may have changed shape")
        return 3
    if conv_permute_count != EXPECTED_CONV_PERMUTE_COUNT:
        log(f"error: expected {EXPECTED_CONV_PERMUTE_COUNT} conv permutes, got {conv_permute_count} — conv-weight key set may have changed")
        return 3

    log(f"permuting {conv_permute_count} conv tensors")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    out_part = args.out.with_suffix(args.out.suffix + ".part")
    log(f"writing {len(out_tensors)} tensors to {out_part}")
    save_file(out_tensors, str(out_part))
    os.replace(out_part, args.out)

    out_size_mb = args.out.stat().st_size / 1e6
    log(f"wrote {args.out} ({out_size_mb:.1f} MB)")

    digest = sha256_file(args.out)
    sha_path = args.out.with_suffix(args.out.suffix + ".sha256")
    sha_path.write_text(f"{digest}  {args.out.name}\n")
    log(f"sha256: {digest} -> {sha_path}")

    if args.mapping_out is not None:
        args.mapping_out.parent.mkdir(parents=True, exist_ok=True)
        with args.mapping_out.open("w") as f:
            json.dump(mapping, f, indent=2)
        log(f"wrote mapping to {args.mapping_out}")

    log("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
