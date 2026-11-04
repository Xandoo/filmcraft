//! Expand one Project-panel recording through the ordinary timeline placement command.
use crate::{EngineError, Result, Session, commands::ItemPlacement};
use filmcraft_media::{
    SharedSource,
    streams::{MAX_CONTAINER_STREAMS, StreamInfo, StreamKind},
};
use filmcraft_project::{AudioChannels, ClipId, ItemId, ItemKind, MediaClip, MediaRef, Project, Track, TrackId, TrackKind};
use filmcraft_time::{Tick, TimeRange};

struct Part {
    stream: StreamInfo,
    source: SharedSource,
    kind: TrackKind,
    lane: usize,
    range: TimeRange,
    at: Tick,
    channels: Option<Vec<u16>>,
}
fn error(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}

fn window(stream: &StreamInfo, origin: Tick, range: TimeRange, at: Tick) -> Result<Option<(TimeRange, Tick)>> {
    let a = range.start.0;
    let b = a.checked_add(range.duration.0).ok_or_else(|| error("recording source end overflow"))?;
    let first = stream.start.0.checked_sub(origin.0).ok_or_else(|| error("recording start overflow"))?;
    let last = stream.end.0.checked_sub(origin.0).ok_or_else(|| error("recording end overflow"))?;
    let (start, end) = (a.max(first), b.min(last));
    if end <= start {
        return Ok(None);
    }
    let local = start.checked_sub(first).ok_or_else(|| error("recording source offset overflow"))?;
    let duration = end.checked_sub(start).ok_or_else(|| error("recording duration overflow"))?;
    let placed = start.checked_sub(a).and_then(|d| at.0.checked_add(d)).ok_or_else(|| error("recording placement overflow"))?;
    placed.checked_add(duration).ok_or_else(|| error("recording placement end overflow"))?;
    if placed < 0 {
        return Err(error("recording placement must not be negative"));
    }
    Ok(Some((TimeRange::new(Tick(local), Tick(duration)), Tick(placed))))
}

