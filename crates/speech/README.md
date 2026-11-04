# filmcraft-speech

Speech-to-text for FilmCraft's Text panel ▸ Transcript (L2). User-facing behaviour, commands and
model handling: [docs/transcripts.md](../../docs/transcripts.md).

- `Transcriber` trait (mono 16 kHz in, word-timed `filmcraft_project::Transcript` out) and
  `FixedTranscriber` for tests and agents.
- `models`: Whisper model catalogue (pinned revisions, SHA-256, sizes, licences) and, with feature
  `download`, the verified downloader (ureq + rustls + RustCrypto). Weights are never bundled.
- `whisper` (feature `whisper`): Whisper inference on candle (CPU, pure Rust).
- `gpu-speech`: pure Rust Orukeet FastConformer/TDT and Whisper inference on Burn/wgpu. Orukeet
  also supports the NdArray CPU backend; Whisper retains its Candle CPU fallback. No Python or
  external inference runtime is shipped. See `load_on_device` and the `local_transcribe` example.
  Width-one channel convolutions use matrix multiplication on CPU and retain GPU convolution kernels. Loading verifies the
  pinned artifacts once per device load; Auto reuses that verification if GPU falls back to CPU.
- `mel`, `vad`, `diarize`: log-mel front end, word-bound tightening, MFCC-clustering speaker labels.

Features are off by default in this library and are never enabled for wasm (`cargo xtask wasm` checks the
crate without them).

## References

Implemented from the published description of the model: A. Radford et al., "Robust Speech
Recognition via Large-Scale Weak Supervision" (OpenAI, 2022), the model cards and configuration
files published with the weights (`config.json`, `generation_config.json`, `tokenizer.json`), and
the candle tensor library's public API. Weights: OpenAI Whisper, MIT licence.

Orukeet is implemented from the published FastConformer/TDT architecture and the Oruk model card,
configuration, preprocessing description and tensor layout at Hugging Face revision
`b59c13a733fce5cf193230d0fa317ee7f145a108` (r3 Transformers export, 2026-09 research release).
The model card is https://huggingface.co/oruk/orukeet; weights are CC BY-SA 4.0 and are downloaded
separately with their recorded provenance. Tensor operations use Burn's permissively licensed API.
