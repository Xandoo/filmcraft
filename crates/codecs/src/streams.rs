//! Independent views over one immutable recording index and random-access reader.
use crate::{MkvSource, Mp4Source};
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::streams::{MAX_CONTAINER_STREAMS, StreamCatalog, StreamId, StreamInfo, StreamKind};
use filmcraft_media::{FrameRequest, MediaError, MediaInfo, MediaSource, SharedReader, SharedSource};
use filmcraft_time::Tick;
use std::sync::Arc;

enum Index {
    Mp4(Arc<filmcraft_isobmff::Mp4File>),
    Mkv(Arc<filmcraft_matroska::MkvFile>),
}

/// One index shared by every view made during an import.
pub struct Recording {
    reader: SharedReader,
    index: Index,
    pub catalog: StreamCatalog,
}

impl Recording {
    pub fn open(reader: SharedReader) -> Result<Self, MediaError> {
        let head = filmcraft_media::reader::read_range(&*reader, 0, reader.len().min(16) as usize).map_err(|e| MediaError::Io(e.to_string()))?;
        let bytes = crate::Src(reader.clone());
        let index = if crate::mp4::sniff(&head) {
            Index::Mp4(Arc::new(filmcraft_isobmff::open(&bytes).map_err(|e| MediaError::Decode(e.to_string()))?))
        } else if crate::mkv::sniff(&head) {
            Index::Mkv(Arc::new(filmcraft_matroska::open(&bytes).map_err(|e| MediaError::Decode(e.to_string()))?))
        } else {
            return Err(MediaError::Unsupported("stream import requires MP4/MOV or MKV/WebM".into()));
        };
        let mut recording = Self { reader, index, catalog: StreamCatalog { origin: Tick::ZERO, streams: Vec::new() } };
        let count = match &recording.index {
            Index::Mp4(f) => f.tracks.len(),
            Index::Mkv(f) => f.tracks.len(),
        };
        if count > MAX_CONTAINER_STREAMS {
            return Err(MediaError::Unsupported("recording exceeds 128 container tracks".into()));
        }
        for i in 0..count {
            let descriptor = match &recording.index {
                Index::Mp4(f) => {
                    let Some(t) = f.tracks.get(i) else { continue };
                    if !matches!(t.kind, filmcraft_isobmff::TrackKind::Video | filmcraft_isobmff::TrackKind::Audio) {
                        continue;
                    }
                    let first = t.samples.iter().map(|s| s.pts).min().unwrap_or(0);
                    let start = Tick::from_rational(first.saturating_add(t.edit_offset).max(0), 1, i64::from(t.timescale.max(1)));
                    let end = crate::mp4::presentation_duration(f, t);
                    let kind = if t.kind == filmcraft_isobmff::TrackKind::Video { StreamKind::Video } else { StreamKind::Audio };
                    let codec = t.entries.first().map(|e| crate::mp4::codec_label(&e.codec)).unwrap_or_else(|| "unknown".into());
                    (StreamId::Mp4 { track: t.id }, kind, codec, t.handler_name.clone(), (1, u64::from(t.timescale)), first, start, end)
                }
                Index::Mkv(f) => {
                    let Some(t) = f.tracks.get(i) else { continue };
                    if !matches!(t.kind, filmcraft_matroska::TrackKind::Video | filmcraft_matroska::TrackKind::Audio) {
                        continue;
                    }
                    if t.timebase.0 == 0 || t.timebase.1 == 0 || t.timebase.0 > i64::MAX as u64 || t.timebase.1 > i64::MAX as u64 {
                        return Err(MediaError::Decode("invalid Matroska track timebase".into()));
                    }
                    let first = t.samples.iter().map(|s| s.pts).min().unwrap_or(0);
                    // Demuxed timestamps already include CodecDelay.
                    let at = |pts| Tick::from_rational(pts, t.timebase.0 as i64, t.timebase.1 as i64);
                    let end = t.samples.iter().filter_map(|s| i64::try_from(s.duration).ok().and_then(|d| s.pts.checked_add(d))).max().unwrap_or(first);
                    (
                        StreamId::Matroska { uid: t.uid.to_string(), number: t.number.to_string() },
                        if t.kind == filmcraft_matroska::TrackKind::Video { StreamKind::Video } else { StreamKind::Audio },
                        crate::mkv::codec_label(&t.codec),
                        t.name.clone(),
                        t.timebase,
                        first,
                        at(first).max(Tick::ZERO),
                        at(end),
                    )
                }
            };
            let (id, kind, codec, name, timebase, first_pts, start, mut end) = descriptor;
            if recording.catalog.streams.iter().any(|s| s.id == id) {
                return Err(MediaError::Decode("duplicate container stream identity".into()));
            }
            let opened = recording.source_at(i);
            let (video, audio, unsupported) = match opened {
                Ok(src) => {
                    if matches!(&recording.index, Index::Mkv(_)) {
                        end = src.info().duration;
                    }
                    (src.info().video.clone(), src.info().audio().cloned(), None)
                }
                Err(e) => (None, None, Some(e.to_string())),
            };
            let unsupported = unsupported.or_else(|| (end <= start || timebase.1 == 0).then(|| "empty stream or invalid timing".into()));
            let channel_layout = match audio.as_ref().map(|a| a.channels) {
                Some(1) => vec!["M".into()],
                Some(2) => vec!["L".into(), "R".into()],
                _ => Vec::new(),
            };
            recording.catalog.streams.push(StreamInfo { id, kind, codec, name, timebase, first_pts, start, end, video, audio, channel_layout, unsupported });
        }
        recording.catalog.origin = recording.catalog.streams.iter().map(|s| s.start).min().unwrap_or(Tick::ZERO);
        Ok(recording)
    }