pub(crate) fn place(s: &mut Session, placement: ItemPlacement<'_>) -> Result<Vec<ClipId>> {
    let (_, media, _) = s.project.resolve_media(placement.item).ok_or_else(|| error("recording item is missing"))?;
    let media = media.clone();
    if media.stream.is_some() {
        return Err(error("conflicting recording and stream selections"));
    }
    let catalog = media.recording.as_ref().ok_or_else(|| error("recording catalog is missing"))?;
    if catalog.streams.is_empty() || catalog.streams.len() > MAX_CONTAINER_STREAMS {
        return Err(error("invalid recording stream count"));
    }
    let MediaRef::File { path } = &media.media else {
        return Err(error("recording requires its original media file"));
    };
    let file = s.media.follow(path).map_err(|e| error(format!("recording cannot be opened: {e:?}")))?;
    let identity = crate::relink::identity_of(&*s.services, &file).map_err(|e| error(e.to_string()))?;
    if media.identity.is_some_and(|id| id != identity) {
        return Err(error("recording changed; relink the original before placing its streams"));
    }
    let recording = s.media.recording(&file, &*s.services)?;
    recording.combined(catalog)?;
    let videos: Vec<_> = catalog.streams.iter().filter(|d| d.kind == StreamKind::Video).collect();
    let audios: Vec<_> = catalog.streams.iter().filter(|d| d.kind == StreamKind::Audio).collect();
    let map = media.interpret.audio_channels.as_ref().map_or(&[][..], |m| m.clips.as_slice());
    let specs = if audios.is_empty() { Vec::new() } else { crate::commands::audio_placement_specs(map, audios.len()) };
    let mut parts = Vec::new();
    let mut prepare = |descriptor: &StreamInfo, kind, lane, range, at, channels| -> Result<()> {
        let actual = recording.catalog.streams.iter().find(|d| d.id == descriptor.id).ok_or_else(|| error("recording stream is missing"))?;
        if actual.kind != descriptor.kind {
            return Err(error("recording stream kind changed"));
        }
        if let Some((range, at)) = window(actual, recording.catalog.origin, range, at)? {
            let mut stream = actual.clone();
            if stream.name.is_empty() {
                stream.name = format!("{} {}", if kind == TrackKind::Video { "Video" } else { "Audio" }, lane + 1);
            }
            parts.push(Part { stream, source: recording.select(&actual.id)?, kind, lane, range, at, channels });
        }
        Ok(())
    };
    if placement.vdest.is_some() {
        for (lane, descriptor) in videos.iter().enumerate() {
            prepare(descriptor, TrackKind::Video, lane, placement.range, placement.at, None)?;
        }
    }
    if placement.adest.is_some() {
        let (range, at) = placement.audio_split.unwrap_or((placement.range, placement.at));
        for spec in &specs {
            let descriptor = audios.get(spec.audio_stream).ok_or_else(|| error("recording audio stream is missing"))?;
            let channels = spec.source_channels.clone().or_else(|| if spec.audio_stream == 0 { map.first().cloned() } else { None });
            prepare(descriptor, TrackKind::Audio, spec.track_offset, range, at, channels)?;
        }
    }
    if parts.is_empty() {
        return Err(error("the source range contains no streams for the destination tracks"));
    }
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let q = s.project.sequence(seq_id).ok_or(EngineError::NoSequence)?;
    let existing = q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.items.len().saturating_add(t.transitions.len())).fold(0usize, usize::saturating_add);
    let budget =
        u64::try_from(parts.len().saturating_mul(2).saturating_add(videos.len()).saturating_add(specs.len()).saturating_add(existing.saturating_mul(2)))
            .map_err(|_| error("recording identity budget overflow"))?;
    s.project.next_id.checked_add(budget).ok_or_else(|| error("project identity space exhausted"))?;
    let pool = s.media.clone();
    let label = s.project.item(placement.item).map(|i| i.label).ok_or_else(|| error("recording item is missing"))?;
    let scaling = s.prefs.media.default_media_scaling.clone();
    let (ids, cached) = s.edit(placement.label, |p, state| {
        let vtracks = tracks(p, seq_id, placement.vdest, videos.len(), TrackKind::Video, placement.label)?;
        let atracks = tracks(p, seq_id, placement.adest, specs.len(), TrackKind::Audio, placement.label)?;
        let settings = p.sequence(seq_id).ok_or(EngineError::NoSequence)?.settings.clone();
        let mut placements = Vec::new();
        let mut cached = Vec::new();
        for part in &parts {
            let mut selected = media.clone();
            selected.recording = None;
            selected.stream = Some(part.stream.id.clone());
            selected.info = part.source.info().clone();
            selected.mark_in = None;
            selected.mark_out = None;
            selected.proxy = None;
            selected.interpret.audio_channels = None;
            let key = crate::media_pool::media_key(&selected);
            let item = selected_item(p, selected, &part.stream.name, label)?;
            let tid = if part.kind == TrackKind::Video { vtracks.get(part.lane) } else { atracks.get(part.lane) }
                .copied()
                .ok_or_else(|| error("stream destination is missing"))?;
            let mut clip = p.make_track_item(item, part.kind, part.at, part.range, settings.frame_rate).ok_or_else(|| error("cannot create stream clip"))?;
            clip.duration = part.range.duration;
            if let Some(channels) = &part.channels {
                clip.source_channels = channels.clone();
            }
            if let Some(v) = &part.source.info().video {
                for effect in &mut clip.effects {
                    filmcraft_project::resolve_auto_points(effect, (settings.width, settings.height), (v.width, v.height));
                }
                let shown = filmcraft_project::conformed_size((v.width, v.height), v.par, settings.par);
                crate::settings::apply_media_scaling(&mut clip, &scaling, (settings.width, settings.height), shown);
            }
            name_track(p, seq_id, tid, part)?;
            placements.push((tid, clip));
            cached.push((item, key, part.source.clone()));
        }
        let snapshot = std::sync::Arc::new(p.clone());
        let snap = snapshot.clone();
        let durations = move |id| crate::media_duration(&snapshot, &pool, id);
        let starts = move |id| crate::media_start(&snap, id);
        let mut next = p.next_id;
        let seq = p.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
        let mut ctx = filmcraft_edit::EditCtx {
            next_id: &mut next,
            media_duration: &durations,
            media_start: &starts,
            min_duration: settings.frame_rate.frame_duration(),
        };
        let span = placements.iter().map(|(_, c)| c.start).min().zip(placements.iter().map(|(_, c)| c.end()).max());
        let ids = if placement.insert { filmcraft_edit::insert(seq, placements, &mut ctx)? } else { filmcraft_edit::overwrite(seq, placements, &mut ctx)? };
        if placement.insert
            && state.ripple_sequence_markers
            && let Some((a, b)) = span
        {
            crate::sequence_tools::ripple_markers(&mut seq.markers, a, b - a);
        }
        p.next_id = next;
        state.selection = ids.clone();
        Ok((ids, cached))
    })?;
    for (item, key, source) in cached {
        s.media.insert_selected_file(item, path, key, source);
    }
    Ok(ids)
}

