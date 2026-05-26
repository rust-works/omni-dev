//! Thin wrapper around the `tokenizers` crate for Parakeet's
//! SentencePiece BPE vocabulary.
//!
//! Parakeet 0.6B v2 ships a 1024-token SentencePiece BPE model. The
//! install pipeline converts the `tokenizer.model` SentencePiece blob
//! into HuggingFace `tokenizer.json` format so this loader can use the
//! existing `tokenizers::Tokenizer::from_file` already in the crate
//! graph (the same path the Whisper backend uses). The blank token is
//! NOT part of the BPE vocab — it lives at id `vocab_size` (= 1024) and
//! is added only by the decoder, never produced by the tokenizer.

use std::path::Path;

use anyhow::{anyhow, Result};
use tokenizers::Tokenizer;

/// Wraps the converted HF tokenizer for Parakeet's BPE vocab.
pub struct ParakeetTokenizer {
    inner: Tokenizer,
}

impl ParakeetTokenizer {
    /// Loads a HF `tokenizer.json` file from disk.
    ///
    /// The install pipeline emits this from the upstream
    /// `tokenizer.model` SentencePiece blob; if the file is missing,
    /// the caller should surface the `voice install-model` hint via
    /// [`ModelSpec::ensure_present`](crate::voice::models::ModelSpec::ensure_present)
    /// before reaching this loader.
    pub fn from_file(path: &Path) -> Result<Self> {
        let inner = Tokenizer::from_file(path)
            .map_err(|e| anyhow!("load Parakeet tokenizer at {}: {e}", path.display()))?;
        Ok(Self { inner })
    }

    /// Number of tokens in the BPE vocab (excludes blank). For
    /// Parakeet 0.6B v2 this is 1024.
    #[must_use]
    pub fn vocab_size(&self) -> usize {
        self.inner.get_vocab_size(false)
    }

    /// Decodes a sequence of token ids to text. Skips any id equal to
    /// or greater than the vocab size — that's the blank slot, which
    /// the decoder filters out but a paranoid caller might pass through.
    pub fn decode(&self, ids: &[u32]) -> Result<String> {
        let vocab = self.inner.get_vocab_size(false) as u32;
        let filtered: Vec<u32> = ids.iter().copied().filter(|&t| t < vocab).collect();
        self.inner
            .decode(&filtered, false)
            .map_err(|e| anyhow!("Parakeet tokenizer decode: {e}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn from_file_missing_path_errors() {
        let p = PathBuf::from("/nope/does/not/exist/tokenizer.json");
        let Err(err) = ParakeetTokenizer::from_file(&p) else {
            panic!("expected missing-file error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("load Parakeet tokenizer"), "got: {msg}");
    }
}
