use std::sync::Arc;

use serde_json::json;

use crate::Session;
use filmcraft_project::{ItemId, Transcript, Word};
use filmcraft_speech::FixedTranscriber;
use filmcraft_time::Tick;

/// The demo project with a fake transcriber whose transcript fits the first A1 clip's media:
/// "Hello um world." (Speaker 1), a 1.2 s pause, "Second speaker here." (Speaker 2).
fn session() -> (Session, ItemId, Tick) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let a = &q.audio_tracks[0].items[0];
    let (item, sin) = (a.item, a.source_in);
    let sec = |x: f64| sin + Tick::from_seconds_f64(x);
    let mut t = Transcript { language: "en".into(), ..Default::default() };
    for (text, a, b, sp) in
        [("Hello", 0.2, 0.5, 0), ("um", 0.6, 0.9, 0), ("world.", 1.0, 1.4, 0), ("Second", 2.6, 3.0, 1), ("speaker", 3.0, 3.5, 1), ("here.", 3.5, 4.0, 1)]
    {
        let mut w = Word::new(text, sec(a), sec(b));
        w.speaker = Some(sp);
        t.words.push(w);
    }
    t.normalize();
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: t, id: "fixed".into() }));
    let start = a_start(&s);
    (s, item, start)
}

fn a_start(s: &Session) -> Tick {
    s.active_sequence().unwrap().audio_tracks[0].items[0].start
}

#[test]
fn transcript_text_clips_are_editable_grouped_timed_and_undoable() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
    let before = s.project.to_json();
    let undo = s.history.undo.len();
    let words = crate::transcript::sequence_words(&s);
    let first = words.first().unwrap();
    let rate = s.sequence_rate();
    let shown_at = first.start + rate.frame_duration();
    let without_text = s.render_program_at(0.25, shown_at).unwrap();
    let result = s.execute("transcript.createText", json!({"grouping":"words","maxWords":2})).unwrap();
    assert!(result["count"].as_u64().unwrap() > 0);
    let q = s.active_sequence().unwrap();
    let track = q.video_tracks.last().unwrap();
    assert_eq!(track.id.0, result["track"]);
    assert_eq!(track.items[0].start, rate.snap(first.start));
    assert_eq!(track.items[0].effects.last().unwrap().params["text"].value, filmcraft_project::ParamValue::Text("Hello um".into()));
    assert!(track.items.windows(2).all(|w| w[0].end() <= w[1].start));
    let id = track.items[0].id;
    let with_text = s.render_program_at(0.25, shown_at).unwrap();
    assert_ne!(with_text.px, without_text.px, "generated graphics must render over the footage");
    let changed = with_text.px.iter().zip(&without_text.px).filter(|(a, b)| (*a - *b).abs() > 0.001).count();
    assert!(changed < with_text.px.len() / 10, "text must preserve the picture outside its glyphs");
    if let Some(directory) = std::env::var_os("FILMCRAFT_UI_SHOTS") {
        let path = std::path::PathBuf::from(directory).join("transcript-frame.png");
        s.execute("file.exportFrame", json!({"path":path,"time":shown_at.0})).unwrap();
    }
    s.execute("graphics.setText", json!({"clip":id.0,"text":"Edited"})).unwrap();
    s.undo();
    assert_eq!(s.history.undo.len(), undo + 1);
    s.undo();
    assert_eq!(s.project.to_json(), before);
    s.redo();
    assert_eq!(s.active_sequence().unwrap().video_tracks.last().unwrap().id.0, result["track"]);
}

