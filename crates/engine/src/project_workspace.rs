//! Desktop project folders, recent-project metadata and local media approvals.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use crate::{EngineError, Result, Session};
use filmcraft_project::{ItemKind, MediaRef, Project, ProjectView};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_CACHE: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

fn error(message: impl ToString) -> EngineError {
    EngineError::Other(message.to_string())
}
fn string<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| error(format!("`{key}` must be a nonempty string")))
}

pub fn validate_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes().get(3).is_some_and(|c| c.is_ascii_digit()));
    if name.trim() != name
        || name.is_empty()
        || name.len() > 180
        || matches!(name, "." | "..")
        || name.ends_with('.')
        || name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        || reserved
    {
        return Err(error("Choose a project name without reserved filenames, separators, trailing dots, or surrounding spaces (up to 180 UTF-8 bytes)."));
    }
    Ok(())
}

/// Canonicalization on Windows returns an extended-length spelling; document paths use the
/// ordinary drive/UNC spelling so a normal local file is not mistaken for a device reference.
fn ordinary(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    if let Some(drive) = value.strip_prefix(r"\\?\")
        && drive.as_bytes().get(1) == Some(&b':')
    {
        return drive.to_string();
    }
    value.into_owned()
}

fn destination(p: &Value) -> Result<(String, PathBuf, String)> {
    if cfg!(target_arch = "wasm32") {
        return Err(error("Project folders are available in the desktop app."));
    }
    let name = string(p, "name")?.to_string();
    validate_name(&name)?;
    let parent = std::fs::canonicalize(string(p, "parentDirectory")?).map_err(|e| error(format!("Choose an accessible parent directory: {e}")))?;
    if !parent.is_dir() {
        return Err(error("The project location must be a directory."));
    }
    let root = parent.join(&name);
    let file = root.join(format!("{name}.fcproj"));
    if file.to_str().is_none() {
        return Err(error("The project location must be valid Unicode."));
    }
    let path = ordinary(&file);
    Ok((name, root, path))
}

fn reserve(root: &Path) -> Result<()> {
    std::fs::create_dir(root).map_err(|e| error(format!("Cannot create project folder {}: {e}. Choose a different name or location.", ordinary(root))))
}

/// Visit only filesystem references, never arbitrary user text or metadata.
pub(crate) fn map_paths(project: &mut Project, mut map: impl FnMut(&str) -> String) {
    for item in project.items.values_mut() {
        if let ItemKind::Media(m) = &mut item.kind {
            for reference in [Some(&mut m.media), m.proxy.as_mut()].into_iter().flatten() {
                if let MediaRef::File { path } = reference {
                    *path = map(path);
                }
            }
        }
    }
    let settings = &mut project.settings;
    for path in [
        &mut settings.scratch.captured,
        &mut settings.scratch.auto_save,
        &mut settings.scratch.video_previews,
        &mut settings.scratch.audio_previews,
        &mut settings.ingest.destination,
    ]
    .into_iter()
    .flatten()
    {
        *path = map(path);
    }
}

/// Resolve lexical paths for serialization, without opening or granting access to them.
pub(crate) fn absolute_reference(path: &str, document: Option<&str>) -> Option<PathBuf> {
    let resolved = craft_docpath::resolve(path, document.map(Path::new)).ok()?;
    let resolved = std::path::absolute(resolved).ok()?;
    let mut clean = PathBuf::new();
    for part in resolved.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            _ => clean.push(part.as_os_str()),
        }
    }
    Some(clean)
}

pub(crate) fn for_document(project: &Project, source: Option<&str>, destination: &str) -> Project {
    let mut copy = project.clone();
    let root = std::path::absolute(destination).ok().and_then(|p| p.parent().map(Path::to_path_buf));
    map_paths(&mut copy, |path| {
        let Some(full) = absolute_reference(path, source.or(Some(destination))) else { return path.to_string() };
        root.as_ref()
            .and_then(|r| full.strip_prefix(r).ok())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| full.to_string_lossy().into_owned())
    });
    copy
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Recent {
    pub path: String,
    pub title: String,
    pub last_opened: Option<i64>,
    pub last_modified: Option<i64>,
    pub bytes: Option<u64>,
    pub available: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Library {
    version: u32,
    list_view: bool,
    parent_directory: String,
    entries: BTreeMap<String, Recent>,
    grants: BTreeMap<String, BTreeSet<String>>,
}
/// Convert filesystem timestamps without panicking on an out-of-range date.
pub fn unix_timestamp(time: std::time::SystemTime) -> Option<i64> {
    let seconds = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok()?,
        Err(error) => {
            let duration = error.duration();
            i64::try_from(duration.as_secs()).ok()?.checked_neg()?.checked_sub(i64::from(duration.subsec_nanos() != 0))?
        }
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0).map(|date| date.timestamp())
}

