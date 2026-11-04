//! Bounded, cancellable transcription workers. Only the session thread commits results.
use crate::{EngineError, Job, Result, Session, commands::bad};
use filmcraft_project::{ClipId, ItemId, TrackItem, Transcript};
use filmcraft_speech::{Options, SpeechError, Transcriber};
use filmcraft_time::Tick;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, atomic::Ordering};

static RECOGNITION: Mutex<()> = Mutex::new(());
const CHUNK: usize = 60 * filmcraft_speech::SAMPLE_RATE as usize;
const MAX_ITEMS: usize = 1024;
const MAX_WORDS: usize = 2_000_000;
pub(crate) type Cache = Arc<Mutex<Option<(std::path::PathBuf, String, String, Arc<dyn Transcriber>)>>>;

struct Input {
    item: ItemId,
    descriptor: Value,
    source: filmcraft_media::SharedSource,
    ranges: Vec<AudioRange>,
    clips: Vec<(ItemId, ClipId, Value)>,
    existing: Option<Arc<Transcript>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioRange {
    start: i64,
    end: i64,
    stream: usize,
}

fn clip_range(clip: &TrackItem) -> Result<AudioRange> {
    let consumed = clip.duration.0 as f64 * clip.speed;
    if clip.start.0.checked_add(clip.duration.0).is_none()
        || clip.duration.0 <= 0
        || !clip.speed.is_finite()
        || clip.speed <= 0.0
        || !consumed.is_finite()
        || !(1.0..i64::MAX as f64).contains(&consumed)
        || clip.source_in.0 < 0
        || clip.reverse
        || clip.frame_hold.is_some()
    {
        return Err(error("transcription needs a forward-playing audio clip with valid trim bounds"));
    }
    let end = clip.source_in.0.checked_add(consumed.round() as i64).ok_or_else(|| error("audio trim bounds overflow"))?;
    let sr = filmcraft_speech::SAMPLE_RATE as i64;
    // Read only complete samples inside the trim, never samples outside it.
    let start = clip.source_in.to_units_floor(sr);
    let start = start.checked_add(i64::from(filmcraft_speech::sample_tick(start) < clip.source_in)).ok_or_else(|| error("audio sample offset overflow"))?;
    let end = Tick(end).to_units_floor(sr);
    if end <= start || end - start > 72 * 3600 * sr {
        return Err(error("trimmed audio duration must be between one sample and 72 hours"));
    }
    Ok(AudioRange { start, end, stream: clip.audio_stream })
}

type ClipPlan = BTreeMap<ItemId, (Vec<AudioRange>, Vec<(ItemId, ClipId, Value)>)>;
fn selected_ranges(s: &Session, p: &Value) -> Result<Option<ClipPlan>> {
    let explicit = p.get("clips");
    let ids: Vec<ClipId> = if let Some(raw) = explicit {
        let ids = raw
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= MAX_ITEMS)
            .ok_or_else(|| bad("transcript.start", "clips must contain 1–1024 clip identities"))?;
        ids.iter().map(|v| v.as_u64().filter(|v| *v > 0).map(ClipId).ok_or_else(|| bad("transcript.start", "invalid clip identity"))).collect::<Result<_>>()?
    } else if p.get("items").is_some() || p.get("item").is_some() {
        return Ok(None);
    } else {
        s.state.selection.clone()
    };
    if ids.is_empty() {
        return Ok(None);
    }
    if ids.len() > MAX_ITEMS {
        return Err(error("select at most 1024 audio clips"));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let sequence = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let mut plan = ClipPlan::new();
    for id in ids {
        let clip = q.audio_tracks.iter().flat_map(|t| &t.items).find(|it| it.id == id && it.enabled);
        let Some(clip) = clip else {
            if explicit.is_some() {
                return Err(error("requested clip is not enabled timeline audio"));
            }
            continue;
        };
        let range = clip_range(clip)?;
        let entry = plan.entry(clip.item).or_default();
        entry.0.push(range);
        entry.1.push((sequence, id, serde_json::to_value(clip).map_err(error)?));
    }
    if plan.is_empty() {
        return Err(error("select an enabled timeline audio clip to transcribe"));
    }
    for (ranges, _) in plan.values_mut() {
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<AudioRange> = Vec::new();
        for range in ranges.iter().copied() {
            if merged.first().is_some_and(|r| r.stream != range.stream) {
                return Err(error("select one audio stream per media item for transcription"));
            }
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        *ranges = merged;
    }
    Ok(Some(plan))
}

// Replace only the recognized spans; retain transcripts of other uses of the same source.
fn merge_transcript(existing: &Transcript, mut new: Transcript, ranges: &[AudioRange]) -> std::result::Result<Transcript, String> {
    if existing.words.len() > MAX_WORDS || existing.speakers.len() > 32 {
        return Err("existing transcript exceeds merge limits".into());
    }
    let mut merged = existing.clone();
    merged.words.retain(|w| !ranges.iter().any(|r| w.end > filmcraft_speech::sample_tick(r.start) && w.start < filmcraft_speech::sample_tick(r.end)));
    for w in &mut new.words {
        if let Some(speaker) = w.speaker {
            let label = new.speakers.get(speaker as usize).ok_or("invalid speaker identity")?;
            let index = if let Some(i) = merged.speakers.iter().position(|s| s == label) {
                i
            } else {
                if merged.speakers.len() >= 32 {
                    return Err("merged transcript exceeds speaker limit".into());
                }
                merged.speakers.push(label.clone());
                merged.speakers.len() - 1
            };
            w.speaker = Some(index as u32);
        }
    }
    if merged.words.len().saturating_add(new.words.len()) > MAX_WORDS {
        return Err("merged transcript exceeds word limit".into());
    }
    merged.source = new.source;
    if merged.language.is_empty() {
        merged.language = new.language;
    }
    merged.words.extend(new.words);
    merged.normalize();
    merged.check()?;
    Ok(merged)
}

pub struct Pending {
    job: u64,
    epoch: u64,
    inputs: Vec<Input>,
    output: Arc<Mutex<Option<Vec<(ItemId, Transcript)>>>>,
    text_track: Option<(ItemId, Value, crate::transcript_tracks::TextTrackOptions)>,
}

pub(crate) fn queued(s: &Session, item: ItemId) -> bool {
    s.transcription_jobs.iter().any(|j| j.epoch == s.document_epoch && j.inputs.iter().any(|i| i.item == item))
}
fn lock<T>(v: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    v.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn cancellable_lock<'a, T>(v: &'a Mutex<T>, cancel: &std::sync::atomic::AtomicBool) -> std::result::Result<std::sync::MutexGuard<'a, T>, String> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(SpeechError::Cancelled.to_string());
        }
        match v.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => return Ok(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}
fn error(message: impl ToString) -> EngineError {
    EngineError::Other(message.to_string())
}

pub(crate) fn start(s: &mut Session, p: &Value) -> Result<Value> {
    crate::transcript::can_transcribe(s).map_err(error)?;
    let text_track = p
        .get("textTrack")
        .map(|value| -> Result<_> {
            let options = crate::transcript_tracks::TextTrackOptions::from_params(value)?;
            let id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
            let q = s.project.sequence(id).ok_or(EngineError::NoSequence)?;
            Ok((id, serde_json::to_value(q).map_err(error)?, options))
        })
        .transpose()?;
    if p.get("clips").is_some() && (p.get("items").is_some() || p.get("item").is_some()) {
        return Err(bad("transcript.start", "choose timeline clips or source media items, not both"));
    }
    if let Some(raw) = p.get("items") {
        let items = raw
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= MAX_ITEMS)
            .ok_or_else(|| bad("transcript.start", "items must contain 1–1024 media identities"))?;
        if items.iter().any(|v| v.as_u64().is_none_or(|v| v == 0)) {
            return Err(bad("transcript.start", "invalid media identity"));
        }
    }
    if p.get("item").is_some_and(|v| v.as_u64().is_none_or(|v| v == 0)) {
        return Err(bad("transcript.start", "invalid media identity"));
    }
    for name in ["diarize", "automatic", "wait"] {
        if p.get(name).is_some_and(|v| !v.is_boolean()) {
            return Err(bad("transcript.start", format!("{name} must be boolean")));
        }
    }
    if s.transcription_jobs.len() >= 16 {
        return Err(error("too many queued transcription jobs"));
    }
    let mut plan = selected_ranges(s, p)?;
    if plan.is_none() && p.get("items").is_none() && p.get("item").is_none() {
        return Err(error("select an enabled timeline audio clip to transcribe"));
    }
    let items = plan.as_ref().map(|v| v.keys().copied().collect()).unwrap_or_else(|| crate::transcript::targets(s, p));
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(bad("transcript.start", "select between 1 and 1024 audio items"));
    }
    let model = match p.get("model") {
        Some(Value::String(v)) if !v.is_empty() && v.len() <= 128 => v.clone(),
        Some(_) => return Err(bad("transcript.start", "invalid model identity")),
        None => s.prefs.media_analysis.whisper_model.clone(),
    };
    let device = match p.get("device") {
        Some(Value::String(v)) if ["auto", "gpu", "cpu"].contains(&v.as_str()) => v.clone(),
        Some(_) => return Err(bad("transcript.start", "device must be auto, gpu or cpu")),
        None => s.prefs.media_analysis.speech_device.clone(),
    };
    if s.transcriber.is_none() && filmcraft_speech::models::find(&model).is_none() {
        return Err(error(SpeechError::UnknownModel(model)));
    }
    let ma = &s.prefs.media_analysis;
    let language = match p.get("language") {
        Some(Value::String(v)) if v.len() <= 16 => {
            if v.is_empty() || v == "auto" {
                None
            } else {
                Some(v.clone())
            }
        }
        Some(_) => return Err(bad("transcript.start", "invalid language")),
        None => {
            if ma.language_auto_detect {
                None
            } else {
                Some(ma.default_language.clone())
            }
        }
    };
    let max_speakers = match p.get("maxSpeakers") {
        Some(v) => v.as_u64().filter(|v| (1..=32).contains(v)).ok_or_else(|| bad("transcript.start", "maxSpeakers must be 1–32"))? as usize,
        None => Options::default().max_speakers,
    };
    let opts = Options { language, diarize: p.get("diarize").and_then(Value::as_bool).unwrap_or(ma.speaker_labeling != "off"), max_speakers };
    let automatic = p.get("automatic").and_then(Value::as_bool).unwrap_or(false);
    let mut inputs = Vec::new();
    for item in items {
        if s.transcription_jobs.iter().any(|j| j.epoch == s.document_epoch && j.inputs.iter().any(|i| i.item == item)) {
            continue;
        }
        if automatic && s.project.transcripts.contains_key(&item) {
            continue;
        }
        let Some(source) = s.source(item).filter(|v| v.info().has_audio()) else { continue };
        let duration = s.project.resolve_media(item).map(|(_, m, _)| m.duration()).unwrap_or_default();
        let samples = duration.to_units_floor(filmcraft_speech::SAMPLE_RATE as i64);
        if samples <= 0 || (plan.is_none() && samples > 72 * 3600 * filmcraft_speech::SAMPLE_RATE as i64) {
            return Err(error("audio duration must be between one sample and 72 hours"));
        }
        let (ranges, clips) = match plan.as_mut().and_then(|v| v.remove(&item)) {
            Some(v) => v,
            None => (vec![AudioRange { start: 0, end: samples, stream: 0 }], Vec::new()),
        };
        if ranges.iter().any(|r| r.end > samples || r.stream >= source.info().audio_streams.len()) {
            return Err(error("audio trim or stream lies outside the source media"));
        }
        inputs.push(Input {
            item,
            descriptor: serde_json::to_value(s.project.item(item)).map_err(error)?,
            source,
            ranges,
            clips,
            existing: s.project.transcripts.get(&item).cloned(),
        });
    }
    if inputs.is_empty() {
        return Err(error("no new audio items to transcribe (items may already be queued)"));
    }
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0).checked_add(1).ok_or_else(|| error("job identity exhausted"))?;
    let job = Job { id, label: format!("Transcribe ({model})"), progress: Default::default(), result: Default::default() };
    job.progress.total.store(1_000_000, Ordering::Relaxed);
    let output: Arc<Mutex<Option<Vec<(ItemId, Transcript)>>>> = Arc::default();
    let work: Vec<_> =
        inputs.iter().map(|i| (i.item, i.source.clone(), i.ranges.clone(), if i.clips.is_empty() { None } else { i.existing.clone() })).collect();
    let (progress, result, out) = (job.progress.clone(), job.result.clone(), output.clone());
    let supplied = s.transcriber.clone();
    let cache = s.speech_cache.clone();
    let directory = crate::transcript::models_dir();
    let run = move || {
        let started = web_time::Instant::now();
        let task = || -> std::result::Result<Vec<(ItemId, Transcript)>, String> {
            *lock(&progress.status) = "Waiting for transcription worker".into();
            let _serial = cancellable_lock(&RECOGNITION, &progress.cancel)?;
            let report = |fraction: f32, status: &str| {
                *lock(&progress.status) = status.to_string();
                progress.done.store((fraction.clamp(0.0, 1.0) * 1_000_000.0) as u64, Ordering::Relaxed);
                !progress.cancel.load(Ordering::Relaxed)
            };
            if !report(0.0, "Preparing transcription") {
                return Err("stopped".into());
            }
            let transcriber: Arc<dyn Transcriber> = match supplied {
                Some(v) => v,
                None => {
                    let directory = directory.ok_or("no model data directory")?;
                    let cached = lock(&cache)
                        .as_ref()
                        .filter(|(dir, id, preference, _)| dir == &directory && id == &model && preference == &device)
                        .map(|(_, _, _, t)| t.clone());
                    if let Some(transcriber) = cached {
                        transcriber
                    } else {
                        // Keep a single resident model; release the previous device allocation first.
                        lock(&cache).take();
                        if !report(0.15, "Verifying and loading model") {
                            return Err("stopped".into());
                        }
                        let transcriber = match filmcraft_speech::load_on_device(&directory, &model, &device) {
                            Ok(transcriber) => transcriber,
                            Err(SpeechError::NotInstalled(_)) => {
                                #[cfg(feature = "speech-download")]
                                {
                                    let info = filmcraft_speech::models::find(&model).ok_or("unknown speech model")?;
                                    filmcraft_speech::models::download(&directory, info, &mut |done, total, file| {
                                        report(0.15 * done as f32 / total.max(1) as f32, &format!("Downloading {file}"))
                                    })
                                    .map_err(|e| e.to_string())?;
                                    if !report(0.15, "Loading model") {
                                        return Err("stopped".into());
                                    }
                                    filmcraft_speech::load_on_device(&directory, &model, &device).map_err(|e| e.to_string())?
                                }
                                #[cfg(not(feature = "speech-download"))]
                                return Err("model is missing and this build has no model downloads".into());
                            }
                            Err(e) => return Err(e.to_string()),
                        };
                        *lock(&cache) = Some((directory, model.clone(), device.clone(), transcriber.clone()));
                        transcriber
                    }
                }
            };
            let prepared = started.elapsed();
            let mut audio_seconds = 0.0f64;
            let mut recognition_seconds = 0.0f64;
            let total: u64 = work.iter().flat_map(|(_, _, ranges, _)| ranges).map(|r| ((r.end - r.start) as u64).div_ceil(CHUNK as u64)).sum();
            let mut complete = 0usize;
            let mut total_words = 0usize;
            let mut transcripts = Vec::new();
            for (item, source, ranges, existing) in work {
                let mut transcript = Transcript { source: transcriber.id(), ..Default::default() };
                for range in &ranges {
                    for start in (range.start..range.end).step_by(CHUNK) {
                        if !report(0.2 + 0.8 * complete as f32 / total.max(1) as f32, "Reading audio") {
                            return Err("stopped".into());
                        }
                        let count = usize::try_from((range.end - start).min(CHUNK as i64)).map_err(|e| e.to_string())?;
                        let audio = source.audio_stream(range.stream, start, count, filmcraft_speech::SAMPLE_RATE).map_err(|e| e.to_string())?;
                        if audio.channels.len() > 64 || audio.channels.iter().any(|c| c.len() > count) {
                            return Err("audio reader returned an oversized buffer".into());
                        }
                        let mut mono = filmcraft_speech::downmix(&audio.channels);
                        if mono.len() > CHUNK || mono.iter().any(|v| !v.is_finite()) {
                            return Err("audio reader returned an invalid chunk".into());
                        }
                        let peak = mono.iter().fold(1.0f32, |peak, v| peak.max(v.abs()));
                        if peak > 1.0 {
                            for sample in &mut mono {
                                *sample /= peak;
                            }
                        }
                        let inference_started = web_time::Instant::now();
                        let mut part = transcriber
                            .transcribe(&mono, &opts, &mut |fraction, status| {
                                report(0.2 + 0.8 * (complete as f32 + fraction) / total.max(1) as f32, &format!("{status} · {}", transcriber.device()))
                            })
                            .map_err(|e| e.to_string())?;
                        recognition_seconds += inference_started.elapsed().as_secs_f64();
                        audio_seconds += mono.len() as f64 / filmcraft_speech::SAMPLE_RATE as f64;
                        total_words = total_words.saturating_add(part.words.len());
                        if total_words > MAX_WORDS
                            || part.speakers.len() > 32
                            || part.words.iter().any(|w| w.speaker.is_some_and(|s| s >= 32) || w.text.len() > 32768)
                        {
                            return Err("transcript exceeds word limit".into());
                        }
                        let end = filmcraft_speech::sample_tick(mono.len() as i64);
                        if part.words.iter().any(|w| w.start < filmcraft_time::Tick::ZERO || w.end < w.start || w.end > end || !w.confidence.is_finite()) {
                            return Err("recognizer returned invalid word bounds or confidence".into());
                        }
                        part.normalize();
                        part.check()?;
                        let offset = filmcraft_speech::sample_tick(start);
                        for word in &mut part.words {
                            word.start = TickChecked::add(word.start, offset)?;
                            word.end = TickChecked::add(word.end, offset)?;
                        }
                        if transcript.language.is_empty() {
                            transcript.language = part.language;
                        }
                        if part.speakers.len() > transcript.speakers.len() {
                            transcript.speakers = part.speakers;
                        }
                        transcript.words.extend(part.words);
                        complete = complete.saturating_add(1);
                    }
                }
                transcript.normalize();
                transcript.check()?;
                if let Some(existing) = existing {
                    transcript = merge_transcript(&existing, transcript, &ranges)?;
                }
                transcripts.push((item, transcript));
            }
            if !report(
                1.0,
                &format!(
                    "Transcription complete · {} · {:.1}s preparation · {:.1}s recognition / {:.1}s audio",
                    transcriber.device(),
                    prepared.as_secs_f64(),
                    recognition_seconds,
                    audio_seconds
                ),
            ) {
                return Err("stopped".into());
            }
            Ok(transcripts)
        };
        let completed = match task() {
            Ok(transcripts) => {
                *lock(&out) = Some(transcripts);
                Ok(filmcraft_export::Report {
                    path: String::new(),
                    frames: 0,
                    seconds: started.elapsed().as_secs_f64(),
                    bytes: 0,
                    render_fps: 0.0,
                    extra_files: Vec::new(),
                })
            }
            Err(e) => {
                *lock(&progress.status) = e.clone();
                Err(e)
            }
        };
        *lock(&result) = Some(completed);
        progress.finished.store(true, Ordering::Release);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let item_ids: Vec<_> = inputs.iter().map(|i| i.item.0).collect();
    s.jobs.push(job.clone());
    s.transcription_jobs.push(Pending { job: id, epoch: s.document_epoch, inputs, output, text_track });
    if p.get("wait").and_then(Value::as_bool) == Some(true) || cfg!(target_arch = "wasm32") {
        run();
        poll(s);
    } else if let Err(e) = std::thread::Builder::new().name("filmcraft-transcribe".into()).spawn(run) {
        *lock(&job.result) = Some(Err(e.to_string()));
        job.progress.finished.store(true, Ordering::Release);
        return Err(error(e));
    }
    Ok(json!({"job":id,"items":item_ids}))
}

