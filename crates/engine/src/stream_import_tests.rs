//! Recording import acceptance. Media is generated locally, never committed.
use crate::Session;
use filmcraft_project::ItemId;
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn fixture(extension: &str) -> PathBuf {
    let ff = filmcraft_testkit::oracle::ffmpeg_or_skip("stream import").expect("ffmpeg required for recording acceptance");
    let dir = filmcraft_testkit::fixtures_dir("engine/streams");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("four-six.{extension}"));
    let mut cmd = Command::new(ff);
    cmd.args(["-y", "-v", "error"]);
    for (i, color) in ["red", "green", "blue", "yellow"].iter().enumerate() {
        cmd.args(["-itsoffset", if i < 2 { "0.070" } else { "0.103" }, "-f", "lavfi", "-i"]);
        cmd.arg(format!("color=c={color}:s={}:r=30:d=2", if i == 3 { "48x64" } else { "64x48" }));
    }
    for i in 0..6 {
        cmd.args(["-f", "lavfi", "-i"]).arg(format!("aevalsrc=0.08*sin(2*PI*{}*t)|0.06*sin(2*PI*{}*t):s=48000:d=2.2", 300 + i * 130, 371 + i * 130));
    }
    for i in 0..10 {
        cmd.arg("-map").arg(format!("{i}:0"));
    }
    cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2", "-fps_mode", "passthrough", "-c:a", "aac", "-ac", "2"]);
    for (i, name) in ["Main Output", "Game", "Model", "Vertical", "VOD", "Main", "Mic", "Chat", "Music", "System"].iter().enumerate() {
        let key = if extension == "mp4" { "handler_name" } else { "title" };
        cmd.arg(format!("-metadata:s:{i}")).arg(format!("{key}={name}"));
    }
    let result = cmd.arg(&path).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    path
}

fn ids(catalog: &Value) -> Value {
    Value::Array(catalog["streams"].as_array().unwrap().iter().map(|s| s["id"].clone()).collect())
}

#[test]
fn exhausted_ordinary_import_and_track_commands_do_not_panic() {
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
    let bytes: std::sync::Arc<[u8]> = encoded.into_inner().into();
    let mut s = Session::default();
    let item = crate::commands::import_bytes(&mut s, "identity-test.png", bytes.clone(), None).unwrap();
    s.execute("file.newSequence", json!({"video":1,"audio":1})).unwrap();
    s.state.project_selection = vec![item];
    std::sync::Arc::make_mut(&mut s.project).next_id = u64::MAX;
    let before = s.project.to_json();
    let history = (s.history.undo.len(), s.history.redo.len());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::commands::import_bytes(&mut s, "identity-test.png", bytes, None))).unwrap().is_err()
    );
    for (command, params) in [
        ("file.newSequence", json!({"video":1,"audio":1})),
        ("file.newSequenceFromClip", json!({"items":[item.0]})),
        ("sequence.addTracks", json!({"video":1,"audio":1})),
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, params))).unwrap().is_err(), "{command}");
        assert_eq!(s.project.to_json(), before);
        assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
    }
}

