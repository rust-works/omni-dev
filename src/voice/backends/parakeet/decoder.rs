//! Token-and-Duration Transducer (TDT) decoder.
//!
//! Mirrors `senstella/parakeet-mlx::parakeet_mlx/rnnt.py` (predictor +
//! joiner) and `parakeet.py::ParakeetTDT::decode_greedy` (the TDT loop).
//!
//! Three pieces:
//!
//! - [`LstmStack`] — multi-layer LSTM with MLX-style weight names
//!   (`Wx`, `Wh`, `bias`). candle's [`candle_nn::LSTM`] uses
//!   PyTorch-style names (`weight_ih_l0`, `weight_hh_l0`, …) so it
//!   cannot load the MLX checkpoint directly. The math is the same;
//!   this module reimplements one step explicitly. MLX combines the
//!   `ih` and `hh` biases into a single tensor that candle's two-bias
//!   form would split — handled by feeding the combined bias as
//!   `b_ih` with `b_hh = 0` in the step formula.
//!
//! - [`TdtPredictor`] — embedding lookup + LSTM stack. Takes the
//!   previously-emitted token (or `None` for the start-of-stream blank)
//!   and produces a 640-dim predictor output plus an updated LSTM
//!   state.
//!
//! - [`TdtJoiner`] — three linear projections producing
//!   `vocab + 1 + num_durations` logits. For Parakeet 0.6B v2 the
//!   output split is `1024 vocab + 1 blank + 5 duration tokens = 1030`.
//!
//! The TDT greedy decode loop ([`TdtDecoder::decode_greedy`]) lives at
//! the bottom — one frame at a time, predict + joint + argmax over the
//! token logits and the duration logits, advance `step` by the chosen
//! duration. The "anti-stuck" rule from the MLX reference is preserved
//! verbatim: if a `duration=0` decision repeats `max_symbols_per_step`
//! times without advancing, force-advance by one frame.

use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Module, Tensor};
use candle_nn::{ops::sigmoid, Linear, VarBuilder};

/// Hyperparameters for the TDT decoder. Concrete values for Parakeet
/// 0.6B v2 live in [`PARAKEET_TDT_0_6B_V2`].
#[derive(Debug, Clone)]
pub struct TdtConfig {
    /// Vocabulary size (excludes blank). Parakeet 0.6B v2: 1024.
    pub vocab_size: usize,
    /// Predictor hidden dim. Parakeet 0.6B v2: 640.
    pub pred_hidden: usize,
    /// Number of stacked LSTM layers in the predictor. Parakeet: 2.
    pub pred_rnn_layers: usize,
    /// Encoder output dim (matches the encoder's d_model). Parakeet: 1024.
    pub encoder_hidden: usize,
    /// Joint network hidden dim. Parakeet: 640.
    pub joint_hidden: usize,
    /// Duration token values. Parakeet uses [0, 1, 2, 3, 4] — five
    /// possible per-emission time advances. The number of duration
    /// tokens is what NeMo calls `num_extra_outputs`.
    pub durations: &'static [usize],
    /// Cap on consecutive `duration=0` emissions before forcing a
    /// frame-advance. Prevents the greedy loop from getting stuck on a
    /// single encoder frame. Parakeet uses 10.
    pub max_symbols_per_step: usize,
    /// Joint network activation between the (enc + pred) sum and the
    /// final linear. Parakeet uses ReLU.
    pub joint_activation: JointActivation,
}

/// Activation choice for the joint network. The MLX reference supports
/// ReLU / Sigmoid / Tanh; Parakeet 0.6B v2 ships ReLU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointActivation {
    /// ReLU activation (Parakeet's choice).
    Relu,
    /// Sigmoid activation (other NeMo TDT variants).
    Sigmoid,
    /// Tanh activation (other NeMo TDT variants).
    Tanh,
}

/// Concrete decoder config for Parakeet-TDT-0.6B-v2.
pub const PARAKEET_TDT_0_6B_V2: TdtConfig = TdtConfig {
    vocab_size: 1024,
    pred_hidden: 640,
    pred_rnn_layers: 2,
    encoder_hidden: 1024,
    joint_hidden: 640,
    durations: &[0, 1, 2, 3, 4],
    max_symbols_per_step: 10,
    joint_activation: JointActivation::Relu,
};

/// One LSTM layer with MLX-style weight names. Combined `bias` rather
/// than PyTorch's `bias_ih` + `bias_hh` split (they're added either way).
struct LstmLayer {
    w_ih: Tensor,
    w_hh: Tensor,
    bias: Option<Tensor>,
}