fn cache_path(s: &Session) -> Option<PathBuf> {
    s.prefs_path.as_ref()?.parent().map(|d| d.join("recent-projects.json"))
}
fn load(s: &Session) -> Library {
    let read = || -> Option<Library> {
        let file = std::fs::File::open(cache_path(s)?).ok()?;
        if file.metadata().ok()?.len() > MAX_CACHE {
            return None;
        }
        let cache: Library = serde_json::from_reader(file.take(MAX_CACHE)).ok()?;
        (cache.version == 1).then_some(cache)
    };
    read().unwrap_or_default()
}
struct CacheBytes(Vec<u8>);
impl Write for CacheBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = self.0.len().checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("Project metadata size overflow"))?;
        if length as u64 > MAX_CACHE {
            return Err(std::io::Error::other("Local project metadata is too large to cache; media may need relinking after restarting."));
        }
        self.0.try_reserve(bytes.len()).map_err(std::io::Error::other)?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn persist(s: &Session, cache: &mut Library) -> Result<()> {
    cache.version = 1;
    let Some(path) = cache_path(s) else { return Ok(()) };
    let mut data = CacheBytes(Vec::new());
    serde_json::to_writer(&mut data, cache).map_err(error)?;
    filmcraft_format::atomic_write(&path, &data.0).map_err(error)
}

fn key(path: &str) -> String {
    let path = std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
    if cfg!(windows) { path.replace('/', "\\").to_lowercase() } else { path }
}
fn warning(s: &mut Session, e: impl ToString) {
    s.events.push(crate::Event::Toast { message: e.to_string(), error: true });
}

pub fn recent_projects(s: &mut Session, _: &Value) -> Result<Value> {
    let cache = load(s);
    let entries: Vec<_> = s
        .prefs
        .general
        .recent_projects
        .iter()
        .take(10)
        .map(|path| {
            let mut entry = cache.entries.get(&key(path)).cloned().unwrap_or_else(|| Recent {
                path: path.clone(),
                title: Path::new(path).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into()),
                ..Default::default()
            });
            entry.path = path.clone();
            // Do not probe remote volumes on the UI thread; their last known metadata is sufficient.
            entry
        })
        .collect();
    Ok(json!({"projects": entries, "listView": cache.list_view, "parentDirectory": cache.parent_directory}))
}

pub fn library_settings(s: &mut Session, p: &Value) -> Result<Value> {
    let mut cache = load(s);
    if let Some(value) = p.get("listView") {
        cache.list_view = value.as_bool().ok_or_else(|| error("`listView` must be a boolean"))?;
    }
    if let Some(value) = p.get("parentDirectory") {
        cache.parent_directory = value.as_str().ok_or_else(|| error("`parentDirectory` must be a string"))?.to_string();
    }
    persist(s, &mut cache)?;
    Ok(json!({"listView": cache.list_view, "parentDirectory": cache.parent_directory}))
}

pub fn remove_recent(s: &mut Session, p: &Value) -> Result<Value> {
    let path = string(p, "path")?;
    let mut prefs = s.prefs.clone();
    prefs.general.recent_projects.retain(|old| key(old) != key(path));
    let mut cache = load(s);
    cache.entries.remove(&key(path));
    persist(s, &mut cache)?;
    s.set_prefs(prefs).map_err(error)?;
    Ok(json!({"removed": path}))
}

pub fn locate_recent(s: &mut Session, p: &Value) -> Result<Value> {
    let previous = string(p, "previousPath")?.to_string();
    let path = string(p, "path")?.to_string();
    if s.is_dirty() {
        return Err(error("Save or discard the current project's changes before locating another project."));
    }
    let result = s.execute("file.open", json!({"path": path}))?;
    if key(&previous) != key(&path) {
        remove_recent(s, &json!({"path": previous}))?;
    }
    Ok(result)
}