    fn source_at(&self, i: usize) -> Result<SharedSource, MediaError> {
        match &self.index {
            Index::Mp4(f) => {
                let t = f.tracks.get(i).ok_or(MediaError::NoStream("selected"))?;
                if t.timescale == 0 {
                    return Err(MediaError::Decode("zero track timescale".into()));
                }
                if t.duration > i64::MAX as u64 {
                    return Err(MediaError::Decode("track duration exceeds supported range".into()));
                }
                // The current reader implements leading-empty + one media edit only.
                if t.edits.iter().any(|e| e.media_time < -1 || (e.media_time >= 0 && e.media_rate != 65536)) {
                    return Err(MediaError::Unsupported("MP4 dwell, rate-changing, or invalid edits".into()));
                }
                if t.edits.iter().filter(|e| e.media_time >= 0).count() > 1 {
                    return Err(MediaError::Unsupported("multiple media edits in one MP4 track".into()));
                }
                if t.kind == filmcraft_isobmff::TrackKind::Video {
                    if t.samples.iter().any(|s| s.pts.saturating_add(t.edit_offset) < 0) {
                        return Err(MediaError::Unsupported("negative video presentation timestamps".into()));
                    }
                    let entry = t.entries.first().ok_or(MediaError::NoStream("video description"))?;
                    crate::make_video_decoder(entry)?;
                }
                let src = Mp4Source::from_index("stream", self.reader.clone(), f.clone(), Some(i))?;
                if t.kind == filmcraft_isobmff::TrackKind::Audio {
                    let entry = t.entries.first().ok_or(MediaError::NoStream("audio description"))?;
                    if !matches!(entry.codec, filmcraft_isobmff::CodecConfig::Pcm(_)) {
                        let rate = src.info().audio().ok_or(MediaError::NoStream("audio"))?.sample_rate;
                        crate::audio::PacketDecoder::for_isobmff(&entry.codec, rate)?;
                    }
                }
                Ok(Arc::new(src))
            }
            Index::Mkv(f) => {
                let t = f.tracks.get(i).ok_or(MediaError::NoStream("selected"))?;
                if t.kind == filmcraft_matroska::TrackKind::Video && t.samples.iter().any(|s| s.pts < 0) {
                    return Err(MediaError::Unsupported("negative video presentation timestamps".into()));
                }
                if !t.frames_readable() {
                    return Err(MediaError::Unsupported("encrypted or compressed Matroska track".into()));
                }
                let src = MkvSource::from_index("stream", self.reader.clone(), f.clone(), Some(i))?;
                src.validate_selected()?;
                Ok(Arc::new(src))
            }
        }
    }

