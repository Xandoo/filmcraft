//! Opt-in, atomic import of independent streams from a recording.
use crate::{EngineError, Result, Session};
use filmcraft_media::streams::{MAX_CONTAINER_STREAMS, StreamId};
use filmcraft_project::{ItemId, ItemKind, MediaClip, MediaRef, SequenceSettings, Track, TrackId, TrackKind};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};
use std::collections::HashSet;

pub fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let path = p.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| error("need path"))?;
    let recording = s.media.recording(path, &*s.services)?;
    serde_json::to_value(&recording.catalog).map_err(|e| error(e.to_string()))
}

fn error(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

/// Ordinary import keeps a multi-video recording as one Project-panel item. Single-video
/// media retains its established opener, audio mappings, and timestamp interpretation.
pub(crate) fn prepare_import(
    s: &Session,
    path: &str,
    src: filmcraft_media::SharedSource,
    bytes: Option<std::sync::Arc<[u8]>>,
) -> Result<(filmcraft_media::SharedSource, Option<filmcraft_media::streams::StreamCatalog>)> {
    if !matches!(src.info().container.as_str(), "MPEG-4" | "QuickTime" | "Matroska" | "WebM") {
        return Ok((src, None));
    }
    let recording = match bytes {
        Some(bytes) => std::sync::Arc::new(filmcraft_codecs::streams::Recording::open(std::sync::Arc::new(filmcraft_media::reader::MemReader(bytes)))?),
        None => s.media.recording(path, &*s.services)?,
    };
    if recording.catalog.streams.iter().filter(|d| d.kind == filmcraft_media::streams::StreamKind::Video).count() < 2 {
        return Ok((src, None));
    }
    let combined = recording.combined(&recording.catalog)?;
    Ok((combined, Some(recording.catalog.clone())))
}

pub fn import(s: &mut Session, p: &Value) -> Result<Value> {
    let path = p.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| error("need path"))?;
    let values = p.get("streams").and_then(Value::as_array).ok_or_else(|| error("need streams from media.inspectStreams"))?;
    if values.is_empty() || values.len() > MAX_CONTAINER_STREAMS {
        return Err(error("select 1 to 128 streams"));
    }
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        for key in ["uid", "number"] {
            if value.get(key).and_then(Value::as_str).is_some_and(|v| v.len() > 20) {
                return Err(error("container stream identity is too long"));
            }
        }
        let id: StreamId = serde_json::from_value(value.clone()).map_err(|e| error(format!("invalid stream identity: {e}")))?;
        if let StreamId::Matroska { uid, number } = &id
            && (uid.parse::<u64>().is_err() || number.parse::<u64>().is_err())
        {
            return Err(error("invalid Matroska identity"));
        }
        if !seen.insert(id.clone()) {
            return Err(error("duplicate stream selection"));
        }
        selected.push(id);
    }
    let sequence = match p.get("sequence") {
        Some(v) => Some(ItemId(v.as_u64().ok_or_else(|| error("sequence must be an item id"))?)),
        None => s.state.active_sequence,
    };
    if sequence.is_some_and(|id| s.project.sequence(id).is_none()) {
        return Err(error("sequence does not exist"));
    }
    let at = match p.get("at") {
        Some(v) => Tick(v.as_i64().ok_or_else(|| error("at must be integer ticks"))?),
        None => s.playhead(),
    };
    if at < Tick::ZERO {
        return Err(error("placement anchor must be nonnegative"));
    }
    // Project allocators are infallible for established edits. Prove this transaction's
    // entire allocation budget before entering them, including hostile saved next_id values.
    let ids_needed = u64::try_from(selected.len())
        .ok()
        .and_then(|n| n.checked_mul(3))
        .and_then(|n| n.checked_add(u64::from(sequence.is_none())))
        .ok_or_else(|| error("stream identity allocation overflow"))?;
    s.project.next_id.checked_add(ids_needed).ok_or_else(|| error("project identity space exhausted"))?;
    let recording = s.media.recording(path, &*s.services)?;
    let identity = crate::relink::identity_of(&*s.services, path).map_err(|e| error(format!("{path}: {e}")))?;
    let mut prepared = Vec::new();
    for id in selected {
        let descriptor = recording.catalog.streams.iter().find(|d| d.id == id).ok_or_else(|| error("selected stream is missing"))?;
        let src = recording.select(&id)?;
        let info = src.info().clone();
        let start =
            descriptor.start.0.checked_sub(recording.catalog.origin.0).and_then(|d| at.0.checked_add(d)).ok_or_else(|| error("placement time overflow"))?;
        start.checked_add(info.duration.0).ok_or_else(|| error("stream end overflow"))?;
        let name = if descriptor.name.is_empty() { format!("Stream {:?}", id) } else { descriptor.name.clone() };
        let kind = if info.has_video() { TrackKind::Video } else { TrackKind::Audio };
        prepared.push((id, name, src, info, Tick(start), kind));
    }
    if let Some(q) = sequence.and_then(|id| s.project.sequence(id)) {
        for kind in [TrackKind::Video, TrackKind::Audio] {
            let existing = if kind == TrackKind::Video { q.video_tracks.len() } else { q.audio_tracks.len() };
            if existing.saturating_add(prepared.iter().filter(|p| p.5 == kind).count()) > crate::sequence_tools::MAX_TRACKS {
                return Err(error("sequence track limit exceeded"));
            }
        }
    }
    let mut settings = SequenceSettings::default();
    if let Some(v) = prepared.iter().find_map(|p| p.3.video.as_ref()) {
        settings.width = v.width;
        settings.height = v.height;
        settings.frame_rate = v.frame_rate;
        settings.par = v.par;
        settings.preset = format!("{}x{} {}", v.width, v.height, v.frame_rate.label());
    }
    if sequence.is_none() {
        settings.validate().map_err(error)?;
    }
    let labels = prepared.iter().map(|p| s.prefs.labels.for_media(p.3.kind, p.3.has_video(), p.3.has_audio())).collect::<Vec<_>>();
    let (seq, items, clips) = s.edit("Import Recording Streams", |project, state| {
        let seq = sequence.unwrap_or_else(|| project.new_sequence("Recording", settings, 0, 0, None));
        let sequence_settings = project.sequence(seq).ok_or(EngineError::NoSequence)?.settings.clone();
        let rate = sequence_settings.frame_rate;
        let mut items = Vec::new();
        let mut clips = Vec::new();
        for ((id, name, _, info, start, kind), label) in prepared.iter().zip(&labels) {
            let item = project.add_item(
                name,
                *label,
                ItemKind::Media(Box::new(MediaClip {
                    stream: Some(id.clone()),
                    recording: None,
                    media: MediaRef::File { path: path.to_string() },
                    info: info.clone(),
                    interpret: Default::default(),
                    mark_in: None,
                    mark_out: None,
                    markers: Vec::new(),
                    offline: false,
                    proxy: None,
                    identity: Some(identity),
                })),
                None,
            );
            let mut clip = project
                .make_track_item(item, *kind, *start, TimeRange::new(Tick::ZERO, info.duration), rate)
                .ok_or_else(|| error("cannot create stream clip"))?;
            // Initial recording placement preserves subframe starts and exact source durations.
            clip.duration = info.duration;
            if let Some(video) = &info.video {
                for effect in &mut clip.effects {
                    filmcraft_project::resolve_auto_points(effect, (sequence_settings.width, sequence_settings.height), (video.width, video.height));
                }
            }
            if let Some(audio) = info.audio() {
                let channels = u16::try_from(audio.channels).map_err(|_| error("audio channel count exceeds channel map limits"))?;
                clip.source_channels = (0..channels).collect();
            }
            clips.push(clip.id);
            items.push(item);
            let mut track = Track::new(TrackId(project.alloc_id()), *kind, name.clone());
            if let Some(audio) = info.audio() {
                track.channels = match audio.channels {
                    1 => filmcraft_project::AudioChannels::Mono,
                    2 => filmcraft_project::AudioChannels::Stereo,
                    _ => filmcraft_project::AudioChannels::Adaptive,
                };
            }
            track.items.push(clip);
            let q = project.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            if *kind == TrackKind::Video {
                q.video_tracks.push(track);
            } else {
                q.audio_tracks.push(track);
            }
        }
        state.active_sequence = Some(seq);
        if !state.open_sequences.contains(&seq) {
            state.open_sequences.push(seq);
        }
        state.selection = clips.clone();
        state.project_selection = items.clone();
        Ok((seq, items, clips))
    })?;
    s.media.allow(path);
    for (item, prepared) in items.iter().zip(&prepared) {
        let m = s.project.item(*item).and_then(|i| i.as_media()).ok_or_else(|| error("imported media is missing"))?;
        s.media.insert_selected_file(*item, path, crate::media_pool::media_key(m), prepared.2.clone());
    }
    s.events.push(crate::Event::OpenSequence(seq));
    Ok(json!({"sequence":seq.0,"items":items.iter().map(|i| i.0).collect::<Vec<_>>(),"clips":clips.iter().map(|c| c.0).collect::<Vec<_>>()}))
}