pub(crate) fn note_recent(s: &mut Session) {
    if cfg!(target_arch = "wasm32") {
        return;
    }
    let Some(path) = s.path.clone() else { return };
    let mut cache = load(s);
    let metadata = std::fs::metadata(&path).ok();
    cache.entries.insert(
        key(&path),
        Recent {
            path: path.clone(),
            title: s.project.name.clone(),
            last_opened: unix_timestamp(std::time::SystemTime::now()),
            last_modified: metadata.as_ref().and_then(|m| m.modified().ok()).and_then(unix_timestamp),
            bytes: metadata.as_ref().map(std::fs::Metadata::len),
            available: metadata.is_some(),
        },
    );
    let keep: BTreeSet<_> = s.prefs.general.recent_projects.iter().map(|p| key(p)).collect();
    cache.entries.retain(|k, _| keep.contains(k));
    if let Err(e) = persist(s, &mut cache) {
        warning(s, format!("Recent-project metadata: {e}"));
    }
}

/// Persist only exact paths the user selected, and only for this local project path.
pub(crate) fn remember_access(s: &mut Session) {
    if cfg!(target_arch = "wasm32") {
        return;
    }
    let Some(path) = s.path.as_deref() else { return };
    let mut cache = load(s);
    let selected = s.media.selected_paths(&s.project);
    if selected.is_empty() {
        return;
    }
    let grants = cache.grants.entry(key(path)).or_default();
    let before = grants.len();
    grants.extend(selected.into_iter().filter(|p| p.len() <= 32_768).map(|path| key(&path)).take(MAX_FILES.saturating_sub(before)));
    if grants.len() != before
        && let Err(e) = persist(s, &mut cache)
    {
        warning(s, format!("Remembering media access: {e}"));
    }
}

pub(crate) fn restore_access(s: &Session) {
    if cfg!(target_arch = "wasm32") {
        return;
    }
    let Some(path) = s.path.as_deref() else { return };
    let cache = load(s);
    if let Some(grants) = cache.grants.get(&key(path)) {
        for item in s.project.items.values().filter_map(|item| item.as_media()).take(MAX_FILES) {
            for reference in [Some(&item.media), item.proxy.as_ref()].into_iter().flatten() {
                if let MediaRef::File { path: stored } = reference
                    && grants.contains(&key(stored))
                {
                    s.media.allow(stored);
                }
            }
        }
    }
}

pub fn create(s: &mut Session, p: &Value) -> Result<Value> {
    if p.get("force").is_some_and(|v| !v.is_boolean()) {
        return Err(error("`force` must be a boolean"));
    }
    if s.is_dirty() && p.get("force").and_then(Value::as_bool) != Some(true) {
        return Err(error("Save or discard the current project's changes before creating a project."));
    }
    let (name, root, path) = destination(p)?;
    let template = p.get("template").map(|v| v.as_str().ok_or_else(|| error("`template` must be empty or demo"))).transpose()?.unwrap_or("empty");
    let pool = crate::MediaPool::default();
    let project = match template {
        "empty" => Project::new(&name),
        "demo" => crate::demo::demo_project(&pool).0,
        _ => return Err(error("`template` must be empty or demo")),
    };
    let mut project = project;
    project.name.clone_from(&name);
    project.root.name.clone_from(&name);
    let data = filmcraft_format::encode(&project, false);
    reserve(&root)?;
    if let Err(e) = s.services.write_file_new(&path, &data) {
        let _ = std::fs::remove_dir(&root); // only an empty directory, never recursive cleanup
        return Err(error(format!("Cannot save the new project: {e}")));
    }
    crate::masks::discard_jobs(s);
    crate::commands::install_project(s, project, Some(path.clone()), true);
    s.note_recent_project();
    if let Err(e) = library_settings(s, &json!({"parentDirectory": root.parent().map(ordinary)})) {
        warning(s, e);
    }
    saved(s);
    Ok(json!({"path": path, "name": name}))
}

pub struct PendingSave {
    job: u64,
    source_path: Option<String>,
    epoch: u64,
    revision: u64,
    view: ProjectView,
    receiver: mpsc::Receiver<std::result::Result<(Project, String), String>>,
    grants: BTreeSet<String>,
}

