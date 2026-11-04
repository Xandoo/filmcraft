# Transcripts and text-based editing

FilmCraft's Text panel ▸ **Transcript** tab shows the dialogue of the open sequence as text. Select
words to mark In/Out, then extract or lift them; remove filler words and long pauses in one step;
turn the transcript into captions. Every action is an engine command (`transcript.*`), so the CLI,
the control channel and MCP agents can do the same.

## Model

- A **transcript** belongs to a media item (`Project::transcripts`, saved in the `.fcproj` since
  schema v9). It lists **words** with media-time bounds (`Tick`s), an optional speaker index and a
  confidence, plus the speaker names and the language. Because the times are media time, the
  transcript stays valid however the clip is trimmed, moved, sped up or reused.
- The **sequence transcript** is derived, never stored (`filmcraft_edit::transcript::sequence_words`):
  audio tracks are read top first; a word is heard through the first enabled clip whose range
  covers the word's midpoint (duplicates of the same dialogue on lower tracks read once); disabled,
  reversed and frame-hold clips contribute nothing.
- Speaker names come from the clip transcripts, so renaming "Speaker 1" in every transcript renames
  it across the sequence.

## Commands

| Command | What it does |
|---|---|
| `transcript.generate` | Synchronous transcription for existing callers (`items`, else Project selection, else sequence audio). Uses the selected model, `language`, `diarize`, and `maxSpeakers`. One undo step. |
| `transcript.start` | Background transcription with automatic verified downloads. Returns `{job}`; inspect/cancel with `jobs.list` / `jobs.cancel`. Accepts `items`, `model`, `device` (`auto`, `gpu`, `cpu`), `language`, `diarize`, `maxSpeakers`; `wait:true` waits and applies results before returning. |
| `transcript.set` | Store a transcript you bring (JSON: `language`, `speakers`, `words` with `text`/`start`/`end`/`speaker`); it is sorted and made well formed. |
| `transcript.delete` | Remove transcripts. |
| `transcript.inspect` | The sequence transcript: words (index, text, sequence times, speaker, clip), paragraphs, speakers, the word at the playhead. |
| `transcript.search` | Word-index ranges matching a phrase (case and punctuation ignored; the last word may be a prefix). |
| `transcript.select` | Mark In/Out around words `from..=to` (frame-snapped outward) and move the playhead there. |
| `transcript.extract` / `transcript.lift` | Extract (ripple) or lift the words' frames on the targeted tracks. |
| `transcript.renameSpeaker` | Rename a speaker by name (every transcript) or by index in one `item`. |
| `transcript.removeFillers` | Ripple-delete filler words (`fillers`, default um/uh/erm/…; phrases such as "you know" allowed). |
| `transcript.removePauses` | Ripple-delete pauses longer than `minSeconds`, keeping `keepSeconds` of air on both sides. |
| `transcript.createCaptions` | Lay the words out as captions on a new caption track (`maxChars`, `lines`, `minSeconds`, `maxSeconds`, `gapFrames`). |
| `transcript.createText` | Create editable graphics clips on a new video track above existing video. Choose `grouping: "characters"` with `maxChars`, or `"words"` with `maxWords`. No recognition is run. |
| `transcript.models` / `transcript.downloadModel` | List the speech models (size, licence, installed) / download one. |

## Speech recognition

### Editable text clips

Text ▸ Transcript ▸ **Create editable text clips…** (the text tool button), or
Sequence ▸ Transcript ▸ **Create Text Clips…**, opens grouping settings for an existing transcript.
The Transcribe Sequence dialog also offers **Create editable text clips after transcription**.
Each group becomes an ordinary graphic clip with an editable text layer on a new video track;
edit its text and styling through Graphics or the Type tool. Caption export remains separate.

Choose a maximum character count (spaces and punctuation included; Unicode characters rather
than bytes) or word count (one for individual words). Words are never cut in half, so a single
word longer than the character limit stays intact. Additional controls split at pauses, sentences,
speaker changes, clip boundaries and maximum duration; a single word's duration remains intact.
Set the text size and new track name in the same dialog. The settings persist in UI state.

A clip starts at its first word and ends at its last word, snapped outward to sequence frames;
it does not stretch across subsequent silence. Adjacent clips never overlap. If multiple groups
start within one video frame, generation reports an error suggesting a larger group size.
Creating clips is one undo step. Changing grouping on an existing transcript does not reload a model.

Agents use `transcript.createText`, or pass `textTrack` with the same settings to
`transcript.start`, `transcript.generate` or `sequence.transcribe`:

