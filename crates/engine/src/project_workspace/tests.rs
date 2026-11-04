use super::*;
use crate::{FsServices, Services};
use std::sync::{Arc, atomic::AtomicU64};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("filmcraft-workspace-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn session(&self) -> Session {
        Session { prefs_path: Some(self.0.join("preferences.json")), ..Default::default() }
    }
    fn create(&self, session: &mut Session, name: &str) -> String {
        session.execute("file.createProject", json!({"name":name,"parentDirectory":self.0,"template":"empty"})).unwrap()["path"].as_str().unwrap().to_string()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn wav(path: &Path) {
    std::fs::write(path, filmcraft_media::wav::write_wav16(&[0.0; 480], 1, 48_000)).unwrap();
}

#[test]
fn create_saves_before_installing_and_rejects_collisions_without_changing_history() {
    let temp = Temp::new();
    let mut s = temp.session();
    s.project_closed = true;
    let path = temp.create(&mut s, "My Film");
    assert!(Path::new(&path).ends_with("My Film/My Film.fcproj"));
    assert!(!s.project_closed);
    assert!(!s.is_dirty());
    assert_eq!(filmcraft_format::decode(&std::fs::read(&path).unwrap()).unwrap().project.name, "My Film");
    s.execute("file.newBin", json!({"name":"Work"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    let (project, epoch, history) = (s.project.clone(), s.document_epoch, (s.history.undo.len(), s.history.redo.len()));
    assert!(s.execute("file.createProject", json!({"name":"My Film","parentDirectory":temp.0})).is_err());
    assert!(Arc::ptr_eq(&project, &s.project));
    assert_eq!(epoch, s.document_epoch);
    assert_eq!(history, (s.history.undo.len(), s.history.redo.len()));
}

#[test]
fn hostile_creation_parameters_never_panic_or_mutate() {
    let temp = Temp::new();
    let mut s = temp.session();
    let project = s.project.clone();
    for name in ["", ".", "..", "../escape", "a/b", "a\\b", "CON", "nul.txt", "COM1", "LPT9", " spaced", "tail.", "a:b", "\0"] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("file.createProject", json!({"name":name,"parentDirectory":temp.0}))));
        assert!(result.unwrap().is_err(), "{name:?}");
        assert!(Arc::ptr_eq(&project, &s.project));
    }
    for params in [
        json!({}),
        json!({"name":42,"parentDirectory":temp.0}),
        json!({"name":"test","parentDirectory":false}),
        json!({"name":"test","parentDirectory":temp.0,"template":[]}),
        json!({"name":"test","parentDirectory":temp.0,"template":"unknown"}),
        json!({"name":"test","parentDirectory":temp.0,"force":"true"}),
    ] {
        assert!(s.execute("file.createProject", params).is_err());
        assert!(Arc::ptr_eq(&project, &s.project));
    }
    assert!(!temp.0.join("test").exists());
}

struct DiskFull;
impl Services for DiskFull {
    fn read_file(&self, p: &str) -> std::io::Result<Vec<u8>> {
        FsServices.read_file(p)
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
    fn write_file_new(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
}
#[test]
fn failed_creation_and_file_save_preserve_the_project_name_and_history() {
    let temp = Temp::new();
    let mut s = Session::new(Arc::new(DiskFull));
    s.execute("file.newBin", json!({"name":"Keep me"})).unwrap();
    let project = s.project.clone();
    let history = s.history.undo.len();
    assert!(s.execute("file.createProject", json!({"name":"New","parentDirectory":temp.0,"force":true})).is_err());
    assert!(!temp.0.join("New").exists());
    assert!(s.execute("file.saveAs", json!({"path":temp.0.join("Rename.fcproj")})).is_err());
    assert_eq!(s.project.name, "Untitled");
    assert!(Arc::ptr_eq(&project, &s.project));
    assert_eq!(s.history.undo.len(), history);
    assert!(s.is_dirty());
}

#[test]
fn simultaneous_creation_never_overwrites_the_winner() {
    let temp = Temp::new();
    let params = json!({"name":"Race","parentDirectory":temp.0});
    let other = params.clone();
    let worker = std::thread::spawn(move || Session::default().execute("file.createProject", other));
    let result = Session::default().execute("file.createProject", params);
    let second = worker.join().unwrap();
    assert_ne!(result.is_ok(), second.is_ok());
    assert!(temp.0.join("Race/Race.fcproj").is_file());
}

#[test]
fn demo_is_explicit_and_saved_with_a_sequence() {
    let temp = Temp::new();
    let mut s = temp.session();
    s.execute("file.createProject", json!({"name":"Example","parentDirectory":temp.0,"template":"demo"})).unwrap();
    assert!(s.project.sequences().next().is_some());
    assert!(s.state.active_sequence.is_some());
    assert_eq!(s.project.name, "Example");
    assert!(!s.is_dirty());
}

#[test]
fn moving_project_folder_keeps_internal_media_and_external_references() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Original");
    let internal = Path::new(&path).parent().unwrap().join("recording.wav");
    let external = temp.0.join("external.wav");
    wav(&internal);
    wav(&external);
    s.execute("file.import", json!({"paths":[internal,external]})).unwrap();
    s.execute("file.save", json!({})).unwrap();
    let loaded = filmcraft_format::decode(&std::fs::read(&path).unwrap()).unwrap().project;
    let paths: Vec<_> = loaded
        .items
        .values()
        .filter_map(|i| i.as_media())
        .filter_map(|m| match &m.media {
            MediaRef::File { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert!(paths.iter().any(|p| p == "recording.wav"), "{paths:?}");
    assert!(paths.iter().any(|p| Path::new(p).is_absolute()));
    s.execute("file.closeProject", json!({})).unwrap();
    drop(s);
    std::fs::rename(Path::new(&path).parent().unwrap(), temp.0.join("Moved")).unwrap();
    let mut recipient = Session::default();
    recipient.execute("file.open", json!({"path":temp.0.join("Moved/Original.fcproj")})).unwrap();
    assert!(recipient.offline.missing.is_empty(), "{:?}", recipient.offline.missing);
}

#[test]
fn folder_save_as_copies_internal_media_without_copying_external_media() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Original");
    let internal = Path::new(&path).parent().unwrap().join("voice.wav");
    let external = temp.0.join("nas-take.wav");
    wav(&internal);
    wav(&external);
    s.execute("file.import", json!({"paths":[internal,external]})).unwrap();
    s.execute("file.saveAsProject", json!({"name":"Copy","parentDirectory":temp.0,"wait":true})).unwrap();
    assert!(s.path.as_deref().is_some_and(|p| Path::new(p).ends_with("Copy/Copy.fcproj")));
    assert!(temp.0.join("Copy/voice.wav").is_file());
    assert!(!temp.0.join("Copy/nas-take.wav").exists());
    assert!(Path::new(&path).is_file());
    let mut recipient = Session::default();
    recipient.execute("file.open", json!({"path":s.path})).unwrap();
    assert!(recipient.offline.missing.is_empty());
}

#[test]
fn save_as_preserves_every_image_sequence_frame() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Frames");
    let root = Path::new(&path).parent().unwrap();
    for n in 1..=3 {
        image::RgbaImage::from_pixel(2, 2, image::Rgba([50, 60, 70, 255])).save(root.join(format!("frame{n:03}.png"))).unwrap();
    }
    s.execute("file.importImageSequence", json!({"path":root.join("frame001.png")})).unwrap();
    s.execute("file.saveAsProject", json!({"name":"Copied Frames","parentDirectory":temp.0,"wait":true})).unwrap();
    for n in 1..=3 {
        assert!(temp.0.join(format!("Copied Frames/frame{n:03}.png")).is_file());
    }
}

#[test]
fn save_as_publication_failure_keeps_current_project_and_reports_job_failure() {
    let temp = Temp::new();
    let mut s = temp.session();
    temp.create(&mut s, "Original");
    s.execute("file.newBin", json!({"name":"Unsaved"})).unwrap();
    let project = s.project.clone();
    let path = s.path.clone();
    s.services = Arc::new(DiskFull);
    assert!(s.execute("file.saveAsProject", json!({"name":"Failed","parentDirectory":temp.0,"wait":true})).is_err());
    assert!(Arc::ptr_eq(&project, &s.project));
    assert_eq!(path, s.path);
    assert!(s.is_dirty());
    assert!(!temp.0.join("Failed/Failed.fcproj").exists());
}

#[test]
fn recent_metadata_migrates_from_paths_and_corrupt_cache_is_optional() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Recent");
    library_settings(&mut s, &json!({"listView":true})).unwrap();
    let recent = recent_projects(&mut s, &json!({})).unwrap();
    assert_eq!(recent["projects"][0]["title"], "Recent");
    assert_eq!(recent["listView"], true);
    assert!(recent["projects"][0]["lastOpened"].is_i64());
    std::fs::write(temp.0.join("recent-projects.json"), b"not json").unwrap();
    let recent = recent_projects(&mut s, &json!({})).unwrap();
    assert_eq!(recent["projects"][0]["path"], path);
    remove_recent(&mut s, &json!({"path":path})).unwrap();
    assert!(s.prefs.general.recent_projects.is_empty());
    assert!(Path::new(&path).exists());
}

#[test]
fn local_access_approvals_are_scoped_to_project_and_not_embedded() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Local");
    let network = r"\\nas\approved\take.wav".to_string();
    let mut media = crate::demo::demo_project(&crate::MediaPool::default())
        .0
        .items
        .into_values()
        .find_map(|item| match item.kind {
            ItemKind::Media(media) => Some(media),
            _ => None,
        })
        .unwrap();
    media.media = MediaRef::File { path: network.clone() };
    Arc::make_mut(&mut s.project).add_item("NAS", filmcraft_project::Label::Iris, ItemKind::Media(media), None);
    let mut cache = load(&s);
    cache.grants.entry(key(&path)).or_default().insert(key(&network));
    persist(&s, &mut cache).unwrap();
    s.media.add_stored_paths([network.clone()], Some(&path));
    restore_access(&s);
    assert_eq!(s.media.follow(&network), Ok(network.clone()));
    s.path = Some(temp.0.join("Other.fcproj").to_string_lossy().into_owned());
    s.media = Arc::new(crate::MediaPool::default());
    s.media.add_stored_paths([network.clone()], s.path.as_deref());
    restore_access(&s);
    assert!(s.media.follow(&network).is_err());
    let bytes = std::fs::read(path).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("approved"));
}