pub fn save_as(s: &mut Session, p: &Value) -> Result<Value> {
    if s.workspace_save.is_some() {
        return Err(error("A project Save As is already running."));
    }
    let wait = match p.get("wait") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| error("`wait` must be a boolean"))?,
    };
    let (name, root, path) = destination(p)?;
    let mut project = (*s.project).clone();
    project.name.clone_from(&name);
    project.root.name.clone_from(&name);
    let old_document = s.path.clone();
    let old_root = old_document.as_deref().and_then(|p| Path::new(p).parent()).and_then(|p| std::fs::canonicalize(p).ok());
    let old_lexical_root = old_document.as_deref().and_then(|p| Path::new(p).parent()).and_then(|p| std::path::absolute(p).ok());
    let mut work = BTreeMap::<PathBuf, PathBuf>::new();
    for item in project.items.values_mut() {
        let ItemKind::Media(media) = &mut item.kind else { continue };
        // Proxy attachments are disposable; their old paths must not follow a moved project.
        media.proxy = None;
        let MediaRef::File { path: stored } = &mut media.media else { continue };
        // External references are preserved without accessing a NAS or requiring it online.
        let lexical = absolute_reference(stored, old_document.as_deref());
        if !lexical.as_ref().is_some_and(|p| old_lexical_root.as_ref().is_some_and(|root| p.starts_with(root))) {
            if let Some(lexical) = lexical {
                *stored = ordinary(&lexical);
            }
            continue;
        }
        let resolved = s.media.follow(stored).map_err(|e| error(format!("Cannot copy {stored}: {e}. Link the media before Save As.")))?;
        if std::fs::metadata(&resolved).is_err() {
            return Err(error(format!("Project-owned source {resolved} is unavailable; link it before Save As.")));
        }
        let source = PathBuf::from(&resolved);
        let owned = old_root.as_ref().and_then(|r| std::fs::canonicalize(&source).ok().filter(|real| real.starts_with(r)).map(|real| (r, real)));
        if let Some((old, real)) = owned {
            let relative = real.strip_prefix(old).map_err(error)?;
            let target = root.join(relative);
            work.insert(real.clone(), target.clone());
            if work.len() > MAX_FILES {
                return Err(error("Save As exceeds the 100,000-file project limit."));
            }
            if media.info.kind == filmcraft_media::MediaKind::ImageSequence {
                for frame in crate::media_pool::image_sequence_frames(&resolved, &*s.services)?.into_iter().flatten() {
                    let real = std::fs::canonicalize(&frame).map_err(error)?;
                    let relative = real.strip_prefix(old).map_err(|_| error("An image-sequence frame points outside the project folder."))?;
                    work.insert(real.clone(), root.join(relative));
                    if work.len() > MAX_FILES {
                        return Err(error("Save As exceeds the 100,000-file project limit."));
                    }
                }
            }
            *stored = ordinary(&target);
        } else {
            *stored = std::fs::canonicalize(&source).map(|p| ordinary(&p)).unwrap_or(resolved);
        }
    }
    // Rebase project-owned scratch destinations; retain explicit external overrides.
    map_paths(&mut project, |stored| {
        let Some(full) = absolute_reference(stored, old_document.as_deref()) else { return stored.to_string() };
        if full.starts_with(Path::new(&ordinary(&root))) {
            return ordinary(&full);
        }
        old_lexical_root
            .as_ref()
            .and_then(|r| full.strip_prefix(r).ok())
            .map(|relative| ordinary(&root.join(relative)))
            .unwrap_or_else(|| full.to_string_lossy().into_owned())
    });
    let project = for_document(&project, Some(&path), &path);
    let view = s.project_view();
    let bytes = filmcraft_format::encode_with_view(&project, Some(&view), false);
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0).checked_add(1).ok_or_else(|| error("Job identity space exhausted"))?;
    reserve(&root)?;
    let job = crate::Job { id, label: format!("Saving project: {name}"), progress: Default::default(), result: Default::default() };
    let progress = job.progress.clone();
    progress.total.store(work.len() as u64 + 1, Ordering::Relaxed);
    let result = job.result.clone();
    let services = s.services.clone();
    let (sender, receiver) = mpsc::channel();
    let (save_path, root_copy) = (path.clone(), root.clone());
    let run = move || {
        let outcome = (|| -> std::result::Result<(Project, String), String> {
            let mut buffer = vec![0u8; 1024 * 1024];
            for (source, target) in work {
                if progress.cancel.load(Ordering::Relaxed) {
                    return Err("Project Save As cancelled; the original project is unchanged.".into());
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut input = std::fs::File::open(&source).map_err(|e| format!("{}: {e}", source.display()))?;
                let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| e.to_string())?;
                loop {
                    if progress.cancel.load(Ordering::Relaxed) {
                        return Err("Project Save As cancelled; the original project is unchanged.".into());
                    }
                    let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    output.write_all(buffer.get(..n).ok_or("Invalid copy buffer length")?).map_err(|e| e.to_string())?;
                }
                output.sync_all().map_err(|e| e.to_string())?;
                progress.done.fetch_add(1, Ordering::Relaxed);
            }
            if progress.cancel.load(Ordering::Relaxed) {
                return Err("Project Save As cancelled; the original project is unchanged.".into());
            }
            services.write_file_new(&save_path, &bytes).map_err(|e| format!("Cannot publish project: {e}"))?;
            progress.done.fetch_add(1, Ordering::Relaxed);
            Ok((project, save_path.clone()))
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_dir(&root_copy);
        }
        let outcome = outcome.map_err(|e| format!("{e} Partial files may remain in {}; the current project is unchanged.", ordinary(&root_copy)));
        if let Err(e) = &outcome {
            *progress.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
        }
        *progress.status.lock().unwrap_or_else(|p| p.into_inner()) = match &outcome {
            Ok(_) => "Project saved".into(),
            Err(e) => e.clone(),
        };
        let report = outcome
            .as_ref()
            .map(|(_, path)| filmcraft_export::Report {
                path: path.clone(),
                frames: 0,
                seconds: 0.0,
                bytes: bytes.len() as u64,
                render_fps: 0.0,
                extra_files: vec![],
            })
            .map_err(Clone::clone);
        // Deliver the state transition before announcing job completion: UI continuations
        // must never observe a successful job while its project is still waiting to install.
        let _ = sender.send(outcome);
        *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        progress.finished.store(true, Ordering::Relaxed);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    let grants = old_document.as_deref().and_then(|old| load(s).grants.remove(&key(old))).unwrap_or_default();
    s.workspace_save = Some(PendingSave { job: id, source_path: old_document, epoch: s.document_epoch, revision: s.revision, view, receiver, grants });
    s.jobs.push(job);
    if wait {
        run();
        let outcome = s.jobs.iter().find(|job| job.id == id).and_then(|job| job.result.lock().unwrap_or_else(|e| e.into_inner()).clone());
        poll(s);
        if let Some(Err(e)) = outcome {
            return Err(error(e));
        }
    } else if let Err(e) = std::thread::Builder::new().name("filmcraft-save-project".into()).spawn(run) {
        s.workspace_save = None;
        let _ = std::fs::remove_dir(&root);
        if let Some(job) = s.jobs.iter().find(|j| j.id == id) {
            *job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(e.to_string()));
            job.progress.finished.store(true, Ordering::Relaxed);
        }
        return Err(error(e));
    }
    Ok(json!({"path": path, "job": id}))
}