struct TickChecked;
impl TickChecked {
    fn add(a: filmcraft_time::Tick, b: filmcraft_time::Tick) -> std::result::Result<filmcraft_time::Tick, String> {
        a.0.checked_add(b.0).map(filmcraft_time::Tick).ok_or_else(|| "transcript timestamp overflow".into())
    }
}

pub(crate) fn poll(s: &mut Session) {
    let mut index = 0;
    while index < s.transcription_jobs.len() {
        let pending = &s.transcription_jobs[index];
        let Some(job) = s.jobs.iter().find(|j| j.id == pending.job) else {
            s.transcription_jobs.remove(index);
            continue;
        };
        if !job.progress.finished.load(Ordering::Acquire) {
            index += 1;
            continue;
        }
        let job = job.clone();
        let pending = s.transcription_jobs.remove(index);
        let Some(transcripts) = lock(&pending.output).take() else {
            if !job.progress.cancel.load(Ordering::Relaxed)
                && let Some(Err(e)) = lock(&job.result).as_ref()
            {
                s.error_toast("transcript.start", format!("Transcription: {e}"));
            }
            continue;
        };
        if job.progress.cancel.load(Ordering::Relaxed) {
            continue;
        }
        let stale = pending.epoch != s.document_epoch
            || pending.text_track.as_ref().is_some_and(|(id, before, _)| serde_json::to_value(s.project.sequence(*id)).ok().as_ref() != Some(before))
            || pending.inputs.iter().any(|i| {
                i.clips.iter().any(|(seq, id, before)| {
                    s.project.sequence(*seq).and_then(|q| q.find_item(*id)).and_then(|(_, it)| serde_json::to_value(it).ok()).as_ref() != Some(before)
                }) || serde_json::to_value(s.project.item(i.item)).ok().as_ref() != Some(&i.descriptor)
                    || s.project.transcripts.get(&i.item) != i.existing.as_ref()
                    || s.source(i.item).is_none_or(|source| !Arc::ptr_eq(&source, &i.source))
            });
        if stale {
            *lock(&job.result) = Some(Err("source media or project changed; transcription discarded".into()));
            s.error_toast("transcript.start", "Source media or project changed; transcription discarded");
            continue;
        }
        let applied = s.edit("Transcribe", move |project, _| {
            let items: std::collections::BTreeSet<_> = transcripts.iter().map(|(id, _)| *id).collect();
            for (item, transcript) in transcripts {
                project.transcripts.insert(item, Arc::new(transcript));
            }
            if let Some((seq, _, options)) = pending.text_track {
                let q = project.sequence(seq).ok_or(EngineError::NoSequence)?;
                let clips: std::collections::BTreeSet<_> = pending.inputs.iter().flat_map(|i| i.clips.iter().map(|(_, id, _)| *id)).collect();
                let words: Vec<_> = filmcraft_edit::transcript::sequence_words(q, &project.transcripts)
                    .into_iter()
                    .filter(|w| if clips.is_empty() { items.contains(&w.item) } else { clips.contains(&w.clip) })
                    .collect();
                crate::transcript_tracks::append(project, seq, &words, &options)?;
            }
            Ok(())
        });
        if let Err(e) = applied {
            *lock(&job.result) = Some(Err(e.to_string()));
            s.error_toast("transcript.start", e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_ranges_merge_overlap_but_keep_unselected_gaps_and_account_for_speed() {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let mut first = s.active_sequence().unwrap().audio_tracks[0].items[0].clone();
        first.source_in = Tick::from_seconds_f64(1.0);
        first.duration = Tick::from_seconds_f64(1.0);
        first.speed = 2.0;
        let mut second = first.clone();
        second.id = ClipId(900_001);
        second.source_in = Tick::from_seconds_f64(2.0);
        let mut third = first.clone();
        third.id = ClipId(900_002);
        third.source_in = Tick::from_seconds_f64(5.0);
        let item = first.item;
        let ids = vec![first.id, second.id, third.id, first.id];
        let seq = s.state.active_sequence.unwrap();
        Arc::make_mut(&mut s.project).sequence_mut(seq).unwrap().audio_tracks[0].items = vec![first, second, third];
        s.state.selection = ids;
        let plan = selected_ranges(&s, &json!({})).unwrap().unwrap();
        assert_eq!(plan[&item].0, vec![AudioRange { start: 16000, end: 64000, stream: 0 }, AudioRange { start: 80000, end: 112000, stream: 0 }]);
    }

    #[test]
    fn cancellation_interrupts_a_wait_for_the_inference_device() {
        let gate = Arc::new(Mutex::new(()));
        let _held = lock(&gate);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (worker_gate, worker_cancel) = (gate.clone(), cancel.clone());
        let worker = std::thread::spawn(move || {
            tx.send(cancellable_lock(&worker_gate, &worker_cancel).is_err()).unwrap();
        });
        cancel.store(true, Ordering::Relaxed);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }
}