#[test]
fn recovery_references_remain_anchored_to_original_document() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Recovery");
    let file = Path::new(&path).parent().unwrap().join("voice.wav");
    wav(&file);
    s.execute("file.import", json!({"paths":[file]})).unwrap();
    let project = for_document(&s.project, Some(&path), &path);
    let autosave = temp.0.join("Recovery/FilmCraft Auto-Save/a.fcproj");
    let copy = for_document(&project, Some(&path), &autosave.to_string_lossy());
    let media = copy.items.values().find_map(|i| i.as_media()).unwrap();
    let MediaRef::File { path: stored } = &media.media else { panic!() };
    assert_eq!(std::fs::canonicalize(crate::doc_path::resolve(stored, Some(&autosave)).unwrap()).unwrap(), std::fs::canonicalize(file).unwrap());
}

#[test]
fn cancelled_copy_and_stale_copy_results_never_replace_the_session() {
    let temp = Temp::new();
    let mut s = temp.session();
    temp.create(&mut s, "Original");
    let (sender, receiver) = mpsc::channel();
    let id = 1;
    s.workspace_save = Some(PendingSave {
        job: id,
        source_path: s.path.clone(),
        epoch: s.document_epoch,
        revision: s.revision,
        view: s.project_view(),
        receiver,
        grants: Default::default(),
    });
    sender.send(Err("cancelled".into())).unwrap();
    let before = s.project.clone();
    poll(&mut s);
    assert!(Arc::ptr_eq(&before, &s.project));
    let (sender, receiver) = mpsc::channel();
    s.workspace_save = Some(PendingSave {
        job: id,
        source_path: s.path.clone(),
        epoch: s.document_epoch,
        revision: s.revision,
        view: s.project_view(),
        receiver,
        grants: Default::default(),
    });
    s.execute("file.newBin", json!({"name":"Newer edits"})).unwrap();
    let before = s.project.clone();
    sender.send(Ok((Project::new("Older snapshot"), "snapshot.fcproj".into()))).unwrap();
    poll(&mut s);
    assert!(Arc::ptr_eq(&before, &s.project));
    assert!(s.is_dirty());
}