pub struct Thumbnail {
    epoch: u64,
    generation: u64,
    path: String,
    receiver: mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
}

pub(crate) struct ThumbnailWork {
    project: std::sync::Arc<Project>,
    media: std::sync::Arc<crate::MediaPool>,
    services: std::sync::Arc<dyn crate::Services>,
    sequence: filmcraft_project::ItemId,
    time: filmcraft_time::Tick,
    scale: f32,
    epoch: u64,
    generation: u64,
    path: String,
}

/// Keep at most one rendering worker and one latest saved snapshot, even during rapid saves.
pub(crate) fn saved(s: &mut Session) {
    s.workspace_save_generation = s.workspace_save_generation.wrapping_add(1);
    s.workspace_thumbnail_next = None;
    if cfg!(target_arch = "wasm32") {
        return;
    }
    let (Some(path), Some(sequence)) = (s.path.clone(), s.state.active_sequence) else { return };
    let Some(seq) = s.project.sequence(sequence) else { return };
    if seq.settings.width == 0 || seq.settings.height == 0 {
        return;
    }
    let scale = (320.0 / seq.settings.width as f32).min(180.0 / seq.settings.height as f32).min(1.0);
    s.workspace_thumbnail_next = Some(ThumbnailWork {
        project: s.project.clone(),
        media: s.media.clone(),
        services: s.services.clone(),
        sequence,
        time: s.state.playheads.get(&sequence).copied().unwrap_or(filmcraft_time::Tick::ZERO),
        scale,
        epoch: s.document_epoch,
        generation: s.workspace_save_generation,
        path,
    });
    start_thumbnail(s);
}

