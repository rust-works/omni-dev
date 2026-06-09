//! Tekken tokenizer (**decode-only**) — a port of `mlx-audio`'s `tokenizer.py`.
//!
//! The decoder emits token ids; transcription only needs id → text, so this is
//! decode-only (no BPE merging / encoding). Token layout in `tekken.json`:
//!
//! - ids `0..n_special` (default 1000) and any id in `special_tokens[].rank` are
//!   control tokens (BOS=1, EOS=2, STREAMING_PAD=32) — skipped on decode.
//! - ids `≥ n_special` index the regular vocab at `id - n_special`; each entry's
//!   `token_bytes` is base64-encoded UTF-8 bytes.
//!
//! Decoding concatenates the raw bytes of all non-special ids, then interprets
//! the buffer as UTF-8 (lossily — a multi-byte char may span tokens).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::Deserialize;

#[derive(Deserialize)]
struct TekkenFile {
    vocab: Vec<VocabEntry>,
    #[serde(default)]
    config: TekkenConfig,
    #[serde(default)]
    special_tokens: Vec<SpecialToken>,
}

#[derive(Deserialize)]
struct VocabEntry {
    token_bytes: String,
}

#[derive(Deserialize, Default)]
struct TekkenConfig {
    #[serde(default)]
    default_num_special_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct SpecialToken {
    #[serde(default)]
    rank: Option<usize>,
}

/// A decode-only Tekken tokenizer: the per-id UTF-8 byte sequences plus the
/// special-id set needed to skip control tokens.
pub struct TekkenTokenizer {
    /// Decoded bytes for each regular vocab entry (index = `id - n_special`).
    vocab_bytes: Vec<Vec<u8>>,
    n_special: usize,
    special_ids: HashSet<usize>,
}

impl TekkenTokenizer {
    /// Loads `tekken.json` from a model directory.
    pub fn from_model_dir(dir: &Path) -> Result<Self> {
        let path = dir.join("tekken.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read tekken.json at {}", path.display()))?;
        Self::from_json(&raw)
    }

    /// Parses tokenizer state from `tekken.json` contents.
    pub fn from_json(raw: &str) -> Result<Self> {
        let file: TekkenFile = serde_json::from_str(raw).context("parse tekken.json")?;
        let n_special = file.config.default_num_special_tokens.unwrap_or(1000);
        let special_ids = file
            .special_tokens
            .iter()
            .filter_map(|s| s.rank)
            .collect::<HashSet<_>>();
        let engine = base64::engine::general_purpose::STANDARD;
        let vocab_bytes = file
            .vocab
            .iter()
            .map(|e| {
                engine
                    .decode(e.token_bytes.as_bytes())
                    .map_err(|err| anyhow!("base64 decode vocab entry: {err}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            vocab_bytes,
            n_special,
            special_ids,
        })
    }

    /// The raw bytes a single id contributes (empty for special / out-of-range).
    fn token_bytes(&self, token_id: i64) -> &[u8] {
        if token_id < 0
            || (token_id as usize) < self.n_special
            || self.special_ids.contains(&(token_id as usize))
        {
            return &[];
        }
        let vocab_id = token_id as usize - self.n_special;
        self.vocab_bytes.get(vocab_id).map_or(&[], Vec::as_slice)
    }

    /// Decodes a sequence of ids to text, skipping special tokens and
    /// interpreting the concatenated bytes as UTF-8 (lossily).
    pub fn decode(&self, token_ids: &[i64]) -> String {
        let mut out: Vec<u8> = Vec::new();
        for &id in token_ids {
            out.extend_from_slice(self.token_bytes(id));
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_skips_special_and_concatenates_bytes() {
        // n_special = 3; ids 0..3 special (1 also explicitly special); vocab
        // entries map ids 3..=4 to "hi" and " there" (base64).
        let json = r#"{
            "config": {"default_num_special_tokens": 3},
            "special_tokens": [{"rank": 1}, {"rank": 2}],
            "vocab": [
                {"token_bytes": "aGk="},
                {"token_bytes": "IHRoZXJl"}
            ]
        }"#;
        let tok = TekkenTokenizer::from_json(json).unwrap();
        // [BOS=1, "hi"=3, " there"=4, EOS=2] -> "hi there"
        assert_eq!(tok.decode(&[1, 3, 4, 2]), "hi there");
        // A bare special id decodes to empty.
        assert_eq!(tok.decode(&[1, 2]), "");
    }

    #[test]
    fn out_of_range_ids_are_empty() {
        let json =
            r#"{"config":{"default_num_special_tokens":3},"vocab":[{"token_bytes":"aGk="}]}"#;
        let tok = TekkenTokenizer::from_json(json).unwrap();
        assert_eq!(tok.decode(&[3]), "hi");
        assert_eq!(tok.decode(&[99]), ""); // beyond vocab
    }
}