#[test]
fn transcript_text_settings_reject_hostile_input_without_touching_history() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
    s.execute("transcript.createText", json!({})).unwrap();
    s.undo(); // Preserve an existing redo as well as undo history on rejection.
    let before = s.project.to_json();
    let (undo, redo) = (s.history.undo.len(), s.history.redo.len());
    for p in [
        json!({"maxChars":u64::MAX}),
        json!({"maxWords":0}),
        json!({"maxSeconds":-1}),
        json!({"pauseSeconds":"NaN"}),
        json!({"fontSize":1e200}),
        json!({"grouping":"bytes"}),
        json!({"name":""}),
        json!(null),
    ] {
        for command in ["transcript.createText", "transcript.start"] {
            let params = if command == "transcript.start" { json!({"items":[item.0],"textTrack":p}) } else { p.clone() };
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, params))).unwrap().is_err());
            assert_eq!(s.project.to_json(), before);
            assert_eq!((s.history.undo.len(), s.history.redo.len()), (undo, redo));
        }
    }
}

#[test]
fn transcript_text_during_generation_uses_selected_trim_and_one_undo_step() {
    for command in ["transcript.start", "transcript.generate", "sequence.transcribe"] {
        let (mut s, _, _) = session();
        let audio = s.active_sequence().unwrap().audio_tracks[0].items[0].id;
        s.state.selection = vec![audio];
        let before = s.project.to_json();
        let undo = s.history.undo.len();
        let mut params = json!({"wait":true,"textTrack":{"grouping":"words","maxWords":1}});
        if command == "sequence.transcribe" {
            params["background"] = json!(true);
        }
        s.execute(command, params).unwrap();
        assert_eq!(s.history.undo.len(), undo + 1, "{command}: {:?}", s.jobs.iter().map(|j| j.to_json()).collect::<Vec<_>>());
        let track = s.active_sequence().unwrap().video_tracks.last().unwrap();
        assert_eq!(track.name, "Transcript Text");
        assert_eq!(track.items.len(), crate::transcript::sequence_words(&s).iter().filter(|w| w.clip == audio).count());
        s.undo();
        assert_eq!(s.project.to_json(), before);
        s.redo();
        assert_eq!(s.active_sequence().unwrap().video_tracks.last().unwrap().name, "Transcript Text");
    }
}

fn words(s: &mut Session) -> Vec<String> {
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    r["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap().to_string()).collect()
}

#[test]
fn background_transcription_applies_once_and_is_undoable() {
    let (mut s, item, _) = session();
    let before = s.project.to_json();
    let undo = s.history.undo.len();
    let r = s.execute("transcript.start", json!({"items":[item.0],"wait":true})).unwrap();
    assert!(s.project.transcripts.contains_key(&item), "{:?}", s.jobs.iter().map(|j| j.to_json()).collect::<Vec<_>>());
    assert_eq!(s.history.undo.len(), undo + 1);
    assert!(s.jobs.iter().any(|j| j.id == r["job"].as_u64().unwrap() && j.to_json()["finished"] == true));
    s.undo();
    assert_eq!(s.project.to_json(), before);
    s.redo();
    assert!(s.project.transcripts.contains_key(&item));
}

#[test]
fn background_transcription_rejects_hostile_options_without_mutation() {
    let (mut s, item, _) = session();
    let before = s.project.to_json();
    let undo = s.history.undo.len();
    for p in [
        json!({"items":[item.0],"maxSpeakers":-1}),
        json!({"items":[item.0],"maxSpeakers":u64::MAX}),
        json!({"items":[item.0],"language":42}),
        json!({"items":[item.0],"language":"x".repeat(100)}),
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("transcript.start", p))).unwrap().is_err());
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.history.undo.len(), undo);
    }
}

#[test]
fn background_transcription_rejects_malformed_recognizer_output() {
    struct Bad(filmcraft_project::Word);
    impl filmcraft_speech::Transcriber for Bad {
        fn id(&self) -> String {
            "bad".into()
        }
        fn transcribe(&self, _: &[f32], _: &filmcraft_speech::Options, _: filmcraft_speech::ProgressFn) -> Result<Transcript, filmcraft_speech::SpeechError> {
            Ok(Transcript { words: vec![self.0.clone()], ..Default::default() })
        }
    }
    let normal = Word::new("word", Tick::ZERO, Tick::from_seconds_f64(0.1));
    let mut speaker = normal.clone();
    speaker.speaker = Some(u32::MAX);
    let mut bounds = normal.clone();
    bounds.end = Tick::MAX;
    let mut confidence = normal.clone();
    confidence.confidence = f32::NAN;
    for word in [speaker, bounds, confidence] {
        let (mut s, item, _) = session();
        s.transcriber = Some(Arc::new(Bad(word)));
        let before = s.project.to_json();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("transcript.start", json!({"items":[item.0],"wait":true}))));
        assert!(result.unwrap().is_ok());
        assert_eq!(s.project.to_json(), before);
        assert!(s.jobs.last().unwrap().to_json()["result"]["error"].is_string());
    }
}