#[test]
fn stale_thumbnail_results_and_corrupt_cache_do_not_change_project_save_success() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Thumbnail");
    let (sender, receiver) = mpsc::channel();
    s.workspace_thumbnail = Some(Thumbnail { epoch: s.document_epoch, generation: s.workspace_save_generation, path: path.clone(), receiver });
    sender.send(Ok(vec![1, 2, 3])).unwrap();
    s.document_epoch = s.document_epoch.wrapping_add(1);
    poll(&mut s);
    assert!(!temp.0.join("Thumbnail/.filmcraft/thumbnail.png").exists());
    assert!(Path::new(&path).is_file());
    assert!(!s.is_dirty());
}

#[test]
fn explicit_demo_save_produces_a_bounded_thumbnail() {
    let temp = Temp::new();
    let mut s = temp.session();
    s.execute("file.createProject", json!({"name":"Demo","parentDirectory":temp.0,"template":"demo"})).unwrap();
    let start = std::time::Instant::now();
    while s.workspace_thumbnail.is_some() && start.elapsed() < std::time::Duration::from_secs(20) {
        poll(&mut s);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let path = temp.0.join("Demo/.filmcraft/thumbnail.png");
    let image = image::open(path).unwrap();
    assert!(image.width() <= 320 && image.height() <= 180);
    assert!(!s.is_dirty());
}

#[test]
fn generated_recordings_narrations_and_extracted_audio_use_project_subdirectories() {
    let temp = Temp::new();
    let mut s = temp.session();
    let project = temp.create(&mut s, "Working Files");
    let root = Path::new(&project).parent().unwrap();
    assert_eq!(Path::new(&crate::voiceover::media_dir(&s, &json!({}), "Narrations")), root.join("Narrations"));
    assert_eq!(Path::new(&crate::voiceover::media_dir(&s, &json!({}), "Voice-over Recordings")), root.join("Voice-over Recordings"));
    let source = temp.0.join("source.wav");
    wav(&source);
    let imported = s.execute("file.import", json!({"paths":[source]})).unwrap();
    let id = imported["items"][0].as_u64().unwrap();
    s.state.project_selection = vec![filmcraft_project::ItemId(id)];
    let extracted = s.execute("clip.extractAudio", json!({"items":[id]})).unwrap();
    let path = extracted["extracted"][0]["path"].as_str().unwrap();
    assert_eq!(Path::new(path).parent().unwrap(), root.join("Extracted Audio"));
    assert!(Path::new(path).is_file());
}

#[test]
fn hostile_file_timestamps_do_not_panic() {
    assert_eq!(unix_timestamp(std::time::UNIX_EPOCH), Some(0));
    if let Some(time) = std::time::UNIX_EPOCH.checked_sub(std::time::Duration::from_millis(1)) {
        assert_eq!(unix_timestamp(time), Some(-1));
    }
    if let Some(time) = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(i64::MAX as u64)) {
        assert_eq!(unix_timestamp(time), None);
    }
}

#[test]
fn configured_scratch_override_survives_save_and_reopen() {
    let temp = Temp::new();
    let mut s = temp.session();
    let path = temp.create(&mut s, "Scratch");
    let external = temp.0.join("External Scratch");
    std::fs::create_dir(&external).unwrap();
    let chosen = external.to_string_lossy().replace('\\', "/");
    Arc::make_mut(&mut s.project).settings.scratch.captured = Some(chosen.clone());
    s.prefs.auto_save.scratch_folders.push(chosen);
    s.execute("file.save", json!({})).unwrap();
    s.execute("file.open", json!({"path":path})).unwrap();
    assert_eq!(Path::new(&crate::voiceover::media_dir(&s, &json!({}), "Narrations")), external);
}
