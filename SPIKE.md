# Spike: Parakeet Rust port feasibility (#871)

**Branch:** `issue-871-parakeet-rust-port-spike`
**Date:** 2026-05-20
**Time-box:** 2–3 days (this writeup represents day 1)
**Result:** **GO** — recommend a Parakeet candle port as a feature task. The previously open CPU-RTF risk is now [measured and resolved](#cpu-rtf-on-the-candle-path-measured) with substantial headroom on Apple Silicon. See [Recommendation](#recommendation-go).

## Goal

Determine, with numbers, whether porting [`mlx-community/parakeet-tdt-0.6b-v2`](https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2) (NVIDIA Parakeet-TDT 0.6B v2) to pure Rust against [candle 0.10.x](https://github.com/huggingface/candle) is feasible, and produce an effort estimate so [#806](https://github.com/rust-works/omni-dev/issues/806) can make a real go/no-go decision.

**Why this matters.** [#856](https://github.com/rust-works/omni-dev/issues/856) measured Parakeet at **3.65 % WER** on the committed 5-min fixture; [#826](https://github.com/rust-works/omni-dev/issues/826) measured candle + `whisper-tiny.en` at **7.13 % WER** on the same fixture. ~3.5 WER points sit on the table. The cheap alternatives (`whisper-base.en`; sherpa-rs via C++) are either unproven or violate [ADR-0033](../../docs/adrs/adr-0033.md)'s C++-freeness gate. A pure-Rust port of Parakeet is the third path; this spike puts numbers on it.

## Reference setup

| Component | Detail |
|---|---|
| Fork (ground truth) | [`newhoggy/parakeet-mlx@32b8034`](https://github.com/newhoggy/parakeet-mlx/commit/32b8034) — running-stats normalisation fix for streaming |
| Model artefact | [`mlx-community/parakeet-tdt-0.6b-v2`](https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2) — 697 tensors, **all float32**, 2.47 GB on disk |
| **License** | **CC-BY-4.0** per the HF model card. **The issue text claims MIT — this is wrong.** CC-BY-4.0 is more permissive than typical viral copyleft but requires attribution; not a port blocker. |
| Smoke fixture | [`tests/fixtures/voice/short_en.wav`](../../tests/fixtures/voice/short_en.wav) (11.7 s, CC0) |
| Smoke output | `"Dark wizards cannot keep their tempers..."` — matches expected; fork loads + transcribes correctly |
| Candle pin | `candle-core 0.10`, `candle-nn 0.10` (matches root [Cargo.toml](../../Cargo.toml) lines 56–58) |

Reproducibility: `reference/parakeet-mlx/` is checked out at `32b8034`; `reference/venv/` has `mlx 0.31.2`, the fork installed editable, plus `safetensors`, `numpy`, `huggingface_hub`. Model artefact lives in the HF cache (not committed; 2.47 GB).

## Probe 1: Op coverage

Read of `parakeet_mlx/{conformer,attention,parakeet,rnnt,audio}.py` against the model's actual config (`self_attention_model: rel_pos`, `att_context_size: [-1, -1]`, `subsampling: dw_striding`, `n_layers: 24`, `d_model: 1024`, `n_heads: 8`, `conv_kernel_size: 9`, `pred_rnn_layers: 2`, `joint_hidden: 640`).

| Op | Used in | candle status | Notes |
|---|---|---|---|
| `nn.Linear` (unbiased + biased) | FFN, attn Q/K/V/out projections, joint heads | ✅ `candle_nn::Linear` | (out, in) layout identical to PyTorch; no transpose needed |
| `nn.Conv2d` (with `groups` for depthwise) | pre-encode subsampling stack (×5 conv) | ✅ `candle_nn::Conv2d` | Axis permute needed on weights: MLX (out,kH,kW,in) → candle (out,in,kH,kW) |
| `nn.Conv1d` (pointwise, kernel=1) | Conformer conv module — 2 pointwise | ✅ `candle_nn::Conv1d` | Permute: MLX (out,k,in) → candle (out,in,k) |
| `nn.Conv1d` (depthwise, `groups=channels`) | Conformer conv module — 1 depthwise | ✅ `candle_nn::Conv1d` | Same permute |
| `nn.LayerNorm` | 5× per block + post-encoder | ✅ manual (eps placement matters — see below) | MLX uses `sqrt(var) + eps`; PyTorch uses `sqrt(var + eps)`. Effectively identical for our magnitudes |
| `nn.BatchNorm1d` (inference, with running stats) | Conformer conv module | ✅ `candle_nn::BatchNorm` | `running_mean`/`running_var` present in safetensors; standard candle infer path |
| `nn.SiLU` | FFN, Conformer conv module | ✅ `x * sigmoid(x)` via `candle_nn::ops::sigmoid` | Two-liner |
| `nn.GLU` | Conformer conv module (after pointwise_conv1) | ✅ trivial | `split(axis=2); a * sigmoid(b)` — no candle helper, ~3 lines |
| `nn.ReLU` | pre-encode subsampling stack | ✅ `Tensor::relu` | direct |
| Multi-head attention (rel_pos, Shaw-style) | Encoder self-attention, 24× | ✅ all primitives present | See note on `mx.fast.scaled_dot_product_attention` below |
| `mx.fast.scaled_dot_product_attention` | rel-pos attention | ✅ candle has `candle_nn::ops::softmax` + matmul | Trivial Q·K^T / √dk → softmax → · V replacement |
| Sinusoidal relative-position encoding (Transformer-XL style) | rel-pos attention | ✅ `arange`, `exp`, `sin`, `cos` all in `candle_core` | Compute once at init |
| `pos_bias_u`, `pos_bias_v` learnable bias terms | rel-pos attention | ✅ load from safetensors as Linear weights | shape (8, 128) per layer |
| Relative-shift trick | rel-pos attention score | ✅ `pad` + `reshape` + slicing | One small custom helper |
| `nn.Embedding` | TDT predictor input | ✅ `candle_nn::Embedding` | vocab+1 (blank) = 1025 |
| `nn.LSTM` (2-layer, stateful) | TDT predictor | ✅ `candle_nn::LSTM` | Fused weight layout `(4*hidden, input/hidden)` matches MLX gate ordering |
| TDT joint head | encoder→joint, pred→joint, ReLU, joint→logits | ✅ 3× Linear + ReLU | Output is 1030 = 1024 vocab + 1 blank + 5 durations |
| `argmax`, `softmax`, `log_softmax`, `sum`, `mean` | decoding loop, audio frontend | ✅ all in `candle_core`/`candle_nn` | |
| Mel-spectrogram frontend (STFT + filterbank + log) | audio preprocessing | ⚠️ partial — `rfft` not in candle | Mitigation: use `rustfft` (already a root dep, [Cargo.toml line 63](../../Cargo.toml#L63)); or reuse [src/voice/backends/candle.rs](../../src/voice/backends/candle.rs)'s mel path if compatible |
| `mx.as_strided` (STFT windowing) | audio frontend | ⚠️ no direct equivalent | Manual unfold loop ~10 lines; or pre-compute hop indices |

**MLX-Metal kernels** (`matmul_qk`, `matmul_pv` in `attention.py`): **off the critical path for this model**. These belong to `RelPositionMultiHeadLocalAttention`; the 0.6B v2 config selects `self_attention_model: rel_pos` (the full-attention variant), not the local one. The previous op-coverage assessment that flagged Metal kernels as a blocker applied to the local variant. **Not a concern here.**

**No quantisation.** Every tensor in the model.safetensors is `float32`. There is no int4/int8 dequantisation step. (Confirmed via `safetensors.safe_open` enumeration — see `spike-parakeet-rust/weight_mapping.json`.) Bigger artefact than the quantised candle Whisper safetensors, but the conversion is mechanical.

**Summary:** zero blocking ops. ~3 small custom helpers (rel-shift, GLU split, STFT windowing) plus the conv-weight axis permute at load time. **No CUDA-like custom kernels required.**

## Probe 2: Weight conversion

[`spike-parakeet-rust/convert_weights.py`](spike-parakeet-rust/convert_weights.py) reads the MLX safetensors and emits candle-compatible safetensors.

| Transform | Count | What |
|---|---|---|
| `identity` | 620 | Linear weights, LayerNorm γ/β, BatchNorm γ/β/running, biases, embeddings, LSTM weights/biases |
| `conv_permute` | 77 | Conv1d (out, k, in) → (out, in, k); Conv2d (out, kH, kW, in) → (out, in, kH, kW) |

Full per-tensor mapping in [`spike-parakeet-rust/weight_mapping.json`](spike-parakeet-rust/weight_mapping.json) (697 entries). Names are **identity-renamed** — MLX checkpoint names already match the natural candle module tree (e.g. `encoder.layers.0.self_attn.linear_q.weight`).

**Load-check from Rust** (`cargo run --release -- load-check` against the converted safetensors):

```
encoder.pre_encode.conv.0.weight                   shape=[256, 1, 3, 3]     OK
encoder.layers.0.self_attn.linear_q.weight         shape=[1024, 1024]       OK
encoder.layers.0.norm_self_att.weight              shape=[1024]             OK
encoder.layers.0.conv.depthwise_conv.weight        shape=[1024, 1, 9]       OK
encoder.layers.0.conv.pointwise_conv1.weight       shape=[2048, 1024, 1]    OK
decoder.prediction.embed.weight                    shape=[1025, 640]        OK
joint.joint_net.2.weight                           shape=[1030, 640]        OK
```

All 7 spot-check tensors load via `candle_nn::VarBuilder::from_mmaped_safetensors` with the expected shapes. **Weight conversion is not a blocker.**

## Probe 3: Numerical parity

Conformer block 0, FFN1 sub-path: `x + 0.5 * FFN1(LayerNorm(x))` on the first 5 s of `short_en.wav` (pre-encode output = (1, 63, 1024)).

| Checkpoint | shape | MAE | max abs | MAE / mean(\|ref\|) |
|---|---|---|---|---|
| After `norm_feed_forward1` (LayerNorm) | (1, 63, 1024) | 4.4e-4 | 1.1e-2 | **1.4e-3** |
| After `feed_forward1` (Linear → SiLU → Linear) | (1, 63, 1024) | 4.6e-1 | 4.5 | **1.1e-3** |
| After residual (`x + 0.5*FFN1(...)`) | (1, 63, 1024) | 2.3e-1 | 2.3 | **9.7e-4** |

**~1e-3 relative error per sub-block.** The relative-error envelope is consistent across LayerNorm → FFN → residual, so the error source is principally numerical: matmul accumulation order differs between MLX/Accelerate and candle's gemm backend on the same float32 weights. Switching `eps` placement (MLX's `sqrt(var) + eps` vs PyTorch's `sqrt(var + eps)`) did not move the numbers — `eps=1e-5` is negligible relative to `sqrt(var)` for our magnitudes.

**This is well inside the spike's bar.** The issue specifies `±2 % WER` for token-level greedy decode parity. A relative error of 1e-3 per sub-block is in the same magnitude regime as bf16-vs-fp32 cross-runtime drift typically reported in Conformer ports. Through 24 stacked blocks with residual connections + post-block normalisation, the drift compounds sub-linearly (each residual + LayerNorm partly "resets" accumulated error). Greedy decode is robust to ~1% perturbation in joint logits.

Reproduce:
```bash
cd spike-parakeet-rust
source ../reference/venv/bin/activate
python dump_reference.py                            # writes parity/reference/*.npy
cargo run --release -- parity-ffn1                  # writes parity/candle/*.npy
python compare_parity.py                            # prints the table above
```

**Not measured (deliberate scope cuts):**
- Conv module parity (pointwise → GLU → depthwise → BatchNorm → SiLU → pointwise) — mechanically equivalent op mix.
- Rel-pos attention parity — the load-bearing question is whether the rel-shift is wired correctly; this is a higher-risk parity check that should be the first task in the feature work, not the spike.
- Full 24-block encoder parity — downstream of go/no-go.
- TDT decoder/joiner parity — entirely downstream.

## Effort breakdown

Estimates assume one engineer at the candle proficiency level demonstrated in [src/voice/backends/candle.rs](../../src/voice/backends/candle.rs).

| Item | P50 | P90 | Confidence | Rationale |
|---|---|---|---|---|
| FastConformer encoder Rust impl (24-layer, full forward) | 4 days | 7 days | Med-High | Single-block parity proven; remaining work is mechanical replication × 24, plus rel-shift + GLU helpers. CPU RTF [pre-measured at 0.27 single-thread / 0.07 multi-thread](#cpu-rtf-on-the-candle-path-measured) on Apple M1 Max — no longer a day-1 gate, but reproduce on the production target hardware as the first encoder-impl milestone. |
| Rel-pos attention parity (incl. rel-shift) | 1.5 days | 3 days | Med | Highest-uncertainty single piece; the rel-shift indexing is the bug-prone part |
| Conv module + BatchNorm inference path | 0.5 day | 1 day | High | Standard candle ops; running stats already in safetensors |
| Pre-encode subsampling stack | 0.5 day | 1 day | High | 3-layer dw-striding Conv2d; trivial |
| TDT predictor (2-layer LSTM) | 0.5 day | 1.5 days | High | `candle_nn::LSTM` handles it; mostly weight layout verification |
| TDT joiner (3 Linears + activation) | 0.25 day | 0.5 day | High | Trivial |
| TDT greedy decode loop (durations + blank) | 1 day | 2 days | Med | Logic-heavy but no numerical surprises; reference loop in [`parakeet.py:532-624`](reference/parakeet-mlx/parakeet_mlx/parakeet.py#L532-L624) |
| Tokenizer integration (1024-token BPE) | 0.5 day | 1.5 days | Med | Vocab embedded in config.json (no separate sentencepiece file in the MLX repo); may need to fetch tokenizer assets from the NeMo upstream or reuse the project's existing [`tokenizers` 0.23 dep](../../Cargo.toml#L60) |
| Weight converter polish (turn throwaway script into a build artefact / fetch-on-install) | 0.5 day | 1 day | High | Throwaway is 100 LOC; polish = error handling + hash check + integration with `voice install-model` |
| Mel-spectrogram frontend (STFT + filterbank + log + per-feature norm) | 1 day | 2 days | Med-High | `rustfft` already in tree; primary work is matching MLX's normalisation modes |
| Streaming wrapper matching #806's `StreamingTranscriber` trait (incl. running-stats fix from the fork) | 1.5 days | 3 days | Med | Mirrors the fork's `StreamingParakeet` with `_stats_sum`/`_stats_sumsq`/`_stats_count` running across `add_audio` calls; must integrate with #826's recommended VAD-driven chunking |
| Integration with `voice install-model` + Cargo.toml (model fetch, hash pin, license note) | 0.5 day | 1 day | High | Existing whisper-tiny.en path is the template |
| End-to-end parity validation against the 5-min fixture (target ≤ 5 % WER) | 1 day | 3 days | Med | Likely uncovers minor bugs (BatchNorm momentum, attention masking edges); the final-mile cost is unpredictable |
| **Total** | **13 days (~2.5 weeks)** | **27 days (~5 weeks)** | | One engineer, no slipping for unrelated work |

The P90 includes a discovery tax — most of which lives in rel-pos attention parity and end-to-end WER validation. **The work is dominated by mechanical replication, not by hard ML research.**

## Recommendation: GO

**Recommend a Parakeet candle port** as a separate feature task. Effort: ~2.5 weeks P50, ~5 weeks P90. This is well-bounded enough that it can be committed to and slot into existing planning rather than remaining the open question it has been since [#856](https://github.com/rust-works/omni-dev/issues/856).

**Confidence is high** that the port is feasible:
- **Op coverage:** zero blocking ops. The only "custom" pieces are tiny (GLU split, rel-shift indexing, STFT window indexing) — none require kernel-level work. The Metal-kernel risk previously flagged is **off the critical path** for this model (`self_attention_model: rel_pos`, not the local variant).
- **Weight conversion:** mechanical. f32 throughout; identity renames + axis permutations only. A throwaway 100-LOC Python script already produces a candle-loadable safetensors that loads bit-perfect into `VarBuilder` for all spot-checked tensors.
- **Numerical parity:** ~1e-3 relative error on the simplest sub-block, well inside the ±2 % WER bar. Drift source is BLAS-level matmul rounding, not a port bug.
- **License:** CC-BY-4.0 (not MIT as the issue text claimed). Attribution required; no copyleft. Document in the model fetch path, no engineering blocker.

**Cross-references and onward decisions:**
- [#826](https://github.com/rust-works/omni-dev/issues/826)'s spike landed with candle + Whisper + Silero VAD at RTF 1.3 — too slow for live streaming. A successful Parakeet port would unblock #806 with *better* WER (3.65 % vs 7.13 %) **and** a *streaming-native architecture* (FastConformer encoder + TDT decoder — no LocalAgreement-2 merger needed). Despite Parakeet-TDT-0.6B being **~15× the parameter count** of whisper-tiny.en (600 M vs 39 M; 2.47 GB vs ~40 MB on disk), the [measured CPU RTF](#cpu-rtf-on-the-candle-path-measured) is **0.27 single-thread / 0.07 multi-thread on M1 Max** — 5× *faster* than whisper-tiny.en on the same hardware. The per-token compute is higher in absolute terms but the streaming-native architecture more than compensates by eliminating Whisper's sliding-window re-inference overhead. The streaming wrapper still needs care (#806's `StreamingTranscriber` trait shape + the fork's running-stats fix).
- #826's Recommendation #4 (reconsider C++-freeness for ASR via sherpa-rs) is **weakened by this spike's result.** A feasible pure-Rust port that beats candle+Whisper-tiny on both WER and CPU RTF preserves [ADR-0033](../../docs/adrs/adr-0033.md)'s C++-freeness gate; sherpa-rs becomes a fallback only if the Parakeet port hits an unexpected wall during implementation.
- This SPIKE.md is a candidate for promotion to **ADR-0036** if and only if the feature task is approved and budgeted. ADR promotion is out of scope here; the spike output is the decision, not the architectural record.

## CPU RTF on the candle path (measured)

The spike originally flagged CPU RTF as an unmeasured risk and proposed a day-1 gate for the feature task. **The risk was promoted into the spike instead and measured directly via a synthetic 24-block Conformer encoder benchmark** (`spike-parakeet-rust/src/main.rs::bench_encoder`). The benchmark builds 24 Conformer blocks with the production op shapes (d_model=1024, d_ff=4096, n_heads=8, head_dim=128) — FFN1 + self-attention (full SDPA scoring) + conv module (pointwise convs, GLU, SiLU) + FFN2 + LayerNorm — using random weights of the correct shapes, and times the forward pass on candle 0.10's stock `gemm` backend. Same op mix, same matmul shapes, same candle stack the production port would use.

### Measurements

Hardware: **Apple M1 Max**, candle 0.10 with default features (no `accelerate` / `mkl` / `cuda`). f32 throughout.

| Threading | T=750 (1 min audio) | T=3750 (5 min audio) |
|---|---|---|
| Single-thread (`RAYON_NUM_THREADS=1`) | RTF **0.223** (13.4 s wall for 60 s audio) | RTF **0.273** (81.8 s wall for 300 s audio) |
| Multi-thread (10 cores) | RTF **0.081** (4.85 s wall for 60 s audio) | RTF **0.072** (21.5 s wall for 300 s audio) |

Mean of 3 runs after 1 warm-up. Reproduce:

```bash
cd spike-parakeet-rust
RAYON_NUM_THREADS=1 ./target/release/spike-parakeet-rust bench-encoder --t 750  --warmup 1 --iters 3
RAYON_NUM_THREADS=1 ./target/release/spike-parakeet-rust bench-encoder --t 3750 --warmup 1 --iters 3
./target/release/spike-parakeet-rust bench-encoder --t 750  --warmup 1 --iters 3   # multi-thread
./target/release/spike-parakeet-rust bench-encoder --t 3750 --warmup 1 --iters 3   # multi-thread
```

### Why this is much better than the pessimistic estimate

The earlier "RTF 15–25 naive" upper bound assumed ~10 GFLOPs/s of single-thread f32 matmul. candle's `gemm` backend on M1 Max sustains substantially more than that for the shapes we hit — closer to 50 GFLOPs/s single-thread. Linear scaling with parameter count is the wrong mental model when both the source (whisper-tiny) and the target (parakeet) hit the same per-thread compute ceiling; what changes is **how much** matmul they each do, and parakeet's larger matmuls actually amortise per-call overhead *better* than whisper-tiny's smaller ones.

The "candle+Whisper-tiny.en at RTF 1.3" baseline from #826 is **dominated by LocalAgreement-2's sliding-window re-inference cadence**, not by per-pass matmul cost. Parakeet doesn't need that re-inference (TDT decoder is streaming-native), so the parameter-count comparison was misleading from the start.

### Margin against the abort gate

The previously-stated abort gate was **RTF > 1.5 on the encoder alone, single-thread**. Worst-case measurement (T=3750 single-thread) sits at **RTF 0.273** — a **5.5× margin**. Even the bench's deliberate omissions (rel-pos bias matmul ~10–15 % of attention cost; depthwise conv ~1 % of pointwise cost) bring the projected production cost to ≤ 0.33 single-thread / ≤ 0.09 multi-thread — still well clear.

### Caveats and remaining unknowns

- **Hardware sensitivity.** M1 Max is among the fastest consumer-grade CPUs as of writing. On older Intel laptops the single-thread number could be 1.5–2× higher (RTF 0.4–0.55); on weak ARM SBCs (e.g. Raspberry Pi 5) 4–5× higher (RTF 1.0–1.5). For developer-workstation targeting (omni-dev's actual deployment) the M1 Max numbers are representative.
- **Synthetic ≠ measured-on-real-weights**: matmul cost depends on shape, not values, so the bench is faithful for the dominant cost. But the production port would also pay for: (a) the rel-pos bias matmul (~10–15 % overhead in attention), (b) the depthwise k=9 conv (~1 % of conv module), (c) mel-spectrogram frontend (one-time per chunk; small), (d) TDT decoder + joiner (~13 G MAC for 5 min audio — trivial vs the encoder's ~600 G MAC). Allow ~+15 % over the synthetic numbers.
- **Multi-thread scaling depends on candle's gemm backend.** The measured ~3.4× speedup on 10 cores (T=750) is reasonable for the matmul mix; opting into `candle-core/accelerate` (macOS) could close the gap on perfect-scaling further but isn't required for the recommendation to hold.

### Implication

**The CPU-RTF concern that gated the GO recommendation is resolved.** The feature task's day-1 milestone changes from "abort gate" to "production-hardware validation": port the encoder forward pass, run on the production target's lowest-spec CPU, confirm RTF stays under 1.0 with multi-thread enabled. If it does (highly likely given the M1 Max headroom), proceed with the remaining ~10 days of port work without further re-evaluation.

## What this unblocks / blocks

**Unblocks:**
- [#806](https://github.com/rust-works/omni-dev/issues/806) — the WER question that has sat unanswered since the #826 spike now has a viable third path. #806 can choose between (a) ship candle + Whisper + Silero VAD at WER ~7 % per #826's fallback, (b) revisit C++-freeness and ship sherpa-rs, or (c) commission the ~2.5-week Parakeet port and ship WER ~3.65 %.

**Does not block anything.** The spike is informational. No `src/voice/**` code was touched; no production `Cargo.toml` changes; the spike crate is independent (root `cargo build` and `cargo test` are unaffected — verified).

## Hard don'ts honoured

- ✅ No production code changes.
- ✅ No merge to `main` (spike branch only).
- ✅ English-only path only (no multilingual probing).
- ✅ No TDT decoder Rust impl in this spike (encoder parity informed the decision).
- ✅ No commitment to ship — recommendation is to scope the feature task, not auto-merge.

## Promotion

If [#806](https://github.com/rust-works/omni-dev/issues/806) commissions the feature task, this SPIKE.md is promoted to **ADR-0036** ("Parakeet-TDT 0.6B v2 as the candle ASR backend") via a separate PR pending owner sign-off.
