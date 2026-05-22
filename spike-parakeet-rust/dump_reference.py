#!/usr/bin/env python3
# Spike #871: dump reference activations from the fork at known intermediate points
# so a Rust candle stub can be checked against them.
# Throwaway.

import os
from pathlib import Path

import mlx.core as mx
import numpy as np
from parakeet_mlx import from_pretrained
from parakeet_mlx.audio import get_logmel

OUT_DIR = Path(__file__).parent / "parity" / "reference"
OUT_DIR.mkdir(parents=True, exist_ok=True)

# Reference fixture: 5s of audio
FIXTURE = Path(__file__).resolve().parents[3] / "tests" / "fixtures" / "voice" / "short_en.wav"
print(f"fixture: {FIXTURE}")
assert FIXTURE.exists()

model = from_pretrained("mlx-community/parakeet-tdt-0.6b-v2")

# Load + mel + take first 5s of audio (= 500 frames at 10ms hop after mel)
import librosa
audio, _ = librosa.load(str(FIXTURE), sr=model.preprocessor_config.sample_rate)
audio = audio[: 5 * model.preprocessor_config.sample_rate]
audio_mx = mx.array(audio)

# fork: get_logmel takes mx array and the preprocessor config; outputs (1, T, n_mels)
mel = get_logmel(audio_mx, model.preprocessor_config)
mel_np = np.array(mel)
np.save(OUT_DIR / "00_input_mel.npy", mel_np)
print(f"mel:           {mel.shape} dtype={mel.dtype}")

# Run the encoder up to the input of layer 0 (== pre_encode output)
# The encoder's pre_encode is a subsampling stack; we want its output.
# Then take block 0's FFN1 path.

enc = model.encoder

# pre_encode: takes (B, T, n_mels) and lengths, outputs (B, T', d_model) + new_lengths
lengths = mx.array([mel.shape[1]])
pre_out, new_lengths = enc.pre_encode(mel, lengths)
mx.eval(pre_out)
np.save(OUT_DIR / "01_pre_encode_out.npy", np.array(pre_out))
print(f"pre_encode:    {pre_out.shape} dtype={pre_out.dtype}")

# Block 0 FFN1 sub-path: x + 0.5 * feed_forward1(norm_feed_forward1(x))
block = enc.layers[0]
x = pre_out
norm = block.norm_feed_forward1(x)
mx.eval(norm)
np.save(OUT_DIR / "02_block0_norm_ff1.npy", np.array(norm))
print(f"norm_ff1:      {norm.shape}")

ff1_out = block.feed_forward1(norm)
mx.eval(ff1_out)
np.save(OUT_DIR / "03_block0_ff1_out.npy", np.array(ff1_out))
print(f"ff1_out:       {ff1_out.shape}")

after_ff1_residual = x + 0.5 * ff1_out
mx.eval(after_ff1_residual)
np.save(OUT_DIR / "04_block0_after_ff1_residual.npy", np.array(after_ff1_residual))
print(f"after_ff1_res: {after_ff1_residual.shape}")

# Bonus: dump the LayerNorm/Linear weights from the model object so the Rust side
# can confirm it pulled identical numbers from our converted safetensors.
np.save(OUT_DIR / "wt_norm_ff1_weight.npy", np.array(block.norm_feed_forward1.weight))
np.save(OUT_DIR / "wt_norm_ff1_bias.npy", np.array(block.norm_feed_forward1.bias))
np.save(OUT_DIR / "wt_ff1_linear1_weight.npy", np.array(block.feed_forward1.linear1.weight))
np.save(OUT_DIR / "wt_ff1_linear2_weight.npy", np.array(block.feed_forward1.linear2.weight))
# Linear biases may or may not exist depending on use_bias
if hasattr(block.feed_forward1.linear1, "bias") and block.feed_forward1.linear1.bias is not None:
    np.save(OUT_DIR / "wt_ff1_linear1_bias.npy", np.array(block.feed_forward1.linear1.bias))
if hasattr(block.feed_forward1.linear2, "bias") and block.feed_forward1.linear2.bias is not None:
    np.save(OUT_DIR / "wt_ff1_linear2_bias.npy", np.array(block.feed_forward1.linear2.bias))

print(f"\nartefacts in {OUT_DIR}")
for p in sorted(OUT_DIR.iterdir()):
    print(f"  {p.name}  ({p.stat().st_size / 1e6:.2f} MB)")
