# `monologue_5min.words.jsonl` — provenance

Per-word timing **ground truth** for `monologue_5min.wav`, used by the #969 spike
to measure **partial-latency P95** (time from a word being spoken to its first
`Partial`). The existing `monologue_5min.expected.txt` has no timestamps, so this
artifact supplies them.

## How it was generated

```
whisper-cli -m ggml-tiny.en.bin -f monologue_5min.wav -ml 1 -sow -oj -of mono_words
```

- Tool: **whisper.cpp** (`whisper-cli`), model `ggml-tiny.en.bin`
  (`ggerganov/whisper.cpp` on Hugging Face).
- `-ml 1 -sow` splits output into one word per segment; `-oj` emits each word's
  `from`/`to` offset in milliseconds. Offsets were divided by 1000 → `start_s` /
  `end_s` and written one JSON object per line: `{"word", "start_s", "end_s"}`.
- 623 words. Empty/whitespace segments dropped.

## Status: latency ground truth, not a transcription reference

This is **input data for latency alignment**, not an accuracy reference — WER is
still scored against `monologue_5min.expected.txt`. The word times come from
Whisper's own forced decode (a standard, defensible alignment proxy), not a
hand-labelled aligner, so treat the absolute times as ±~one word. The partial-
latency metric aligns the streaming hypothesis stream to these words with
`difflib` and excludes unmatched words (reported as a coverage caveat).