fn exercise(path: &Path, actual: bool) {
    let mut s = Session::default();
    let catalog = s.execute("media.inspectStreams", json!({"path":path})).unwrap();
    let one = s.media.recording(&path.to_string_lossy(), &*s.services).unwrap();
    let two = s.media.recording(&path.to_string_lossy(), &*s.services).unwrap();
    assert!(std::sync::Arc::ptr_eq(&one, &two));
    assert_eq!(catalog["streams"].as_array().unwrap().len(), 10, "{catalog}");
    assert!(catalog["streams"].as_array().unwrap().iter().all(|s| s["unsupported"].is_null()), "{catalog}");
    if actual {
        for (i, ms) in [70, 70, 103, 103, 0, 0, 0, 0, 0, 0].into_iter().enumerate() {
            assert_eq!(catalog["streams"][i]["start"], json!(Tick::from_rational(ms, 1, 1000)), "{catalog}");
        }
        assert_eq!(catalog["streams"][3]["video"]["width"], 1080);
    }
    // A subset keeps the whole recording's origin; portrait-only import chooses portrait settings.
    let mut subset = Session::default();
    subset.execute("file.importStreams", json!({"path":path,"streams":[catalog["streams"][3]["id"]],"at":0})).unwrap();
    let q = subset.active_sequence().unwrap();
    assert_eq!(q.video_tracks[0].items[0].start.0, catalog["streams"][3]["start"].as_i64().unwrap() - catalog["origin"].as_i64().unwrap());
    assert!(q.settings.height > q.settings.width);
    let before = s.project.clone();
    let imported = s.execute("file.importStreams", json!({"path":path,"streams":ids(&catalog),"at":0})).unwrap();
    let original = s.project.clone();
    assert_eq!(s.active_sequence().unwrap().video_tracks.len(), 4);
    assert_eq!(s.active_sequence().unwrap().audio_tracks.len(), 6);
    for clip in s.active_sequence().unwrap().video_tracks.iter().flat_map(|t| &t.items) {
        for param in clip.effect("motion").unwrap().params.values() {
            if let filmcraft_project::ParamValue::Vec2(v) = &param.value {
                assert!(v.x.is_finite() && v.y.is_finite());
            }
        }
    }
    assert!(
        s.active_sequence().unwrap().video_tracks.iter().chain(&s.active_sequence().unwrap().audio_tracks).flat_map(|t| &t.items).all(|c| c.link.is_none())
    );
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.to_json(), before.to_json());
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.project.to_json(), original.to_json());
    let dir = filmcraft_testkit::fixtures_dir(&format!("engine/streams/acceptance-{}-{}", path.extension().unwrap().to_string_lossy(), actual));
    std::fs::create_dir_all(&dir).unwrap();
    let saved = dir.join("recording.fcproj");
    s.execute("file.saveAs", json!({"path":saved})).unwrap();
    let mut reopened = Session::default();
    reopened.execute("file.open", json!({"path":saved})).unwrap();
    assert_eq!(reopened.project.to_json(), s.project.to_json());
    // Force independent sources to reopen rather than relying on the import-time cache.
    let project = reopened.project.clone();
    verify_views(&reopened, &imported, actual);
    // Each clip edits independently; exact undo/redo keeps its stream identity.
    let clip = imported["clips"][2].as_u64().unwrap();
    let (track, moving) = reopened.active_sequence().unwrap().find_item(filmcraft_project::ClipId(clip)).unwrap();
    let target = moving.start + reopened.sequence_rate().frame_duration();
    reopened.execute("timeline.move", json!({"moves":[{"clip":clip,"track":track.0,"time":target.0}],"linked":false})).unwrap();
    reopened.execute("edit.undo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), project.to_json());

    reopened.execute("timeline.trim", json!({"clip":clip,"edge":"out","deltaFrames":-3,"linked":false})).unwrap();
    let trimmed = reopened.project.clone();
    reopened.execute("edit.undo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), project.to_json());
    reopened.execute("edit.redo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), trimmed.to_json());
    reopened.state.selection = vec![filmcraft_project::ClipId(clip)];
    reopened.execute("sequence.addEdit", json!({"time":TICKS_PER_SECOND})).unwrap();
    reopened.execute("file.saveAs", json!({"path":saved})).unwrap();
    let mut edited = Session::default();
    edited.execute("file.open", json!({"path":saved})).unwrap();
    assert_eq!(edited.project.to_json(), reopened.project.to_json());
    verify_exports(&mut edited, &dir, &imported, actual);
    if actual {
        verify_original_recording_exports(path, &dir);
    }
    verify_original_recording_audio(path, &dir, actual);
    // Export uses its own selected-stream decoder instances, including all six audio tracks.
    let out = dir.join("edited.mov");
    let result = edited
        .execute(
            "file.exportMedia",
            json!({"path":out,"format":"proRes","width":64,"height":48,"range":"custom","startSeconds":0,"endSeconds":0.3,"wait":true}),
        )
        .unwrap();
    assert!(out.exists(), "{result}");
    assert!(result.to_string().contains("offlineMedia"), "{result}");
    assert!(!result.to_string().contains("unreadable"), "{result}");
    verify_rejections(&mut edited, path, &catalog, &imported);
    exercise_ordinary_import(path, actual);
}

fn exercise_ordinary_import(path: &Path, actual: bool) {
    let dir = filmcraft_testkit::fixtures_dir(&format!("engine/streams/ordinary-{}-{actual}", path.extension().unwrap().to_string_lossy()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Session::default();
    let ingest = &mut std::sync::Arc::make_mut(&mut s.project).settings.ingest;
    ingest.enabled = true;
    ingest.action = filmcraft_project::IngestAction::CopyAndCreateProxies;
    ingest.destination = Some(dir.join("must-not-copy").to_string_lossy().into());
    let imported = s.execute("file.import", json!({"paths":[path]})).unwrap();
    assert_eq!(imported["items"].as_array().unwrap().len(), 1);
    let item = imported["items"][0].as_u64().unwrap();
    let m = s.project.item(ItemId(item)).unwrap().as_media().unwrap();
    assert_eq!(m.recording.as_ref().unwrap().streams.len(), 10);
    assert_eq!(m.info.audio_streams.len(), 6);
    assert!(s.active_sequence().is_none());
    assert_eq!(s.project.root.children.len(), 1);
    assert!(!dir.join("must-not-copy").exists());
    let saved = dir.join("project-item.fcproj");
    s.execute("file.saveAs", json!({"path":saved})).unwrap();
    let mut reopened = Session::default();
    reopened.execute("file.open", json!({"path":saved})).unwrap();
    let before = reopened.project.to_json();
    reopened.execute("file.newSequence", json!({"fromItem":item,"video":1,"audio":1})).unwrap();
    let sequence = reopened.state.active_sequence.unwrap();
    let q = reopened.active_sequence().unwrap();
    assert_eq!((q.video_tracks.len(), q.audio_tracks.len()), (4, 6));
    let catalog = reopened.project.item(ItemId(item)).unwrap().as_media().unwrap().recording.as_ref().unwrap();
    for (track, stream) in q.video_tracks.iter().chain(&q.audio_tracks).zip(&catalog.streams) {
        let clip = &track.items[0];
        assert_eq!(clip.start, stream.start - catalog.origin);
        assert_eq!(clip.duration, stream.end - stream.start);
        assert_eq!(clip.source_in, Tick::ZERO);
        let source = reopened.project.item(clip.item).unwrap().as_media().unwrap();
        assert_eq!(source.stream.as_ref(), Some(&stream.id));
    }
    let clips: Vec<_> = q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.items[0].id.0).collect();
    let items: Vec<_> = q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.items[0].item.0).collect();
    let placed = json!({"clips":clips,"items":items});
    assert_eq!(reopened.project.root.children.len(), 2, "recording and sequence, without expanded stream items in the Project bin");
    assert_eq!(reopened.placement_lanes(ItemId(item), filmcraft_time::TimeRange::new(Tick::ZERO, q.duration()), true, true), (4, 6));
    let after = reopened.project.to_json();
    reopened.execute("edit.undo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), before);
    reopened.execute("edit.redo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), after);
    // Undo closes the removed sequence tab; reopening the restored sequence is UI state.
    reopened.state.active_sequence = Some(sequence);
    reopened.state.open_sequences = vec![sequence];
    // A second drop reuses stream references and gives every timeline clip a new identity.
    let count = reopened.project.items.len();
    let again = reopened.execute("timeline.place", json!({"item":item,"seconds":40,"track":"V1","audioTrack":"A1"})).unwrap();
    assert_eq!(again["clips"].as_array().unwrap().len(), 10);
    assert_eq!(reopened.project.items.len(), count);
    reopened.execute("edit.undo", json!({})).unwrap();
    assert_eq!(reopened.project.to_json(), after);
    verify_views(&reopened, &placed, actual);
    verify_recording_drop_rollback(&mut reopened, item);
    let clip = placed["clips"][2].as_u64().unwrap();
    reopened.execute("timeline.trim", json!({"clip":clip,"edge":"out","deltaFrames":-3,"linked":false})).unwrap();
    reopened.state.selection = vec![filmcraft_project::ClipId(clip)];
    reopened.execute("sequence.addEdit", json!({"time":TICKS_PER_SECOND})).unwrap();
    reopened.execute("file.saveAs", json!({"path":saved})).unwrap();
    let mut edited = Session::default();
    edited.execute("file.open", json!({"path":saved})).unwrap();
    assert_eq!(edited.project.to_json(), reopened.project.to_json());
    verify_exports(&mut edited, &dir, &placed, actual);
    verify_original_recording_audio(path, &dir, actual);
    if actual {
        verify_original_recording_exports(path, &dir);
    }
}

fn verify_recording_drop_rollback(s: &mut Session, item: u64) {
    let project = s.project.clone();
    let seq = s.state.active_sequence.unwrap();
    std::sync::Arc::make_mut(&mut s.project).sequence_mut(seq).unwrap().audio_tracks[5].locked = true;
    let before = s.project.to_json();
    let history = (s.history.undo.len(), s.history.redo.len());
    assert!(s.execute("timeline.place", json!({"item":item,"seconds":40,"track":"V1","audioTrack":"A1"})).is_err());
    assert_eq!(s.project.to_json(), before);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
    s.project = project;
}

#[test]
fn recording_streams_mp4_end_to_end() {
    exercise(&fixture("mp4"), false);
}
#[test]
fn recording_streams_mkv_end_to_end() {
    exercise(&fixture("mkv"), false);
}

#[test]
#[ignore = "set FILMCRAFT_OBS_SAMPLE to the local four-video/six-audio recording"]
fn recording_streams_actual_obs_acceptance() {
    exercise(Path::new(&std::env::var("FILMCRAFT_OBS_SAMPLE").expect("set FILMCRAFT_OBS_SAMPLE")), true);
}

// Compare independently decoded original media with the edited export. Preview/export
// agreement alone cannot catch a shared timestamp or decoder-selection mistake.
fn verify_original_recording_exports(path: &Path, dir: &Path) {
    let ff = filmcraft_testkit::oracle::ffmpeg_or_skip("original recording comparison").unwrap();
    let probe = filmcraft_testkit::oracle::ffprobe_or_skip("original recording timestamps").unwrap();
    for stream in 0..4 {
        let packets = Command::new(&probe)
            .args(["-v", "error", "-select_streams"])
            .arg(format!("v:{stream}"))
            .args(["-show_packets", "-show_entries", "packet=pts_time", "-of", "json"])
            .arg(path)
            .output()
            .unwrap();
        assert!(packets.status.success());
        let packets: Value = serde_json::from_slice(&packets.stdout).unwrap();
        for base in [0, 16, 30] {
            // FilmCraft holds the preceding frame. FFmpeg's output -ss instead selects
            // the following frame, so explicitly choose the original packet's PTS.
            let target = f64::from(base) + 0.2;
            let pts = packets["packets"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|p| p["pts_time"].as_str()?.parse::<f64>().ok())
                .filter(|&t| t <= target)
                .max_by(f64::total_cmp)
                .unwrap();
            let reference = Command::new(&ff)
                .args(["-v", "error", "-i"])
                .arg(path)
                .arg("-map")
                .arg(format!("0:{stream}"))
                .arg("-ss")
                .arg((pts - 0.000001).to_string())
                .args(["-frames:v", "1", "-vf", "crop=64:48", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .output()
                .unwrap();
            let exported = Command::new(&ff)
                .args(["-v", "error", "-i"])
                .arg(dir.join(format!("stream-{stream}-{base}.mov")))
                .args(["-ss", "0.2", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .output()
                .unwrap();
            assert!(reference.status.success() && exported.status.success());
            assert_eq!(reference.stdout.len(), 64 * 48 * 3);
            assert_eq!(reference.stdout.len(), exported.stdout.len());
            let mse = reference.stdout.iter().zip(&exported.stdout).map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2)).sum::<f64>()
                / reference.stdout.len() as f64;
            assert!(mse.sqrt() < 12.0, "original video {stream} at {target}: RMSE {}", mse.sqrt());
        }
    }
}

fn verify_original_recording_audio(path: &Path, dir: &Path, actual: bool) {
    let ff = filmcraft_testkit::oracle::ffmpeg_or_skip("original recording audio").unwrap();
    for stream in 4..10 {
        for &base in if actual { &[0, 16, 30][..] } else { &[0][..] } {
            let reference = Command::new(&ff)
                .args(["-v", "error", "-i"])
                .arg(path)
                .arg("-map")
                .arg(format!("0:{stream}"))
                .arg("-ss")
                .arg(base.to_string())
                // Decode beyond the comparison window: FFmpeg's duration cutoff can
                // round a short AAC packet window down by a few output samples.
                .args(["-t", "0.31", "-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"])
                .output()
                .unwrap();
            let exported = Command::new(&ff)
                .args(["-v", "error", "-i"])
                .arg(dir.join(format!("stream-{stream}-{base}.wav")))
                .args(["-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"])
                .output()
                .unwrap();
            assert!(reference.status.success() && exported.status.success());
            verify_original_audio(&reference.stdout, &exported.stdout);
        }
    }
}

fn verify_original_audio(reference: &[u8], exported: &[u8]) {
    let floats = |bytes: &[u8]| bytes.chunks_exact(4).map(|v| f32::from_le_bytes(v.try_into().unwrap())).collect::<Vec<_>>();
    let reference = floats(reference);
    let exported = floats(exported);
    assert!(reference.len() >= 14_400 * 2);
    let reference = &reference[..14_400 * 2];
    assert_eq!(reference.len(), exported.len());
    let error_at = |shift: i32| {
        let offset = shift.unsigned_abs() as usize * 2;
        let (a, b) = if shift >= 0 {
            (&reference[offset..], &exported[..exported.len() - offset])
        } else {
            (&reference[..reference.len() - offset], &exported[offset..])
        };
        a.iter().zip(b).map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2)).sum::<f64>() / a.len() as f64
    };
    // Different AAC decoders and 16-bit export quantization need an amplitude tolerance.
    assert!(error_at(0).sqrt() < 0.0002);
    let energy = reference.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / reference.len() as f64;
    if energy > 0.000001 {
        let best = (-4..=4).min_by(|&a, &b| error_at(a).total_cmp(&error_at(b))).unwrap();
        assert_eq!(best, 0, "original recording audio is shifted by {best} output samples");
    }
}

fn verify_exports(s: &mut Session, dir: &Path, imported: &Value, actual: bool) {
    let ff = filmcraft_testkit::oracle::ffmpeg_or_skip("stream export").unwrap();
    let original = s.project.clone();
    let seq = s.state.active_sequence.unwrap();
    // Export each original stream independently. No other track may hide a wrong selection.
    for &base in if actual { &[0i64, 16, 30][..] } else { &[0i64][..] } {
        for i in 0..10 {
            let mut project = (*original).clone();
            let q = project.sequence_mut(seq).unwrap();
            q.settings.width = 64;
            q.settings.height = 48;
            for (k, t) in q.video_tracks.iter_mut().enumerate() {
                t.enabled = i == k;
                for clip in &mut t.items {
                    let m = clip.effect_mut("motion").unwrap();
                    m.param_mut("position").unwrap().value = filmcraft_project::ParamValue::Vec2(filmcraft_geom::Vec2::new(32.0, 24.0));
                }
            }
            for (k, t) in q.audio_tracks.iter_mut().enumerate() {
                t.muted = i != k + 4;
            }
            s.project = std::sync::Arc::new(project);
            let extension = if i < 4 { "mov" } else { "wav" };
            let out = dir.join(format!("stream-{i}-{base}.{extension}"));
            let result = s
            .execute(
                "file.exportMedia",
                json!({"path":out,"format":if i<4 { "proRes" } else { "wav" },"range":"custom","startSeconds":base,"endSeconds":base as f64+0.3,"wait":true}),
            )
            .unwrap();
            assert!(result["offlineMedia"].as_array().is_some_and(|a| a.is_empty()), "{result}");
            let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
            if i < 4 {
                let expected =
                    filmcraft_render::render_sequence_rgba8(&s.project, seq, Tick::from_rational(base * 5 + 1, 1, 5), Default::default(), &provider).2;
                let decoded = Command::new(&ff)
                    .args(["-v", "error", "-i"])
                    .arg(&out)
                    .args(["-ss", "0.2", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
                    .output()
                    .unwrap();
                assert!(decoded.status.success(), "{}", String::from_utf8_lossy(&decoded.stderr));
                assert_eq!(expected.len(), decoded.stdout.len());
                let mse = expected
                    .iter()
                    .zip(&decoded.stdout)
                    .enumerate()
                    .filter(|(i, _)| i % 4 != 3)
                    .map(|(_, (&a, &b))| (f64::from(a) - f64::from(b)).powi(2))
                    .sum::<f64>()
                    / (expected.len() / 4 * 3) as f64;
                assert!(mse.sqrt() < 12.0, "stream {i}: export vs preview RMSE {}", mse.sqrt());
            } else {
                let q = s.active_sequence().unwrap();
                let expected = filmcraft_render::audio::mix_sequence(&s.project, q, base * 48_000, 14_400, &provider);
                let decoded = Command::new(&ff).args(["-v", "error", "-i"]).arg(&out).args(["-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"]).output().unwrap();
                assert!(decoded.status.success());
                let actual: Vec<f32> = decoded.stdout.chunks_exact(4).map(|v| f32::from_le_bytes(v.try_into().unwrap())).collect();
                let wanted = expected.interleaved();
                assert_eq!(wanted.len(), actual.len());
                let max = wanted.iter().zip(&actual).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
                assert!(max < 0.0001, "stream {i}: sample-aligned WAV differs by {max}");
            }
        }
    }
    // Four visible views, each derived from its own selected source, with only the Mic mix audible.
    let mut project = (*original).clone();
    let sizes: Vec<_> = imported["items"]
        .as_array()
        .unwrap()
        .iter()
        .take(4)
        .map(|v| {
            let m = project.item(ItemId(v.as_u64().unwrap())).unwrap().as_media().unwrap();
            let v = m.info.video.as_ref().unwrap();
            (v.width, v.height)
        })
        .collect();
    let q = project.sequence_mut(seq).unwrap();
    q.settings.width = 128;
    q.settings.height = 96;
    for (i, t) in q.video_tracks.iter_mut().enumerate() {
        t.enabled = true;
        for c in &mut t.items {
            let m = c.effect_mut("motion").unwrap();
            m.param_mut("scale").unwrap().value =
                filmcraft_project::ParamValue::Float((64.0 / f64::from(sizes[i].0)).min(48.0 / f64::from(sizes[i].1)) * 100.0);
            m.param_mut("position").unwrap().value =
                filmcraft_project::ParamValue::Vec2(filmcraft_geom::Vec2::new(32.0 + (i % 2) as f64 * 64.0, 24.0 + (i / 2) as f64 * 48.0));
        }
    }
    for (i, t) in q.audio_tracks.iter_mut().enumerate() {
        t.muted = i != 2;
    }
    s.project = std::sync::Arc::new(project);
    let out = dir.join("four-view.mov");
    s.execute("file.exportMedia", json!({"path":out,"format":"proRes","range":"custom","startSeconds":0,"endSeconds":0.3,"wait":true})).unwrap();
    assert!(out.exists());
    s.project = original;
}

fn verify_views(reopened: &Session, imported: &Value, actual: bool) {
    let project = reopened.project.clone();
    let mut proxy_project = (*project).clone();
    for m in proxy_project.items.values_mut().filter_map(|i| i.as_media_mut()) {
        m.proxy = Some(m.media.clone());
    }
    reopened.media.set_use_proxies(true);
    let mut samples = Vec::new();
    for (index, value) in imported["items"].as_array().unwrap().iter().enumerate() {
        let item = ItemId(value.as_u64().unwrap());
        let info = project.item(item).unwrap().as_media().unwrap();
        assert!(info.stream.is_some());
        let src = reopened.media.full_res_source(&project, item, &*reopened.services).unwrap();
        let own = reopened.media.own_source(&project, item, &*reopened.services).unwrap();
        let proxy = reopened.media.source_for(&proxy_project, item, &*reopened.services).unwrap();
        assert!(std::sync::Arc::ptr_eq(&src, &proxy), "unmapped proxy must not replace a selected stream");
        if info.info.has_video() {
            for time in [1.0, 0.1, if actual { 30.0 } else { 1.5 }, 0.3] {
                let req = filmcraft_media::FrameRequest::full(Tick::from_seconds_f64(time));
                let a = src.video_frame(req).unwrap();
                let b = own.video_frame(req).unwrap();
                assert_eq!((a.width, a.height), (b.width, b.height));
                assert_eq!(a.to_rgba8(), b.to_rgba8());
                if !actual {
                    let expected = [[255i16, 0, 0], [0, 128, 0], [0, 0, 255], [255, 255, 0]][index];
                    let pixel = a.to_rgba8();
                    let center = ((a.height / 2 * a.width + a.width / 2) * 4) as usize;
                    for c in 0..3 {
                        assert!((i16::from(pixel[center + c]) - expected[c]).abs() < 12, "wrong picture from stream {index}");
                    }
                }
            }
        } else {
            let a = src.audio(48_000, 1024, 48_000).unwrap();
            let b = own.audio(48_000, 1024, 48_000).unwrap();
            assert_eq!(a.channels, b.channels);
            samples.push(a.channels);
        }
    }
    for value in imported["items"].as_array().unwrap() {
        assert!(reopened.media.offline_status(ItemId(value.as_u64().unwrap())).is_none());
    }
    if !actual {
        for pair in samples.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
    }
}

fn verify_rejections(edited: &mut Session, path: &Path, catalog: &Value, imported: &Value) {
    let project = edited.project.clone();
    // Reject hostile selection and time without changing project, undo or redo.
    edited.execute("file.newSequence", json!({"name":"Temporary"})).unwrap();
    edited.execute("edit.undo", json!({})).unwrap();
    for params in [
        json!({"streams":[]}),
        json!({"streams":[catalog["streams"][0]["id"],catalog["streams"][0]["id"]]}),
        json!({"streams":[{"container":"mp4","track":4294967295u32}]}),
        json!({"streams":ids(catalog),"at":i64::MAX}),
    ] {
        let project = edited.project.clone();
        let undo = edited.history.undo.len();
        let redo = edited.history.redo.len();
        let mut params = params;
        params["path"] = json!(path);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| edited.execute("file.importStreams", params))).unwrap().is_err());
        assert_eq!(edited.project.to_json(), project.to_json());
        assert_eq!(edited.history.undo.len(), undo);
        assert_eq!(edited.history.redo.len(), redo);
    }
    let mut exhausted = (*edited.project).clone();
    exhausted.next_id = u64::MAX;
    edited.project = std::sync::Arc::new(exhausted);
    let before = edited.project.to_json();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| edited.execute("file.importStreams", json!({"path":path,"streams":ids(catalog)}))));
    assert!(result.unwrap().is_err());
    assert_eq!(edited.project.to_json(), before);
    // A missing explicit identity must never become video/audio stream zero.
    let item = ItemId(imported["items"][0].as_u64().unwrap());
    let mut bad = (*project).clone();
    bad.item_mut(item).unwrap().as_media_mut().unwrap().stream = Some(filmcraft_media::streams::StreamId::Mp4 { track: u32::MAX });
    let pool = crate::media_pool::MediaPool::default();
    pool.full_res_source(&bad, item, &*edited.services).unwrap();
    assert!(pool.offline_status(item).is_some());
}