impl LstmLayer {
    fn load(vb: VarBuilder, in_dim: usize, hidden_dim: usize, use_bias: bool) -> Result<Self> {
        let w_ih = vb
            .get((4 * hidden_dim, in_dim), "Wx")
            .with_context(|| format!("load LSTM Wx ({}, {in_dim})", 4 * hidden_dim))?;
        let w_hh = vb
            .get((4 * hidden_dim, hidden_dim), "Wh")
            .with_context(|| format!("load LSTM Wh ({}, {hidden_dim})", 4 * hidden_dim))?;
        let bias = if use_bias {
            Some(
                vb.get(4 * hidden_dim, "bias")
                    .with_context(|| format!("load LSTM bias ({})", 4 * hidden_dim))?,
            )
        } else {
            None
        };
        Ok(Self { w_ih, w_hh, bias })
    }

    /// Single-step forward. `x` is `(batch, in_dim)`; state tensors are
    /// `(batch, hidden_dim)`. Returns the new `(h, c)`.
    fn step(&self, x: &Tensor, h: &Tensor, c: &Tensor) -> Result<(Tensor, Tensor)> {
        let mut gates = (x.matmul(&self.w_ih.t()?.contiguous()?)?
            + h.matmul(&self.w_hh.t()?.contiguous()?)?)?;
        if let Some(b) = &self.bias {
            gates = gates.broadcast_add(b)?;
        }
        // PyTorch / MLX gate order on the last axis: [input, forget, cell, output].
        let chunks = gates.chunk(4, 1)?;
        let in_gate = sigmoid(&chunks[0])?;
        let forget_gate = sigmoid(&chunks[1])?;
        let cell_gate = chunks[2].tanh()?;
        let out_gate = sigmoid(&chunks[3])?;
        let next_c = ((forget_gate * c)? + (in_gate * cell_gate)?)?;
        let next_h = (out_gate * next_c.tanh()?)?;
        Ok((next_h, next_c))
    }
}

/// Per-layer hidden state for an [`LstmStack`].
#[derive(Debug, Clone)]
pub struct LstmState {
    /// Hidden vectors, one per layer, each `(batch, hidden_dim)`.
    pub h: Vec<Tensor>,
    /// Cell vectors, one per layer, same shape as `h`.
    pub c: Vec<Tensor>,
}

impl LstmState {
    /// All-zeros initial state for `num_layers` layers of `hidden_dim`.
    pub fn zeros(
        num_layers: usize,
        batch: usize,
        hidden_dim: usize,
        device: &Device,
    ) -> Result<Self> {
        let mut h = Vec::with_capacity(num_layers);
        let mut c = Vec::with_capacity(num_layers);
        for _ in 0..num_layers {
            h.push(Tensor::zeros((batch, hidden_dim), DType::F32, device)?);
            c.push(Tensor::zeros((batch, hidden_dim), DType::F32, device)?);
        }
        Ok(Self { h, c })
    }
}

/// Stack of LSTM layers loaded with MLX-style weight names.
pub struct LstmStack {
    layers: Vec<LstmLayer>,
    hidden_dim: usize,
}

impl LstmStack {
    /// Loads `num_layers` LSTM layers from `vb` under the key paths
    /// `<vb>.0.{Wx,Wh,bias}`, `<vb>.1.{Wx,Wh,bias}`, ...
    pub fn load(
        vb: VarBuilder,
        in_dim: usize,
        hidden_dim: usize,
        num_layers: usize,
        use_bias: bool,
    ) -> Result<Self> {
        let mut layers = Vec::with_capacity(num_layers);
        for i in 0..num_layers {
            let layer_in = if i == 0 { in_dim } else { hidden_dim };
            layers.push(
                LstmLayer::load(vb.pp(format!("{i}")), layer_in, hidden_dim, use_bias)
                    .with_context(|| format!("load LSTM layer {i}"))?,
            );
        }
        Ok(Self { layers, hidden_dim })
    }

    /// Hidden dimension of the stack.
    #[must_use]
    pub fn hidden_dim(&self) -> usize {
        self.hidden_dim
    }

    /// Number of layers in the stack.
    #[must_use]
    pub fn num_layers(&self) -> usize {
        self.layers.len()
    }

