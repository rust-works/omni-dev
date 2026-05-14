# `two_speakers.wav` provenance

Two-speaker fixture used by the speaker-enrolment / speaker-locking work
on issue #805 (the speaker-embedding spike, the `voice transcribe
--speaker` filter test, and the eventual `voice enroll` integration
test).

## Source

Two tracks from the LibriVox community recording of **Aesop's Fables,
Volume 1** (catalog item [`aesop_fables_volume_one_librivox`](https://archive.org/details/aesop_fables_volume_one_librivox)),
each read by a different volunteer:

| Speaker | Source file                                                                                                                  | Reader (LibriVox) | Track                              |
|---------|------------------------------------------------------------------------------------------------------------------------------|-------------------|------------------------------------|
| A       | [`fables_01_01_aesop.mp3`](https://archive.org/download/aesop_fables_volume_one_librivox/fables_01_01_aesop.mp3)             | Joplin            | The Fox and The Grapes             |
| B       | [`fables_01_03_aesop.mp3`](https://archive.org/download/aesop_fables_volume_one_librivox/fables_01_03_aesop.mp3)             | Kristin Luoma     | The Cat and the Mice               |

## License

Public domain. LibriVox's standing policy
([about page](https://librivox.org/pages/about-librivox/)):

> Librivox donates its recordings to the public domain.
>
> All our audio is in the public domain, so you may use it for whatever
> purpose you wish.

Functionally equivalent to CC0 for the project's purposes — no
attribution required, no share-alike, no field-of-use restriction. The
attribution table above is courtesy, not obligation.

## Construction

Each source MP3 was trimmed to a 12 second window starting at the 1 s
mark (to skip the leading silence padding LibriVox volunteers commonly
leave at the start of a track), downmixed to 16 kHz mono signed 16-bit
PCM, joined with a 0.5 s silence gap between speakers, and concatenated
to a single WAV. Total runtime is 24.5 s.

Exact `ffmpeg` recipe:

```sh
ffmpeg -y \
  -ss 1 -t 12 -i fables_01_01_aesop.mp3 \
  -ss 1 -t 12 -i fables_01_03_aesop.mp3 \
  -filter_complex "\
    [0:a]aresample=16000,aformat=channel_layouts=mono:sample_fmts=s16[a0];\
    [1:a]aresample=16000,aformat=channel_layouts=mono:sample_fmts=s16[a1];\
    aevalsrc=0:d=0.5:s=16000[sil];\
    [a0][sil][a1]concat=n=3:v=0:a=1[out]" \
  -map "[out]" -ar 16000 -ac 1 -sample_fmt s16 -c:a pcm_s16le \
  two_speakers.wav
```

Output verification (`ffprobe two_speakers.wav`):

- Duration: 24.5 s
- Codec: `pcm_s16le`
- Sample rate: 16 000 Hz
- Channels: 1 (mono)
- Sample format: signed 16-bit

These match the invariants enforced by
`VoiceAudioInput::from_wav_path` in [src/voice/transcriber.rs](../../../src/voice/transcriber.rs).

## Why not Mozilla Common Voice (strict CC0)?

Common Voice clips are individually short (~5 s average) and packaged in
a multi-gigabyte tarball without per-clip download URLs, which makes
reproducible single-fixture construction awkward. LibriVox PD audio is
licence-equivalent for this use, individually downloadable from
[archive.org](https://archive.org), and longer per clip — easier to
trim into a well-controlled 12 s sample per speaker.