#[test]
fn background_transcription_discards_cancelled_and_stale_results() {
    use std::sync::{Mutex, mpsc};
    struct Gate {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl filmcraft_speech::Transcriber for Gate {
        fn id(&self) -> String {
            "gate".into()
        }
        fn transcribe(
            &self,
            _: &[f32],
            _: &filmcraft_speech::Options,
            progress: filmcraft_speech::ProgressFn,
        ) -> Result<Transcript, filmcraft_speech::SpeechError> {
            self.started.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            if !progress(1.0, "Ready") {
                return Err(filmcraft_speech::SpeechError::Cancelled);
            }
            Ok(Transcript { words: vec![Word::new("hello", Tick::ZERO, Tick::from_seconds_f64(0.1))], ..Default::default() })
        }
    }
    for mode in 0..5 {
        let (mut s, item, _) = session();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        s.transcriber = Some(Arc::new(Gate { started: started_tx, release: Mutex::new(release_rx) }));
        let clip = s.active_sequence().unwrap().audio_tracks[0].items[0].id;
        let mut params = if mode == 2 { json!({"clips":[clip.0]}) } else { json!({"items":[item.0]}) };
        if mode >= 3 {
            params["textTrack"] = json!({"maxChars":32});
        }
        s.execute("transcript.start", params).unwrap();
        started_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        match mode {
            1 => s.document_epoch = s.document_epoch.saturating_add(1),
            2 => s
                .edit_sequence("Change trim while transcribing", |q, _, _| {
                    q.find_item_mut(clip).unwrap().1.source_in += Tick::from_seconds_f64(0.1);
                    Ok(())
                })
                .unwrap(),
            4 => s
                .edit_sequence("Change sequence while transcribing text", |q, _, _| {
                    q.settings.width = 1280;
                    Ok(())
                })
                .unwrap(),
            _ => s.jobs.last().unwrap().progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed),
        }
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !s.transcription_jobs.is_empty() && std::time::Instant::now() < deadline {
            s.poll_persistence();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(s.transcription_jobs.is_empty());
        assert!(s.project.transcripts.is_empty());
    }
}

#[test]
fn generate_inspect_search_and_rename() {
    let (mut s, item, start) = session();
    assert!(s.execute("transcript.select", json!({"from": 0})).is_err(), "disabled before transcribing");
    let r = s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    assert_eq!(r["items"][0]["words"], 6, "{r}");
    assert_eq!(r["items"][0]["source"], "fixed");
    assert_eq!(s.project.transcripts[&item].words.len(), 6);

    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(words(&mut s), ["Hello", "um", "world.", "Second", "speaker", "here."]);
    assert_eq!(r["words"][0]["start"].as_i64().unwrap(), (start + Tick::from_seconds_f64(0.2)).0, "mapped to sequence time");
    assert_eq!(r["paragraphs"].as_array().unwrap().len(), 2, "speaker change splits paragraphs: {}", r["paragraphs"]);
    assert_eq!(r["speakers"], json!(["Speaker 1", "Speaker 2"]));

    let r = s.execute("transcript.search", json!({"query": "second spea"})).unwrap();
    assert_eq!(r["matches"], json!([{"from": 3, "to": 4, "start": r["matches"][0]["start"], "end": r["matches"][0]["end"]}]));

    s.execute("transcript.renameSpeaker", json!({"speaker": "Speaker 2", "name": "Ann"})).unwrap();
    let r = s.execute("transcript.inspect", json!({})).unwrap();
    assert_eq!(r["words"][3]["speaker"], "Ann");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[1].name, "Speaker 2");
    s.execute("transcript.renameSpeaker", json!({"speaker": 0, "item": item.0, "name": "Bo"})).unwrap();
    assert_eq!(s.project.transcripts[&item].speakers[0].name, "Bo");
    assert!(s.execute("transcript.renameSpeaker", json!({"speaker": "Nobody", "name": "X"})).is_err());

    // transcribing is one undo step
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.transcripts.is_empty());
}

