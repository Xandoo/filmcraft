# Import recording streams

Drag a multi-video MP4/MOV or MKV/WebM recording into the Project window, or use ordinary
**File → Import**. The Project window keeps one recording item containing all its native streams.
Drag that item into the Timeline: FilmCraft places every video and audio stream on independent
tracks and creates missing tracks automatically. Dropping the file directly onto the Timeline
also works, including an empty Timeline. No stream-selection dialog is required.

The recording keeps one original media file: import does not extract, copy, transcode, or generate
proxies, even when automatic ingest is enabled. Single-video recordings keep their established
import and audio-track behavior. A recording saved in the Project window retains its stream catalog
when reopened, so later timeline drags still expand all streams.

For an explicit subset instead, choose **File → Import Streams…**, pick one recording, check the streams, and confirm.
Supported streams start selected; unsupported ones show a reason. Cancel leaves the project alone.
The selected streams appear together at the playhead, on new tracks, ready for independent editing.

## Commands

1. Run `media.inspectStreams {"path":"/absolute/path/recording.mp4"}`. The result contains an
   `origin` in FilmCraft ticks and a bounded `streams` list. Each entry reports its native `id`,
   kind, codec, name, timebase, raw earliest PTS, presentation start/end, video/audio properties, channel labels
   when known, and an `unsupported` reason (null when selectable).
2. Pass the selected `id` objects unchanged to
   `file.importStreams {"path":"/absolute/path/recording.mp4","streams":[...],"at":0}`.
   `at` is an integer Tick, not seconds. Optional `sequence` identifies an existing sequence;
   otherwise use the active sequence, or create one using the first selected video's settings.

Each selected stream gets a project media item, new track, and unlinked timeline clip. The clips
retain relative presentation offsets, including subframe offsets. The entire import is one undo
step. Empty, duplicate, missing, unsupported, or overflowing selections fail before committing.
Projects saved by this build use schema v19 and retain the native stream identities. Legacy
projects load with their original default-stream behavior. Older builds refuse v19 projects; v18 selections still load unchanged.

Seeking, source monitors, thumbnails, waveforms, playback, relinking, and background exports all
resolve the selected stream. A missing explicit selection becomes unavailable media, never the
first stream of a replacement file. Existing proxies without a selected-stream mapping are
bypassed for explicit stream items. Export uses FilmCraft's usual composited video and audio mix;
this feature does not add a ten-stream output muxer.

## Timing and limits

All selected views share the recording's presentation clock. MP4 edit offsets and Matroska codec
delay are handled by the existing container sources. Import uses one origin for the complete
recording, independent of the checked subset, and places each clip at `at + start - origin`.
The view's local source zero maps back to that stream's presentation start. Nominal frame rates
describe footage; they do not replace presentation timestamps.

Stream import supports at most 128 container tracks and respects sequence track limits. The
existing codec decoders remain authoritative. Encrypted/compressed Matroska tracks and MP4
tracks with multiple nonempty media edits, dwell/rate-changing edits, and negative video presentation
timestamps are unsupported. Unknown multichannel speaker labels
are reported as unspecified rather than invented. Hosts without random-access readers cannot
use this path for files above 64 MiB. No external decoder executable is used by the product.

## Verification

`stream_import_tests` generates short four-video/six-stereo-audio recordings locally with ffmpeg
as a test fixture generator/oracle. It checks independent seeks, editing, exact persistence,
undo/redo, isolated exports and composite output. The real recording test additionally compares
exports with external decoding of the original streams, selecting the held video frame by its
presentation timestamp and checking non-silent audio for sample shifts. `codecs::streams::tests` adds in-memory PCM
identity checks and mutation/truncation coverage. Generated recordings stay in `target/fixtures`.

The real OBS acceptance test is opt-in and must be run explicitly before claiming compatibility
with a particular OBS recording:

```powershell
$env:FILMCRAFT_OBS_SAMPLE = 'C:\Users\adlma\Videos\2026-11-02_15-44-04_MultiTrack.mp4'
cargo test -p filmcraft-engine --lib recording_streams_actual_obs_acceptance -- --ignored --nocapture
```

The initial recording has four H.264 High 8-bit 4:2:0 videos (three 1920×1080, one 1080×1920)
and six AAC-LC 48 kHz stereo mixes. Two videos start at 70 ms, two at 103 ms; audio starts at zero.
This recording passed the local acceptance run: import, independent edits, undo/redo, fresh-session
save/reopen, isolated exports at 0/16/30 seconds, and a four-view composite. Original-stream oracle
comparisons matched the held video frames and found no sample shift in non-silent audio windows.
The acceptance run also covers ordinary import into one Project item, save/reopen before placement,
automatic track expansion, independent edits, and exports. UI tests simulate the OS drop into the
Project window followed by dragging that item onto both empty and existing Timelines; the real OBS
recording passed those gestures with all four video and six audio streams and their exact offsets.
Hybrid/fragmented OBS MP4, other codec profiles, and unusual timestamp/edit structures need their
own acceptance recordings; format recognition alone is not evidence of successful decoding.
Matroska trailing `DiscardPadding` is not exposed by the current demux API; exact trailing-padding
preservation for such recordings is unverified. The tested audio formats here are AAC-LC and PCM.