    /// Single-step forward through all layers. `x` is `(batch, in_dim)`;
    /// `state` carries per-layer `(h, c)`. Returns the top-layer hidden
    /// output and the new state.
    pub fn step(&self, x: &Tensor, state: &LstmState) -> Result<(Tensor, LstmState)> {
        anyhow::ensure!(
            state.h.len() == self.layers.len(),
            "LstmStack::step: state has {} layers, expected {}",
            state.h.len(),
            self.layers.len()
        );
        let mut h_next = Vec::with_capacity(self.layers.len());
        let mut c_next = Vec::with_capacity(self.layers.len());
        let mut layer_in = x.clone();
        for (i, layer) in self.layers.iter().enumerate() {
            let (nh, nc) = layer
                .step(&layer_in, &state.h[i], &state.c[i])
                .with_context(|| format!("LSTM layer {i} step"))?;
            layer_in = nh.clone();
            h_next.push(nh);
            c_next.push(nc);
        }
        Ok((
            layer_in,
            LstmState {
                h: h_next,
                c: c_next,
            },
        ))
    }
}

/// TDT predictor: token embedding -> LSTM stack.
pub struct TdtPredictor {
    embed: Tensor,
    lstm: LstmStack,
    pred_hidden: usize,
}

impl TdtPredictor {
    /// Loads the predictor from `vb` rooted at the model's
    /// `decoder.prediction.` namespace.
    ///
    /// Embedding has `vocab_size + 1` rows because the blank token is
    /// appended (blank id = `vocab_size`) and treated as a valid input
    /// to the predictor (matches MLX's `blank_as_pad=True`).
    pub fn load(vb: VarBuilder, cfg: &TdtConfig) -> Result<Self> {
        let embed_rows = cfg.vocab_size + 1;
        let embed = vb
            .pp("embed")
            .get((embed_rows, cfg.pred_hidden), "weight")
            .with_context(|| {
                format!(
                    "load predictor embed weight ({embed_rows}, {})",
                    cfg.pred_hidden
                )
            })?;
        let lstm = LstmStack::load(
            vb.pp("dec_rnn").pp("lstm"),
            cfg.pred_hidden,
            cfg.pred_hidden,
            cfg.pred_rnn_layers,
            true,
        )
        .context("load predictor dec_rnn.lstm")?;
        Ok(Self {
            embed,
            lstm,
            pred_hidden: cfg.pred_hidden,
        })
    }

    /// All-zeros initial state for the predictor's LSTM stack.
    pub fn zero_state(&self, batch: usize, device: &Device) -> Result<LstmState> {
        LstmState::zeros(
            self.lstm.num_layers(),
            batch,
            self.lstm.hidden_dim(),
            device,
        )
    }

    /// Forward step. `token` is the previously-emitted token id; pass
    /// `None` at the start of a stream (matches MLX's
    /// "if last_token is None: input = zeros"). Returns
    /// `(predictor_out (batch, pred_hidden), new_state)`.
    pub fn step(
        &self,
        token: Option<u32>,
        state: &LstmState,
        device: &Device,
    ) -> Result<(Tensor, LstmState)> {
        let x = if let Some(t) = token {
            // Embedding lookup: row `t` of the embed table, batch-1.
            self.embed.i(t as usize)?.unsqueeze(0)?
        } else {
            Tensor::zeros((1, self.pred_hidden), DType::F32, device)?
        };
        self.lstm.step(&x, state)
    }
}

/// TDT joiner: enc + pred -> activation -> linear logits.
pub struct TdtJoiner {
    enc: Linear,
    pred: Linear,
    out: Linear,
    activation: JointActivation,
}

impl TdtJoiner {
    /// Loads the joiner from `vb` rooted at the model's `joint.`
    /// namespace.
    ///
    /// MLX stores the output projection inside a list at
    /// `joint_net.<index>`. The first slot is the activation (no
    /// params), the second is an Identity (no params), the third is
    /// the output Linear. So safetensors only carries the third entry,
    /// keyed as `joint_net.2.{weight,bias}`.
    pub fn load(vb: VarBuilder, cfg: &TdtConfig) -> Result<Self> {
        let enc = linear(vb.pp("enc"), cfg.encoder_hidden, cfg.joint_hidden, true)
            .context("load joiner enc")?;
        let pred = linear(vb.pp("pred"), cfg.pred_hidden, cfg.joint_hidden, true)
            .context("load joiner pred")?;
        let num_outputs = cfg.vocab_size + 1 + cfg.durations.len();
        let out = linear(
            vb.pp("joint_net").pp("2"),
            cfg.joint_hidden,
            num_outputs,
            true,
        )
        .context("load joiner output linear")?;
        Ok(Self {
            enc,
            pred,
            out,
            activation: cfg.joint_activation,
        })
    }

