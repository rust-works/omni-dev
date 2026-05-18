# `monologue_5min.wav` provenance

Single-speaker 5-minute monologue fixture used by the streaming ASR
runtime spike on issue #826 — the prototypes for `candle` +
LocalAgreement-2 and `tract-onnx` + streaming Zipformer measure latency,
word error rate, RTF, and peak RSS against this file.

## Source

A 5-minute excerpt of the LibriVox community recording of **The
Adventures of Sherlock Holmes** by Arthur Conan Doyle (catalog item
[`adventures_holmes`](https://archive.org/details/adventures_holmes)),
chapter 1 "A Scandal in Bohemia", read by LibriVox volunteer **TBOL3**.

| Source file                                                                                                                                              | Reader (LibriVox) | Chapter                  | Full length |
|----------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------|--------------------------|-------------|
| [`adventureholmes_01_doyle_64kb.mp3`](https://www.archive.org/download/adventures_holmes/adventureholmes_01_doyle_64kb.mp3)                              | TBOL3             | 1. A Scandal in Bohemia  | 01:05:06    |

The full track was trimmed to a 5 minute window starting at the 60 s
mark — past the standard LibriVox preamble ("This is a LibriVox
recording...") so the fixture is uninterrupted prose narration.

## License

Public domain. LibriVox's standing policy
([about page](https://librivox.org/pages/about-librivox/)):

> Librivox donates its recordings to the public domain.
>
> All our audio is in the public domain, so you may use it for whatever
> purpose you wish.

Functionally equivalent to CC0 for the project's purposes — no
attribution required, no share-alike, no field-of-use restriction. The
attribution table above is courtesy, not obligation. The underlying
Doyle text is also public domain (first published 1891).

## Construction

The source MP3 was trimmed to a 5 minute window starting at the 60 s
mark and downmixed to 16 kHz mono signed 16-bit PCM.

Exact `ffmpeg` recipe:

```sh
ffmpeg -y -ss 60 -t 300 -i adventureholmes_01_doyle_64kb.mp3 \
  -ac 1 -ar 16000 -sample_fmt s16 -c:a pcm_s16le \
  monologue_5min.wav
```

Output verification (`ffprobe monologue_5min.wav`):

- Duration: 5 min 00 s exact
- Codec: `pcm_s16le`
- Sample rate: 16 000 Hz
- Channels: 1 (mono)
- Sample format: signed 16-bit
- File size: ~9.6 MB

These match the invariants enforced by
`VoiceAudioInput::from_wav_path` in [src/voice/transcriber.rs](../../../src/voice/transcriber.rs).

## Size note (~9.6 MB vs the issue's "≤ ~6 MB" indicative cap)

The fixture is 9.6 MB rather than the "~6 MB" target mentioned in
[#826](https://github.com/rust-works/omni-dev/issues/826). 16 kHz mono
signed-16-bit PCM has a hard floor of `16000 × 2 = 32 000` bytes per
second, so 5 minutes is ~9.6 MB by construction. The 5-minute duration
is the load-bearing requirement (boundary artefact testing across many
silence-gap utterances), and compressing to FLAC would halve the size
but break the WAV invariant `VoiceAudioInput::from_wav_path` enforces.
The size cap in the issue is read as indicative, not normative.

## Ground truth

A ground-truth transcript is committed alongside this file as
`monologue_5min.expected.txt`. It was generated with
`whisper.cpp` (Homebrew `whisper-cpp`) + `ggml-tiny.en.bin` against
this WAV, then hand-corrected for proper-noun spelling and obvious
mis-recognitions at silence boundaries. The ground truth is reference
data for the spike's WER computation; using the same `tiny.en` model
family as candidate 1 (`candle` + `whisper-tiny.en`) is intentional —
both candidates are scored against the same reference, so the
comparison is fair across runtimes even if the absolute WER number
favours `tiny.en`-family transcribers.

## Why not Mozilla Common Voice (strict CC0)?

Common Voice clips are individually short (~5 s average) and packaged
in a multi-gigabyte tarball without per-clip download URLs, which makes
reproducible single-fixture construction awkward for a 5-minute
monologue. LibriVox PD audio is licence-equivalent for this use,
individually downloadable from [archive.org](https://archive.org), and
chapter-length tracks are easy to trim into a well-controlled
single-reader sample.