#[test]
fn select_extract_and_lift_by_words() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let rate = s.sequence_rate();
    let before = s.active_sequence().unwrap().duration();

    let r = s.execute("transcript.select", json!({"from": 3, "to": 5})).unwrap();
    let (a, b) = (Tick(r["start"].as_i64().unwrap()), Tick(r["end"].as_i64().unwrap()));
    let q = s.active_sequence().unwrap();
    assert_eq!(q.mark_in, Some(a));
    assert_eq!(q.mark_out, Some(b - rate.frame_duration()), "Out is the last frame inside");
    assert_eq!(rate.snap(a), a, "frame aligned");
    assert_eq!(s.playhead(), a);

    // Extract "um": the sequence gets shorter by the word's frames and the word is gone
    let r = s.execute("transcript.extract", json!({"from": 1})).unwrap();
    let cut = Tick(r["end"].as_i64().unwrap()) - Tick(r["start"].as_i64().unwrap());
    assert!(cut > Tick::ZERO);
    assert_eq!(s.active_sequence().unwrap().duration(), before - cut);
    assert!(!words(&mut s).contains(&"um".to_string()), "{:?}", words(&mut s));
    s.active_sequence().unwrap().check().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);

    // Lift leaves a gap: same duration, word gone
    s.execute("transcript.lift", json!({"from": 1, "to": 1})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), before);
    assert_eq!(words(&mut s), ["Hello", "world.", "Second", "speaker", "here."]);
    assert!(s.execute("transcript.extract", json!({"from": 99})).is_err());
}