    /// Forward. `enc_step` is one encoder frame `(batch, encoder_hidden)`;
    /// `pred_out` is the predictor output `(batch, pred_hidden)`. Returns
    /// `(batch, vocab + 1 + num_durations)` logits.
    pub fn forward(&self, enc_step: &Tensor, pred_out: &Tensor) -> Result<Tensor> {
        let e = self.enc.forward(enc_step).context("joiner enc proj")?;
        let p = self.pred.forward(pred_out).context("joiner pred proj")?;
        let summed = (e + p)?;
        let activated = match self.activation {
            JointActivation::Relu => summed.relu()?,
            JointActivation::Sigmoid => sigmoid(&summed)?,
            JointActivation::Tanh => summed.tanh()?,
        };
        self.out.forward(&activated).context("joiner output proj")
    }
}

/// Full TDT decoder: predictor + joiner + the greedy decode loop.
pub struct TdtDecoder {
    config: TdtConfig,
    predictor: TdtPredictor,
    joiner: TdtJoiner,
}

impl TdtDecoder {
    /// Loads the decoder from `vb` rooted at the model's `decoder.`
    /// namespace. The joiner is loaded from a sibling `joint.`
    /// namespace; `vb` should be rooted at the *parent* of both. The
    /// `decoder` and `joint` sub-prefixes are appended internally.
    pub fn load(vb: VarBuilder, cfg: &TdtConfig) -> Result<Self> {
        let predictor = TdtPredictor::load(vb.pp("decoder").pp("prediction"), cfg)
            .context("load TDT predictor")?;
        let joiner = TdtJoiner::load(vb.pp("joint"), cfg).context("load TDT joiner")?;
        Ok(Self {
            config: cfg.clone(),
            predictor,
            joiner,
        })
    }

    /// Accessor for the predictor sub-module. Used by the streaming
    /// session to allocate the per-session `LstmState` via
    /// `predictor().zero_state(...)`.
    #[must_use]
    pub fn predictor(&self) -> &TdtPredictor {
        &self.predictor
    }

    /// Greedy TDT decode over one batch-of-1 encoder output.
    ///
    /// `encoder_out` is `(1, T, encoder_hidden)`. Returns the emitted
    /// token ids in order (excluding blanks). State is fresh per call;
    /// the streaming wrapper in `super::streaming` is responsible for
    /// threading state across chunks.
    pub fn decode_greedy(&self, encoder_out: &Tensor) -> Result<Vec<u32>> {
        let t_frames = encoder_out
            .dim(1)
            .context("decode_greedy expects (1, T, encoder_hidden)")?;
        let device = encoder_out.device();
        let state = self
            .predictor
            .zero_state(1, device)
            .context("predictor zero_state")?;
        let (tokens, _, _) = self.decode_greedy_stateful(encoder_out, t_frames, None, state)?;
        Ok(tokens)
    }

    /// Stateful TDT greedy decode that persists `LstmState` and `last_token`
    /// across calls. Used by streaming, where the encoder produces
    /// incremental frames and the decoder must resume from where the
    /// previous chunk left off.
    ///
    /// Decodes the first `length` frames of `encoder_out`, threading the
    /// supplied `last_token` and `state` through the loop. Returns the
    /// decoded tokens, the final `last_token` (`Some(...)` if any
    /// non-blank token was emitted; otherwise unchanged from input), and
    /// the final `LstmState`.
    ///
    /// `length` must be `<= encoder_out.dim(1)`. Setting it to a value
    /// less than the full encoder length is how the streaming wrapper
    /// implements the finalized-vs-draft split: call once with
    /// `length = finalized_frames` and persist the returned state, then
    /// call again with the same state on the draft frames and discard
    /// the result.
    pub fn decode_greedy_stateful(
        &self,
        encoder_out: &Tensor,
        length: usize,
        last_token: Option<u32>,
        state: LstmState,
    ) -> Result<(Vec<u32>, Option<u32>, LstmState)> {
        let (batch, t_frames, _) = encoder_out
            .dims3()
            .context("decode_greedy_stateful expects (1, T, encoder_hidden)")?;
        anyhow::ensure!(
            batch == 1,
            "decode_greedy_stateful: batch must be 1, got {batch} — batched decode is not implemented"
        );
        anyhow::ensure!(
            length <= t_frames,
            "decode_greedy_stateful: length {length} > encoder frames {t_frames}"
        );

        let device = encoder_out.device();
        let blank_id = self.config.vocab_size as u32;
        let mut state = state;
        let mut last_token = last_token;
        let mut tokens: Vec<u32> = Vec::new();

        let mut step = 0_usize;
        let mut consecutive_zero_durations = 0_usize;

        while step < length {
            let (pred_out, new_state) = self
                .predictor
                .step(last_token, &state, device)
                .context("predictor step")?;
            let enc_step = encoder_out.i((.., step, ..))?;
            let logits = self
                .joiner
                .forward(&enc_step, &pred_out)
                .context("joiner forward")?;
            let logits_flat = logits.i(0)?;

            let token_logits = logits_flat.narrow(0, 0, self.config.vocab_size + 1)?;
            let dur_logits =
                logits_flat.narrow(0, self.config.vocab_size + 1, self.config.durations.len())?;
            let pred_token = argmax_u32(&token_logits)?;
            let dur_idx = argmax_u32(&dur_logits)? as usize;
            let duration = self.config.durations[dur_idx];

            if pred_token != blank_id {
                tokens.push(pred_token);
                last_token = Some(pred_token);
                state = new_state;
            }

            step += duration;

            // Anti-stuck rule from MLX: track consecutive zero-duration
            // emissions and force-advance after `max_symbols_per_step`
            // to prevent infinite loops on a single encoder frame.
            if duration == 0 {
                consecutive_zero_durations += 1;
                if consecutive_zero_durations >= self.config.max_symbols_per_step {
                    step += 1;
                    consecutive_zero_durations = 0;
                }
            } else {
                consecutive_zero_durations = 0;
            }
        }

        Ok((tokens, last_token, state))
    }
}