    pub fn select(&self, id: &StreamId) -> Result<SharedSource, MediaError> {
        let stream = self.catalog.streams.iter().find(|s| &s.id == id).ok_or_else(|| MediaError::Unsupported(format!("selected stream {id:?} is missing")))?;
        if let Some(reason) = &stream.unsupported {
            return Err(MediaError::Unsupported(reason.clone()));
        }
        let i = match (&self.index, id) {
            (Index::Mp4(f), StreamId::Mp4 { track }) => f.tracks.iter().position(|t| t.id == *track),
            (Index::Mkv(f), StreamId::Matroska { uid, number }) => f.tracks.iter().position(|t| t.uid.to_string() == *uid && t.number.to_string() == *number),
            _ => None,
        }
        .ok_or(MediaError::NoStream("selected"))?;
        let inner = self.source_at(i)?;
        let mut info = inner.info().clone();
        info.name = stream.name.clone();
        info.duration = Tick(stream.end.0.checked_sub(stream.start.0).ok_or_else(|| MediaError::Decode("stream duration overflow".into()))?);
        Ok(Arc::new(StreamView { inner, info, start: stream.start }))
    }

    /// Project/source-monitor view: the first selected picture and every audio stream on
    /// the recording clock. Timeline placement uses the individual views instead.
    pub fn combined(&self, catalog: &StreamCatalog) -> Result<SharedSource, MediaError> {
        if catalog.streams.is_empty() || catalog.streams.len() > MAX_CONTAINER_STREAMS {
            return Err(MediaError::Unsupported("invalid recording stream count".into()));
        }
        let mut video = None;
        let mut audio = Vec::new();
        let mut info = None;
        let mut seen = std::collections::HashSet::new();
        for stream in &catalog.streams {
            if !seen.insert(&stream.id) {
                return Err(MediaError::Decode("duplicate recording stream identity".into()));
            }
            let src = self.select(&stream.id)?;
            let actual = self.catalog.streams.iter().find(|d| d.id == stream.id).ok_or(MediaError::NoStream("selected"))?;
            let metadata = info.get_or_insert_with(|| {
                let mut info = src.info().clone();
                info.video = None;
                info.audio_streams.clear();
                info.duration = Tick::ZERO;
                info
            });
            let end = actual.end.0.checked_sub(self.catalog.origin.0).ok_or_else(|| MediaError::Decode("recording duration overflow".into()))?;
            metadata.duration = metadata.duration.max(Tick(end));
            if metadata.video.is_none() && src.info().has_video() {
                metadata.video = src.info().video.clone();
                video = Some((src.clone(), actual.start));
            }
            if let Some(a) = src.info().audio() {
                if audio.len() >= filmcraft_media::MAX_AUDIO_STREAMS {
                    return Err(MediaError::Unsupported("recording exceeds supported audio stream count".into()));
                }
                metadata.audio_streams.push(a.clone());
                audio.push((src, actual.start));
            }
        }
        let mut info = info.ok_or(MediaError::NoStream("recording"))?;
        info.kind = if info.has_video() { filmcraft_media::MediaKind::Movie } else { filmcraft_media::MediaKind::AudioOnly };
        Ok(Arc::new(RecordingView { info, video, audio, origin: self.catalog.origin }))
    }
}