```json
{"clips":[42],"textTrack":{"grouping":"characters","maxChars":32,"pauseSeconds":0.8,"maxSeconds":7,"breakSentences":true,"fontSize":72,"name":"Dialogue"}}
```

When combined with recognition, only the requested clips (or requested media's sequence uses)
produce graphics. Transcript and graphics commit together in one undo step. The originating
sequence is retained even if another sequence becomes active; if that sequence is edited while
recognition runs, the result is discarded as stale. Failed or cancelled jobs leave existing work intact.

### Performance diagnostics

Completion status reports the actual device, preparation time, recognition time and processed audio
duration. Preparation includes waiting for the inference worker and model verification/loading;
it occurs once per cached model/device in a session. Audio reads happen separately. Installed
weights are checksum-verified once per load, including when Auto falls back from GPU to CPU.
Orukeet's width-one channel convolutions use matrix multiplication on CPU and retain the GPU
convolution kernels. CPU remains considerably more expensive than GPU for this model; the local numbers below
are sample measurements, not hardware-independent speed guarantees.

On the local 5.855-second LibriSpeech sample, the matrix path reduced Orukeet CPU recognition
from 182.72 s to 18.80 s in the development build (9.7×), with identical text and word timestamps.
These single-sample measurements include concurrent build activity and are not a general benchmark.

### Recognition backend

Recognition goes through the `Transcriber` trait (`crates/speech`). The built-in recogniser is
OpenAI's Whisper, run in pure Rust on [candle](https://github.com/huggingface/candle) on the CPU,
with timestamp decoding, language detection and word times from cross-attention alignment (see the
`filmcraft_speech::whisper` module docs). The alignment makes words contiguous (each word ends where
the next starts) and places a start that follows a pause late, so the word bounds are then measured
against the audio (`filmcraft_speech::vad`): a pause of 0.2 s or more inside a word's span ends
the word and starts the next one after the pause, and silence at the edges is trimmed. That is
what `transcript.removePauses` and the extract / lift cuts see. Speakers are labelled by
clustering per-chunk MFCC statistics (`filmcraft_speech::diarize`); no model is involved.

Speech features remain optional at the library level and are excluded from the web build. The
desktop app enables `local-speech` by default (Whisper, Orukeet, GPU inference and downloads):

- `whisper` (on `filmcraft-speech`, `filmcraft-engine`, and the `filmcraft` / `filmcraft-cli`
  apps, where it also enables downloads): candle inference.
- `download` (`speech-download` on the engine): HTTPS downloads with rustls + RustCrypto and the
  operating system's certificate verifier.
- `gpu-speech`: Orukeet FastConformer/TDT and Whisper tensor execution through Burn/wgpu, with
  a pure Rust CPU fallback. Choose Auto, GPU or CPU in the Text panel or Transcribe Sequence dialog.
  GPU is required when explicitly selected; Auto reports a CPU fallback in the job status.

Without `whisper`, and with no recogniser installed, `transcript.generate` and Transcribe Sequence
are disabled, with "speech-to-text is not available in this build" as the reason (`describe`,
`command_list {"enabled_only": true}` and the menus show it); with Automatically transcribe clips
on with the all-imported scope, `file.import` reports the same reason as a `transcription: …`
entry in its `errors`.

The default sequence-only scope queues recognition when enabled audio is placed in a sequence (including
Source Insert/Overwrite, timeline placement, paste and sequence expansion). It transcribes the
underlying media once, preserving any existing transcript. Completion is its own undo step, so
it cannot merge with unrelated edits made while recognition runs. Undo/Redo and opening or recovering a project do not run recognition.
If recognition fails, the clips stay placed and a warning appears in the Events panel and a toast.

Without `speech-download`, `transcript.downloadModel` is disabled the same way. Transcripts can
still be imported with `transcript.set` and edited with every other command. Hosts and tests can
install any recogniser in `Session::transcriber`, which enables transcription in any build.

### Models

Weights are **never** bundled or committed. They are downloaded on request into
`<data dir>/models/<id>/` (see `filmcraft_engine::autosave::default_data_dir`), each file pinned to a
revision of the publishers' Hugging Face repositories and checked against its SHA-256:

| Id | Languages | Licence |
|---|---|---|
| `orukeet` (new default) | 25 European languages; choose Whisper for other languages | CC BY-SA 4.0; Oruk AI and contributors |
| `whisper-tiny` | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-base` | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-small` | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |

Existing saved model preferences are retained. The picker shows artifact sizes, qualitative guidance,
model-card links, installation status and a conservative memory estimate (four times artifact bytes;
actual memory depends on device and audio). It does not claim hardware-independent accuracy scores.
Orukeet is pinned to `oruk/orukeet` revision `b59c13a733fce5cf193230d0fa317ee7f145a108`.

Jobs read audio in bounded 60-second chunks, check cancellation between chunks and decoding steps,
and apply completed transcripts on the session thread. Results are discarded if the project,
source media or existing transcript changed while the job ran. Only one model is resident in the
session cache, and inference jobs serialize device use. Failed or cancelled jobs preserve existing work.

### Testing

Unit and engine tests use a fake `Transcriber` (`FixedTranscriber`), so CI needs no model. The
end-to-end test `crates/speech/tests/whisper_model.rs` (feature `whisper`) runs only when weights are
in `target/models/<id>/` (or `$FILMCRAFT_MODELS_DIR`) and speech samples (mono 16 kHz f32 with a
reference `.txt`) are in `target/fixtures/speech/`; it reports the word error rate and otherwise
prints SKIPPED. Measured on 2026-10-01: `whisper-tiny`, English, 12.2 % WER over 797 words of
the local speech samples (LibriSpeech read speech and dialogue clips) (about 9.5 minutes for the run in a release build on an
Apple-silicon laptop CPU).

GPU qualification on 2026-11-02: RTX 5070 Ti (16 GB), Orukeet on three CC BY 4.0 LibriSpeech
recordings (5.855 s, 4.815 s and 12.485 s) produced 0% normalized word error on this small sample.
GPU recognition took 4.94 s, 3.15 s and 2.68 s respectively, excluding model loading; total device
memory observed during the latter two runs peaked at 4,538 MiB, including other desktop GPU users.
The first recording matched CPU output, including every word timestamp; CPU recognition took
190.76 s. The GPU real-time factors were 0.84, 0.65 and 0.22 (recognition seconds / audio seconds).
These are smoke checks,
not a general accuracy or speed benchmark. Whisper Tiny's CPU/GPU text also matched on the first
recording (one abbreviation difference from the reference, 5.88% normalized WER), with identical
word timestamps. Tiny's cold GPU run took 10.80 s (RTF 1.84), versus CPU's 1.50 s (RTF 0.26);
the smaller model is a useful CPU choice for short recordings.

Run the optional real-model check with `cargo run -p filmcraft-speech --features gpu-speech,whisper
--example local_transcribe -- target/models/orukeet sample.f32 --reference=sample.txt`; add `--cpu`
to compare backends. Weights and speech fixtures stay in ignored build directories.

## Limits

- `transcript.generate` deliberately remains synchronous for compatibility; the UI and automatic
  analysis use `transcript.start` instead. Speaker clustering is approximate, including across chunks.
- Track items that refer to a subclip are looked up by the subclip's id, so a transcript made for
  the parent media is not shown through subclip clips yet.
- Word bounds are only moved across pauses. A start that the alignment places late inside
  continuous speech (no pause of 0.2 s between the two words) stays where it is, so the word
  before it can hold the beginning of the next one: the audio level gives no boundary to move it
  to. Words that the alignment places a whole sentence away are not moved back either.

### Choosing audio for transcription

The Text panel's Transcribe action requires selected enabled timeline audio clips. It reads only
complete audio samples inside each clip's source In/Out bounds, on that clip's audio stream.
Forward speed changes consume the corresponding source range; words retain source timestamps
and are mapped back through the clip's speed and timeline position. Reverse and held clips report
an unsupported-input error. No video rendering or sequence export is involved.

Transcribe Sequence defaults to the selected audio clip's track, then a targeted audio track,
then A1. With audio clips selected it processes only those clips on the chosen track. Without
an audio selection it processes the trimmed clips on that track (All audio tracks includes the
other tracks). Unselected portions of a source file are never read by these UI actions.

Overlapping selected uses of one source are recognized once. Disjoint selected ranges are read
separately, and existing transcript words outside those ranges are retained. If the selected clip
is edited while recognition runs, the stale result is discarded. Applying results is one undo step.

`transcript.start` accepts `clips: [clipId]` for explicit timeline bounds; without explicit targets
it uses timeline selection. Explicit `item` / `items` requests and automatic media analysis still
operate on whole media items. The legacy `transcript.generate` API remains synchronous and also honors timeline selection or explicit `clips`.
The job status distinguishes model download, loading, audio reading, and recognition.