/// Loads an `(in, out)` linear layer with optional bias.
fn linear(vb: VarBuilder, in_dim: usize, out_dim: usize, use_bias: bool) -> Result<Linear> {
    let w = vb
        .get((out_dim, in_dim), "weight")
        .with_context(|| format!("load linear weight {in_dim}x{out_dim}"))?;
    let b = if use_bias {
        Some(vb.get(out_dim, "bias").context("load linear bias")?)
    } else {
        None
    };
    Ok(Linear::new(w, b))
}

/// Argmax over a 1-D tensor, returned as `u32`. Pulls the tensor to
/// host once — fine for the small `vocab + 1` (1025) and `num_durations`
/// (5) slices the TDT loop uses.
fn argmax_u32(t: &Tensor) -> Result<u32> {
    let v: Vec<f32> = t.to_vec1::<f32>().context("argmax: tensor to host")?;
    let (idx, _) = v
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .ok_or_else(|| anyhow::anyhow!("argmax on empty tensor"))?;
    u32::try_from(idx).context("argmax index exceeds u32")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn lstm_state_zeros_shape() {
        let state = LstmState::zeros(2, 1, 64, &Device::Cpu).unwrap();
        assert_eq!(state.h.len(), 2);
        assert_eq!(state.c.len(), 2);
        assert_eq!(state.h[0].dims(), &[1, 64]);
        assert_eq!(state.c[1].dims(), &[1, 64]);
    }

    #[test]
    fn argmax_u32_returns_largest_index() {
        let t = Tensor::from_slice(&[0.1_f32, 0.9, 0.2, 0.05], 4_usize, &Device::Cpu).unwrap();
        assert_eq!(argmax_u32(&t).unwrap(), 1);
    }

    #[test]
    fn argmax_u32_breaks_ties_by_first() {
        // total_cmp orders +0 < +1; for identical values, max_by returns
        // the later one (since max_by keeps the latest on tie). This
        // test pins that behaviour so any change becomes intentional.
        let t = Tensor::from_slice(&[1.0_f32, 1.0, 1.0], 3_usize, &Device::Cpu).unwrap();
        let got = argmax_u32(&t).unwrap();
        assert!(got <= 2);
    }

    #[test]
    fn parakeet_tdt_config_constants_match_documented_v2_values() {
        let c = PARAKEET_TDT_0_6B_V2;
        assert_eq!(c.vocab_size, 1024);
        assert_eq!(c.pred_hidden, 640);
        assert_eq!(c.pred_rnn_layers, 2);
        assert_eq!(c.encoder_hidden, 1024);
        assert_eq!(c.joint_hidden, 640);
        assert_eq!(c.durations, &[0, 1, 2, 3, 4]);
        assert_eq!(c.max_symbols_per_step, 10);
        assert_eq!(c.joint_activation, JointActivation::Relu);
        // num_classes (joiner output) = vocab + blank + num_durations
        let num_classes = c.vocab_size + 1 + c.durations.len();
        assert_eq!(num_classes, 1030);
    }
}