#[test]
fn remove_fillers_pauses_and_create_captions() {
    let (mut s, item, _) = session();
    s.execute("transcript.generate", json!({"items": [item.0]})).unwrap();
    let before = s.active_sequence().unwrap().duration();
    let r = s.execute("transcript.removeFillers", json!({})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    assert!(!words(&mut s).contains(&"um".to_string()));
    let r = s.execute("transcript.removePauses", json!({"minSeconds": 1.0, "keepSeconds": 0.1})).unwrap();
    assert_eq!(r["removed"], 1, "{r}");
    let after = s.active_sequence().unwrap().duration();
    assert!(after < before - Tick::from_seconds_f64(1.0), "the pause and the filler are gone");
    assert_eq!(words(&mut s).len(), 5);
    s.active_sequence().unwrap().check().unwrap();

    let r = s.execute("transcript.createCaptions", json!({"maxChars": 32})).unwrap();
    assert_eq!(r["captions"], 2, "one caption per speaker: {r}");
    let q = s.active_sequence().unwrap();
    let tr = &q.caption_tracks[0];
    assert_eq!(tr.captions[0].text, "Hello world.");
    assert_eq!(tr.captions[1].speaker.as_deref(), Some("Speaker 2"));
    q.check().unwrap();
}

#[test]
fn set_delete_and_models() {
    let (mut s, item, _) = session();
    let t = json!({"language": "en", "words": [
        {"text": "b", "start": 2000, "end": 3000, "speaker": 1},
        {"text": "a", "start": 0, "end": 1000},
    ]});
    let r = s.execute("transcript.set", json!({"item": item.0, "transcript": t})).unwrap();
    assert_eq!(r["words"], 2);
    let tr = &s.project.transcripts[&item];
    assert_eq!(tr.words[0].text, "a", "normalized: sorted");
    assert_eq!(tr.speakers.len(), 2, "missing speaker labels added");
    assert_eq!(tr.source, "imported");
    assert!(s.execute("transcript.set", json!({"item": 999_999, "transcript": {}})).is_err());
    assert!(s.execute("transcript.set", json!({"item": item.0, "transcript": {"words": 3}})).is_err());

    // survives save/load
    let bytes = filmcraft_format::encode(&s.project, true);
    let back = filmcraft_format::decode(&bytes).unwrap();
    assert_eq!(back.project.transcripts[&item].words.len(), 2);

    s.execute("transcript.delete", json!({"items": [item.0]})).unwrap();
    assert!(s.project.transcripts.is_empty());
    assert!(s.execute("transcript.delete", json!({})).is_err());

    let m = s.execute("transcript.models", json!({})).unwrap();
    assert_eq!(m["available"], filmcraft_speech::available());
    assert!(m["models"].as_array().unwrap().iter().any(|x| x["id"] == "whisper-base"));
}

#[test]
fn generate_without_a_transcriber() {
    let (mut s, item, _) = session();
    s.transcriber = None;
    let e = s.execute("transcript.generate", json!({"items": [item.0], "model": "nope"})).unwrap_err().to_string();
    // a build without speech-to-text says that first: the command is disabled (#97)
    let why = if filmcraft_speech::available() { "unknown speech model" } else { "not available in this build" };
    assert!(e.contains(why), "{e}");
    if !filmcraft_speech::available() {
        assert!(!s.is_enabled("sequence.transcribe"), "Transcribe Sequence follows transcript.generate");
        let e = s.execute("transcript.generate", json!({"items": [item.0]})).unwrap_err().to_string();
        assert!(e.contains("whisper") && e.contains("not available"), "{e}");
        #[cfg(not(feature = "speech-download"))]
        assert!(s.execute("transcript.downloadModel", json!({})).unwrap_err().to_string().contains("not available"));
    }
    // defaults to the media of the open sequence's audio clips
    s.transcriber = Some(Arc::new(FixedTranscriber { transcript: Transcript::default(), id: "empty".into() }));
    assert!(s.is_enabled("sequence.transcribe"), "an installed recogniser enables Transcribe Sequence");
    let r = s.execute("transcript.generate", json!({})).unwrap();
    assert!(r["items"].as_array().unwrap().len() >= 2, "{r}");
}

#[test]
fn transcription_targets_selected_timeline_audio_before_project_selection() {
    let (mut s, _, _) = session();
    let q = s.active_sequence().unwrap();
    let audio = q.audio_tracks[0].items[0].clone();
    let video = q.video_tracks[0].items[0].clone();
    s.state.selection = vec![audio.id, video.id, audio.id];
    s.state.project_selection = vec![video.item];
    assert_eq!(crate::transcript::targets(&s, &json!({})), vec![audio.item]);
    // Explicit requests still override the timeline selection.
    assert_eq!(crate::transcript::targets(&s, &json!({"item":video.item.0})), vec![video.item]);
    s.state.selection = vec![video.id];
    assert!(crate::transcript::targets(&s, &json!({})).is_empty());
}

struct AudioReadProbe {
    inner: filmcraft_media::SharedSource,
    info: filmcraft_media::MediaInfo,
    reads: Arc<std::sync::Mutex<Vec<(usize, i64, usize)>>>,
}
impl filmcraft_media::MediaSource for AudioReadProbe {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        &self.info
    }
    fn video_frame(&self, _: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
        panic!("transcription must never request video");
    }
    fn audio(&self, _: i64, _: usize, _: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        panic!("transcription must explicitly select the audio stream");
    }
    fn audio_stream(&self, stream: usize, start: i64, count: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.reads.lock().unwrap().push((stream, start, count));
        self.inner.audio(start, count, rate)
    }
}