fn selected_item(p: &mut Project, selected: MediaClip, name: &str, label: filmcraft_project::Label) -> Result<ItemId> {
    if let Some(id) = p.items.values().find_map(|i| i.as_media().filter(|m| **m == selected).map(|_| i.id)) {
        return Ok(id);
    }
    let id = p.add_item(name, label, ItemKind::Media(Box::new(selected)), None);
    // Timeline references persist in the project, without adding stream items to the bin.
    if !p.root.remove_item(id) {
        return Err(error("cannot retain the recording's project item"));
    }
    Ok(id)
}

fn name_track(p: &mut Project, seq: ItemId, tid: TrackId, part: &Part) -> Result<()> {
    let q = p.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
    let target = if part.kind == TrackKind::Video { &mut q.video_tracks } else { &mut q.audio_tracks };
    if let Some(track) = target.iter_mut().find(|t| t.id == tid) {
        let prefix = if part.kind == TrackKind::Video { "Video " } else { "Audio " };
        if track.items.is_empty() && track.name.strip_prefix(prefix).is_some_and(|v| v.parse::<usize>().is_ok()) {
            track.name = part.stream.name.clone();
            if let Some(a) = part.source.info().audio() {
                track.channels = match a.channels {
                    1 => AudioChannels::Mono,
                    2 => AudioChannels::Stereo,
                    _ => AudioChannels::Adaptive,
                };
            }
        }
    }
    Ok(())
}

fn tracks(p: &mut Project, seq: ItemId, destination: Option<TrackId>, count: usize, kind: TrackKind, label: &str) -> Result<Vec<TrackId>> {
    let Some(destination) = destination.filter(|_| count > 0) else {
        return Ok(Vec::new());
    };
    let q = p.sequence(seq).ok_or(EngineError::NoSequence)?;
    let current = if kind == TrackKind::Video { &q.video_tracks } else { &q.audio_tracks };
    let first = current.iter().position(|t| t.id == destination).ok_or_else(|| error("recording destination has the wrong track kind"))?;
    let need = first.checked_add(count).filter(|n| *n <= crate::sequence_tools::MAX_TRACKS).ok_or_else(|| error("recording exceeds sequence track limit"))?;
    if kind == TrackKind::Audio {
        return Ok(crate::commands::ensure_audio_tracks(p, seq, first, count, label)?.into_iter().skip(first).take(count).collect());
    }
    for k in current.len()..need {
        let id = TrackId(p.alloc_id());
        p.sequence_mut(seq).ok_or(EngineError::NoSequence)?.video_tracks.push(Track::new(id, kind, format!("Video {}", k + 1)));
    }
    Ok(p.sequence(seq).ok_or(EngineError::NoSequence)?.video_tracks.iter().skip(first).take(count).map(|t| t.id).collect())
}