struct RecordingView {
    info: MediaInfo,
    video: Option<(SharedSource, Tick)>,
    audio: Vec<(SharedSource, Tick)>,
    origin: Tick,
}
impl MediaSource for RecordingView {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>, MediaError> {
        let (src, start) = self.video.as_ref().ok_or(MediaError::NoStream("video"))?;
        let time =
            req.time.0.checked_add(self.origin.0).and_then(|t| t.checked_sub(start.0)).ok_or_else(|| MediaError::Decode("recording time overflow".into()))?;
        src.video_frame(FrameRequest { time: Tick(time), ..req })
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> Result<AudioBuffer, MediaError> {
        self.audio_stream(0, start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> Result<AudioBuffer, MediaError> {
        if rate == 0 {
            return Err(MediaError::Decode("zero audio output rate".into()));
        }
        let (src, first) = self.audio.get(stream).ok_or(MediaError::NoStream("audio"))?;
        let offset = self.origin.0.checked_sub(first.0).ok_or_else(|| MediaError::Decode("recording sample offset overflow".into()))?;
        let sample = start
            .checked_add(Tick(offset).to_rational_round(1, i64::from(rate)))
            .ok_or_else(|| MediaError::Decode("recording sample offset overflow".into()))?;
        src.audio(sample, frames, rate)
    }
}

struct StreamView {
    inner: SharedSource,
    info: MediaInfo,
    start: Tick,
}
impl MediaSource for StreamView {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>, MediaError> {
        let time = Tick(req.time.0.checked_add(self.start.0).ok_or_else(|| MediaError::Decode("stream time overflow".into()))?);
        self.inner.video_frame(FrameRequest { time, ..req })
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> Result<AudioBuffer, MediaError> {
        if rate == 0 {
            return Err(MediaError::Decode("zero audio output rate".into()));
        }
        let offset = self.start.to_rational_round(1, i64::from(rate));
        self.inner.audio(start.checked_add(offset).ok_or_else(|| MediaError::Decode("stream sample offset overflow".into()))?, frames, rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_isobmff::{Brand, Mp4Writer, PcmConfig, SampleEntry, TrackConfig, WriteSample, WriterOptions};

    fn pcm() -> Vec<u8> {
        let mut writer = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).unwrap();
        for value in [8192i16, -16384] {
            let entry = SampleEntry::pcm(PcmConfig { bits: 16, float: false, big_endian: false, signed: true, channels: 1, sample_rate: 48000.0 });
            let track = writer.add_track(TrackConfig::new(entry, 48000)).unwrap();
            let data: Vec<_> = (0..4800).flat_map(|_| value.to_le_bytes()).collect();
            writer.write_sample(track, WriteSample { data: &data, duration: 4800, composition_offset: 0, is_sync: true }).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn open(bytes: Vec<u8>) -> Result<Recording, MediaError> {
        Recording::open(Arc::new(filmcraft_media::reader::MemReader(bytes.into())))
    }

    #[test]
    fn explicit_audio_identity_and_independent_views() {
        let r = open(pcm()).unwrap();
        assert_eq!(r.catalog.streams.len(), 2);
        assert!(r.catalog.streams.iter().all(|s| s.kind == StreamKind::Audio && !s.codec.is_empty()));
        let a = r.select(&r.catalog.streams[0].id).unwrap();
        let b = r.select(&r.catalog.streams[1].id).unwrap();
        assert!((a.audio(0, 128, 48000).unwrap().channels[0][64] - 0.25).abs() < 0.001);
        assert!((b.audio(0, 128, 48000).unwrap().channels[0][64] + 0.5).abs() < 0.001);
        assert!(r.select(&StreamId::Mp4 { track: u32::MAX }).is_err());
        assert!(a.audio(0, 1, 0).is_err());
        assert!(a.audio(i64::MAX, usize::MAX, 48000).is_err());
    }

    #[test]
    fn hostile_video_bitrate_does_not_overflow() {
        let mut r = open(pcm()).unwrap();
        let Index::Mp4(file) = &mut r.index else { unreachable!() };
        let track = &mut Arc::make_mut(file).tracks[0];
        track.kind = filmcraft_isobmff::TrackKind::Video;
        track.entries = vec![SampleEntry::jpeg(16, 16)];
        track.samples[0].size = u32::MAX;
        track.timescale = u32::MAX;
        track.duration = 1;
        let source = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.source_at(0))).unwrap().unwrap();
        assert!(source.info().video.as_ref().unwrap().bitrate.is_none());
    }

    #[test]
    fn hostile_video_duration_outlier_does_not_overflow() {
        let mut r = open(pcm()).unwrap();
        let Index::Mp4(file) = &mut r.index else { unreachable!() };
        let track = &mut Arc::make_mut(file).tracks[0];
        track.kind = filmcraft_isobmff::TrackKind::Video;
        track.entries = vec![SampleEntry::jpeg(16, 16)];
        track.samples = vec![track.samples[0]; 3];
        track.samples[0].duration = 3000;
        track.samples[1].duration = 3000;
        track.samples[2].duration = u32::MAX;
        track.timescale = 90000;
        let source = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.source_at(0))).unwrap().unwrap();
        assert_eq!(source.info().frame_rate(), filmcraft_time::FrameRate::FPS_30);
    }

    #[test]
    fn hostile_recording_indexes_do_not_panic() {
        let good = pcm();
        for len in [0, 1, 4, 8, 12, 16, 32, 64, good.len() / 2, good.len() - 1] {
            assert!(
                std::panic::catch_unwind(|| {
                    let _ = open(good[..len].to_vec());
                })
                .is_ok()
            );
        }
        for at in (0..good.len()).step_by((good.len() / 40).max(1)) {
            let mut bytes = good.clone();
            bytes[at] ^= 0xff;
            assert!(
                std::panic::catch_unwind(|| {
                    let _ = open(bytes);
                })
                .is_ok()
            );
        }
    }
}