#[test]
fn trimmed_transcription_reads_only_selected_audio_and_preserves_other_words() {
    for command in ["transcript.start", "transcript.generate", "sequence.transcribe"] {
        let (mut s, item, _) = session();
        let clip = s.active_sequence().unwrap().audio_tracks[0].items[0].id;
        let source = s.source(item).unwrap();
        let reads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut info = source.info().clone();
        info.audio_streams.push(info.audio_streams[0].clone());
        s.media.insert(item, Arc::new(AudioReadProbe { inner: source, info, reads: reads.clone() }));
        s.edit_sequence("Trim audio", |q, _, _| {
            q.audio_tracks[0].items.retain(|it| it.id == clip);
            let it = q.find_item_mut(clip).unwrap().1;
            it.start = Tick::from_seconds_f64(7.0);
            it.source_in = Tick::from_seconds_f64(3.0);
            it.duration = Tick::from_seconds_f64(2.0);
            it.speed = 1.0;
            it.audio_stream = 1;
            Ok(())
        })
        .unwrap();
        let word = |text, a, b| Word::new(text, Tick::from_seconds_f64(a), Tick::from_seconds_f64(b));
        s.execute(
            "transcript.set",
            json!({"item":item.0,"transcript":Transcript {
                words: vec![word("before",1.0,1.5),word("replace",3.5,4.0),word("after",6.0,6.5)], ..Default::default()
            }}),
        )
        .unwrap();
        s.transcriber =
            Some(Arc::new(FixedTranscriber { transcript: Transcript { words: vec![word("trimmed", 0.25, 0.5)], ..Default::default() }, id: "probe".into() }));
        s.state.selection = vec![clip];
        let before = s.project.to_json();
        let undo = s.history.undo.len();
        let params = if command == "sequence.transcribe" { json!({"background":true,"wait":true,"track":"A1"}) } else { json!({"wait":true}) };
        s.execute(command, params).unwrap();
        assert_eq!(*reads.lock().unwrap(), vec![(1, 48000, 32000)]);
        let transcript = &s.project.transcripts[&item];
        assert_eq!(transcript.text(), "before trimmed after");
        assert_eq!(transcript.words[1].start, Tick::from_seconds_f64(3.25));
        let words = crate::transcript::sequence_words(&s);
        let word = words.iter().find(|w| w.clip == clip && w.text == "trimmed").unwrap();
        assert_eq!(word.start, Tick::from_seconds_f64(7.25));
        assert_eq!(s.history.undo.len(), undo + 1);
        s.undo();
        assert_eq!(s.project.to_json(), before);
        s.redo();
        assert_eq!(s.project.transcripts[&item].text(), "before trimmed after");
    }
}

#[test]
fn trimmed_transcription_rejects_hostile_clip_bounds_before_starting_jobs() {
    for (source_in, duration, speed) in
        [(Tick(-1), Tick(1), 1.0), (Tick::MAX, Tick(100), 1.0), (Tick::ZERO, Tick(1), f64::NAN), (Tick::ZERO, Tick::MAX, 2.0), (Tick::ZERO, Tick(1), 0.0)]
    {
        let (mut s, _, _) = session();
        let clip = s.active_sequence().unwrap().audio_tracks[0].items[0].id;
        let id = s.state.active_sequence.unwrap();
        let q = Arc::make_mut(&mut s.project).sequence_mut(id).unwrap();
        let it = q.find_item_mut(clip).unwrap().1;
        it.source_in = source_in;
        it.duration = duration;
        it.speed = speed;
        s.state.selection = vec![clip];
        let before = s.project.to_json();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("transcript.start", json!({"wait":true})))).unwrap().is_err());
        assert_eq!(s.project.to_json(), before);
        assert!(s.jobs.is_empty());
    }
    let (mut s, _, _) = session();
    for clips in [json!([]), json!([0]), json!([-1]), json!(["clip"]), json!([u64::MAX])] {
        assert!(s.execute("transcript.start", json!({"clips":clips,"wait":true})).is_err());
    }
}