fn start_thumbnail(s: &mut Session) {
    if s.workspace_thumbnail.is_some() {
        return;
    }
    let Some(work) = s.workspace_thumbnail_next.take() else { return };
    if work.epoch != s.document_epoch || work.generation != s.workspace_save_generation || s.path.as_deref() != Some(&work.path) {
        return;
    }
    let (epoch, generation, path) = (work.epoch, work.generation, work.path.clone());
    let (sender, receiver) = mpsc::channel();
    let result = std::thread::Builder::new().name("filmcraft-project-thumbnail".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || -> std::result::Result<Vec<u8>, String> {
            let provider = work.media.provider(work.project.clone(), work.services);
            let (w, h, rgba) = filmcraft_render::render_sequence_rgba8(
                &work.project,
                work.sequence,
                work.time,
                filmcraft_render::RenderOptions { scale: work.scale, captions: true, ..Default::default() },
                &provider,
            );
            if w > 320 || h > 180 {
                return Err("Project thumbnail exceeded its size limit".into());
            }
            let mut bytes = std::io::Cursor::new(Vec::new());
            let image = image::RgbaImage::from_raw(u32::try_from(w).map_err(|e| e.to_string())?, u32::try_from(h).map_err(|e| e.to_string())?, rgba)
                .ok_or("Invalid project thumbnail")?;
            image.write_to(&mut bytes, image::ImageFormat::Png).map_err(|e| e.to_string())?;
            Ok(bytes.into_inner())
        }))
        .unwrap_or_else(|_| Err("Project thumbnail worker failed".into()));
        let _ = sender.send(outcome);
    });
    match result {
        Ok(_) => s.workspace_thumbnail = Some(Thumbnail { epoch, generation, path, receiver }),
        Err(e) => warning(s, format!("Project thumbnail unavailable: {e}")),
    }
}

/// Hosts should keep polling until background publication has settled.
pub fn adopted(s: &Session, job: u64) -> bool {
    s.workspace_adopted_save == Some(job)
}

pub fn pending(s: &Session) -> bool {
    s.workspace_save.is_some() || s.workspace_thumbnail.is_some() || s.workspace_thumbnail_next.is_some()
}

pub(crate) fn poll(s: &mut Session) {
    if let Some(pending) = s.workspace_save.take() {
        match pending.receiver.try_recv() {
            Ok(Ok((project, path))) if pending.epoch == s.document_epoch && pending.revision == s.revision && pending.source_path == s.path => {
                crate::commands::install_project(s, project, Some(path.clone()), true);
                s.restore_project_view(pending.view);
                s.workspace_adopted_save = Some(pending.job);
                let mut cache = load(s);
                cache.grants.insert(key(&path), pending.grants);
                cache.parent_directory = Path::new(&path).parent().and_then(Path::parent).map(ordinary).unwrap_or_default();
                if let Err(e) = persist(s, &mut cache) {
                    warning(s, e);
                }
                restore_access(s);
                s.note_recent_project();
                saved(s);
                s.events.push(crate::Event::Toast { message: format!("Project saved to {path}"), error: false });
            }
            Ok(Ok((_, path))) => warning(s, format!("Saved a snapshot to {path}; newer edits remain in the current project.")),
            Ok(Err(e)) => warning(s, e),
            Err(mpsc::TryRecvError::Empty) => s.workspace_save = Some(pending),
            Err(mpsc::TryRecvError::Disconnected) => warning(s, format!("Project Save As job {} stopped; the current project is unchanged.", pending.job)),
        }
    }
    if let Some(pending) = s.workspace_thumbnail.take() {
        match pending.receiver.try_recv() {
            Ok(outcome)
                if pending.epoch == s.document_epoch && pending.generation == s.workspace_save_generation && s.path.as_deref() == Some(&pending.path) =>
            {
                let write = || -> std::result::Result<(), String> {
                    let bytes = outcome?;
                    let root = Path::new(&pending.path).parent().ok_or("Project has no folder")?.join(".filmcraft");
                    if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
                        return Err("The thumbnail directory must not be a symbolic link".into());
                    }
                    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                    filmcraft_format::atomic_write(&root.join("thumbnail.png"), &bytes).map_err(|e| e.to_string())
                };
                if let Err(e) = write() {
                    warning(s, format!("Project thumbnail unavailable: {e}"));
                }
            }
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => s.workspace_thumbnail = Some(pending),
            Err(mpsc::TryRecvError::Disconnected) => warning(s, "Project thumbnail worker stopped."),
        }
    }
    start_thumbnail(s);
}

#[cfg(test)]
mod tests;
