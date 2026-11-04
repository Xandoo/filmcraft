//! Project Selection and the shared desktop project-folder dialog.
use crate::{FilmcraftApp, native_dialogs::FileDialog};
use filmcraft_engine::project_workspace::Recent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct State {
    pub active: bool,
    pub list_view: bool,
    pub create: Option<Draft>,
    pub projects: Vec<Recent>,
    pub error: String,
    pub loading: bool,
    pub save_job: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Draft {
    pub name: String,
    pub parent_directory: String,
    pub template: String,
    pub save_as: bool,
    pub error: String,
}
type Catalog = Vec<(Recent, Option<egui::ColorImage>)>;
#[derive(Default)]
pub struct Runtime {
    /// Native hosts opt in to managed project folders; never restored from serialized UI state.
    pub enabled: bool,
    draft_epoch: u64,
    epoch: u64,
    loaded: bool,
    refresh_requested: bool,
    cancel_save: bool,
    receiver: Option<mpsc::Receiver<Result<Catalog, String>>>,
    textures: HashMap<String, egui::TextureHandle>,
    pub queued_imports: Vec<String>,
    pub after_save: Option<crate::state::ClipDialogDraft>,
}

impl Runtime {
    pub(crate) fn for_host(enabled: bool, epoch: u64) -> Self {
        Self { enabled, epoch, ..Default::default() }
    }
}

fn label(text: &str) -> &str {
    crate::i18n::t(text)
}
fn button(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, text: &str) -> bool {
    let r = ui.button(label(text));
    app.auto.add(id, r.rect, text);
    r.clicked()
}
fn fail(app: &mut FilmcraftApp, message: impl ToString) {
    let message = message.to_string();
    app.ui.project_selection.error.clone_from(&message);
    app.ui.status = message;
}

pub fn begin(app: &mut FilmcraftApp, template: &str, save_as: bool) {
    app.project_selection_runtime.draft_epoch = app.session.document_epoch;
    let settings = app.session.execute("file.recentProjects", json!({})).unwrap_or(Value::Null);
    let parent = settings["parentDirectory"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| app.session.path.as_deref().and_then(|p| Path::new(p).parent()).and_then(Path::parent).map(|p| p.to_string_lossy().into_owned()))
        .or_else(|| app.session.services.home_dir())
        .unwrap_or_default();
    app.ui.project_selection.create = Some(Draft {
        name: if save_as && app.session.path.is_none() {
            app.session.project.name.clone()
        } else if save_as {
            format!("{} copy", app.session.project.name)
        } else if template == "demo" {
            "Demo Project".into()
        } else {
            "Untitled".into()
        },
        parent_directory: parent,
        template: template.into(),
        save_as,
        error: String::new(),
    });
}

pub fn refresh(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if app.project_selection_runtime.receiver.is_some() {
        app.project_selection_runtime.refresh_requested = true;
        return;
    }
    let result = app.session.execute("file.recentProjects", json!({}));
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            fail(app, e);
            return;
        }
    };
    app.ui.project_selection.list_view = value["listView"].as_bool().unwrap_or(false);
    let entries: Vec<Recent> = serde_json::from_value(value["projects"].clone()).unwrap_or_default();
    app.ui.project_selection.projects.clone_from(&entries);
    app.ui.project_selection.loading = true;
    app.project_selection_runtime.textures.clear();
    let (sender, receiver) = mpsc::channel();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("filmcraft-project-catalog".into()).spawn(move || {
        let catalog = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            entries
                .into_iter()
                .take(10)
                .map(|mut entry| {
                    let metadata = std::fs::metadata(&entry.path).ok().filter(|m| m.is_file());
                    entry.available = metadata.is_some();
                    entry.bytes = metadata.as_ref().map(std::fs::Metadata::len);
                    entry.last_modified = metadata.and_then(|m| m.modified().ok()).and_then(filmcraft_engine::project_workspace::unix_timestamp);
                    let thumbnail = Path::new(&entry.path).parent().map(|p| p.join(".filmcraft/thumbnail.png"));
                    let picture = thumbnail.and_then(|path| {
                        use std::io::Read;
                        let file = std::fs::File::open(path).ok()?;
                        if file.metadata().ok()?.len() > 1024 * 1024 {
                            return None;
                        }
                        let mut bytes = Vec::new();
                        file.take(1024 * 1024).read_to_end(&mut bytes).ok()?;
                        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
                        let mut limits = image::Limits::default();
                        limits.max_image_width = Some(512);
                        limits.max_image_height = Some(512);
                        limits.max_alloc = Some(4 * 1024 * 1024);
                        reader.limits(limits);
                        let rgba = reader.decode().ok()?.to_rgba8();
                        if rgba.width() == 0 || rgba.height() == 0 {
                            return None;
                        }
                        Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
                    });
                    (entry, picture)
                })
                .collect::<Catalog>()
        }))
        .map_err(|_| "Recent-project scan failed. Use Refresh to try again.".to_string());
        let _ = sender.send(catalog);
        ctx.request_repaint();
    });
    match spawned {
        Ok(_) => app.project_selection_runtime.receiver = Some(receiver),
        Err(e) => {
            app.ui.project_selection.loading = false;
            fail(app, e);
        }
    }
}

pub fn poll(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if cfg!(target_arch = "wasm32") {
        return;
    }
    if !app.project_selection_runtime.enabled {
        if filmcraft_engine::project_workspace::pending(&app.session) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        return;
    }
    if app.project_selection_runtime.epoch != app.session.document_epoch {
        app.project_selection_runtime.epoch = app.session.document_epoch;
        let saved_copy = app.ui.project_selection.save_job.is_some_and(|job| filmcraft_engine::project_workspace::adopted(&app.session, job));
        if app.project_selection_runtime.draft_epoch != app.session.document_epoch && !saved_copy {
            app.project_selection_runtime.after_save = None;
            if app.ui.project_selection.create.as_ref().is_some_and(|draft| draft.save_as) {
                app.ui.project_selection.create = None;
                app.ui.project_selection.save_job = None;
            }
        }
        let entering_editor = app.ui.project_selection.active && !app.session.project_closed;
        app.ui.project_selection.active = app.session.project_closed;
        if entering_editor {
            app.ui.mode = crate::state::Mode::Edit;
        }
        app.project_selection_runtime.loaded = false;
        if !app.session.project_closed && app.ui.project_selection.create.as_ref().is_some_and(|draft| !draft.save_as) {
            app.ui.project_selection.create = None;
            let imports = std::mem::take(&mut app.project_selection_runtime.queued_imports);
            if !imports.is_empty()
                && let Err(e) = app.session.execute("file.import", json!({"paths":imports}))
            {
                fail(app, e);
            }
        }
        app.stop();
        app.stop_source();
    }
    if app.ui.project_selection.active && !app.project_selection_runtime.loaded {
        app.project_selection_runtime.loaded = true;
        refresh(app, ctx);
    }
    if let Some(receiver) = app.project_selection_runtime.receiver.take() {
        match receiver.try_recv() {
            Ok(Ok(catalog)) => {
                app.ui.project_selection.projects.clear();
                for (entry, image) in catalog {
                    if let Some(image) = image {
                        app.project_selection_runtime
                            .textures
                            .insert(entry.path.clone(), ctx.load_texture(format!("recent:{}", entry.path), image, egui::TextureOptions::LINEAR));
                    }
                    app.ui.project_selection.projects.push(entry);
                }
                app.ui.project_selection.loading = false;
            }
            Ok(Err(error)) => {
                app.ui.project_selection.loading = false;
                fail(app, error);
            }
            Err(mpsc::TryRecvError::Empty) => app.project_selection_runtime.receiver = Some(receiver),
            Err(mpsc::TryRecvError::Disconnected) => {
                app.ui.project_selection.loading = false;
                fail(app, "Recent-project scan stopped. Use Refresh to try again.");
            }
        }
    }
    if app.project_selection_runtime.refresh_requested && app.project_selection_runtime.receiver.is_none() {
        app.project_selection_runtime.refresh_requested = false;
        refresh(app, ctx);
    }
    if let Some(id) = app.ui.project_selection.save_job {
        if let Some(job) = app.session.jobs.iter().find(|job| job.id == id) {
            let outcome = job.result.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(outcome) = outcome {
                app.ui.project_selection.save_job = None;
                match outcome {
                    Ok(report)
                        if !app.session.is_dirty()
                            && app.session.path.as_deref() == Some(report.path.as_str())
                            && filmcraft_engine::project_workspace::adopted(&app.session, id) =>
                    {
                        app.ui.project_selection.create = None;
                        if let Some(prompt) = app.project_selection_runtime.after_save.take() {
                            crate::panels::clip_dialogs::leave(app, ctx, &prompt);
                        }
                    }
                    Ok(_) => {
                        let message = "The snapshot was saved; the current project has changed. Choose another name or location to save its latest state.";
                        if let Some(draft) = app.ui.project_selection.create.as_mut() {
                            draft.error = message.into();
                        }
                        fail(app, message);
                    }
                    Err(e) if app.project_selection_runtime.cancel_save => {
                        app.project_selection_runtime.cancel_save = false;
                        app.ui.project_selection.create = None;
                        app.ui.status = e;
                        if let Some(prompt) = app.project_selection_runtime.after_save.take() {
                            app.ui.clip_dialog = Some(prompt);
                        }
                    }
                    Err(e) => {
                        if let Some(draft) = app.ui.project_selection.create.as_mut() {
                            draft.error.clone_from(&e);
                        }
                        fail(app, e);
                    }
                }
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    if app.project_selection_runtime.receiver.is_some() || filmcraft_engine::project_workspace::pending(&app.session) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// Return Some only for desktop project management actions owned by this page.
pub fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, p: &Value) -> Option<Result<Value, String>> {
    if cfg!(target_arch = "wasm32") || !app.project_selection_runtime.enabled {
        return None;
    }
    match id {
        "file.newProject" if p.get("name").is_none() => begin(app, "empty", false),
        "file.openDemoProject" if p.as_object().is_none_or(|m| m.is_empty()) => begin(app, "demo", false),
        "file.saveAs" if p.get("path").is_none() => begin(app, "empty", true),
        "file.save" | "file.saveAll" if app.session.path.is_none() => begin(app, "empty", true),
        "file.import" if app.ui.project_selection.active => {
            let paths: Vec<String> =
                p.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            if paths.is_empty() {
                return Some(app.pick(FileDialog::open_files(&["mp4", "mov", "mkv", "webm", "wav", "mp3", "png", "jpg"]), |app, paths| {
                    if !paths.is_empty() {
                        app.project_selection_runtime.queued_imports = paths;
                        begin(app, "empty", false);
                    }
                    Ok(Value::Null)
                }));
            }
            app.project_selection_runtime.queued_imports = paths;
            begin(app, "empty", false);
        }
        "file.projectSelection.refresh" => refresh(app, ctx),
        "file.save" | "file.saveAs" | "file.saveCopy" if p.get("path").is_some() => return None,
        _ if app.ui.project_selection.active && !allowed_on_selection(id) && filmcraft_engine::find_command(id).is_some_and(|command| command.journal) => {
            return Some(Err("Open or create a project first.".into()));
        }
        _ => return None,
    }
    Some(Ok(json!({"dialog": "createProject"})))
}

pub fn allowed_on_selection(id: &str) -> bool {
    id.starts_with("app.")
        || id.starts_with("prefs.")
        || id.starts_with("shortcuts.")
        || id.starts_with("help.")
        || id.starts_with("view.appearance")
        || id.starts_with("view.theme.")
        || id.starts_with("file.openRecent.")
        || matches!(
            id,
            "file.newProject"
                | "file.openDemoProject"
                | "file.createProject"
                | "file.open"
                | "file.import"
                | "file.recover"
                | "file.recoveryList"
                | "file.discardRecovery"
                | "file.recentProjects"
                | "file.removeRecentProject"
                | "file.locateRecentProject"
                | "file.projectSelectionSettings"
                | "file.clearRecent"
        )
}

fn timestamp(value: Option<i64>) -> String {
    value.and_then(|v| chrono::DateTime::<chrono::Utc>::from_timestamp(v, 0)).map(|v| v.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "—".into())
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: egui::Rect) {
    let ctx = ui.ctx().clone();
    let mut page = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(28.0)).id_salt("project-selection"));
    page.label(egui::RichText::new(label("FilmCraft")).size(28.0).strong().color(app.tokens.text));
    page.add_space(6.0);
    page.label(label("Your projects"));
    page.add_space(20.0);
    page.horizontal_wrapped(|ui| {
        if button(app, ui, "projects.new", "New Project") {
            begin(app, "empty", false);
        }
        if button(app, ui, "projects.open", "Open Project") {
            let _ = crate::menus::invoke(app, &ctx, "file.open", json!({}));
        }
        if button(app, ui, "projects.demo", "Create Demo Project") {
            begin(app, "demo", false);
        }
        if button(app, ui, "projects.refresh", "Refresh") {
            refresh(app, &ctx);
        }
        if !app.session.recovery_candidates().is_empty() && button(app, ui, "projects.recover", "Recover Unsaved Changes…") {
            let _ = crate::menus::invoke(app, &ctx, "file.recover", json!({}));
        }
    });
    page.add_space(20.0);
    page.horizontal(|ui| {
        ui.strong(label("Recent projects"));
        for (list, name, id) in [(false, "Grid view", "projects.grid"), (true, "List view", "projects.list")] {
            let r = ui.selectable_label(app.ui.project_selection.list_view == list, label(name));
            app.auto.add(id, r.rect, name);
            if r.clicked() {
                app.ui.project_selection.list_view = list;
                if let Err(e) = app.session.execute("file.projectSelectionSettings", json!({"listView": list})) {
                    fail(app, e);
                }
            }
        }
    });
    if !app.ui.project_selection.error.is_empty() {
        page.colored_label(egui::Color32::LIGHT_RED, &app.ui.project_selection.error);
    }
    if app.ui.project_selection.loading {
        page.label(label("Loading projects…"));
    }
    let entries = app.ui.project_selection.projects.clone();
    if entries.is_empty() && !app.ui.project_selection.loading {
        page.label(label("Create a project or open an existing one to get started."));
    }
    let list = app.ui.project_selection.list_view;
    let width = page.available_width();
    let columns = if list || !width.is_finite() || width < 268.0 { 1 } else { (width / 268.0).floor().min(10.0) as usize };
    egui::ScrollArea::vertical().show(&mut page, |ui| {
        egui::Grid::new("recent-projects-grid").num_columns(columns).spacing([16.0, 16.0]).show(ui, |ui| {
            for (i, entry) in entries.iter().enumerate() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(if list { (rect.width() - 90.0).max(200.0) } else { 240.0 });
                    ui.vertical(|ui| {
                        if !list {
                            let image_rect = ui.allocate_exact_size(egui::vec2(240.0, 135.0), egui::Sense::hover()).0;
                            if let Some(texture) = app.project_selection_runtime.textures.get(&entry.path) {
                                let size = texture.size_vec2();
                                let fit = (image_rect.width() / size.x).min(image_rect.height() / size.y);
                                let picture = egui::Rect::from_center_size(image_rect.center(), size * fit);
                                ui.painter().image(
                                    texture.id(),
                                    picture,
                                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                                    egui::Color32::WHITE,
                                );
                            } else {
                                ui.painter().rect_filled(image_rect, 6.0, app.tokens.header_bg);
                                // Original procedural placeholder: a small film frame and play glyph.
                                let center = image_rect.center();
                                let frame = egui::Rect::from_center_size(center, egui::vec2(56.0, 36.0));
                                ui.painter().rect_stroke(frame, 4.0, egui::Stroke::new(1.5, app.tokens.text_dim), egui::StrokeKind::Inside);
                                ui.painter().add(egui::Shape::convex_polygon(
                                    vec![center + egui::vec2(-5.0, -8.0), center + egui::vec2(-5.0, 8.0), center + egui::vec2(8.0, 0.0)],
                                    app.tokens.text_dim,
                                    egui::Stroke::NONE,
                                ));
                            }
                            let response = ui.interact(image_rect, ui.scope_id().with((&entry.path, "open")), egui::Sense::click());
                            app.auto.add(&format!("projects.item.{i}.thumbnail"), image_rect, &entry.title);
                            if response.clicked() {
                                open(app, &ctx, &entry.path);
                            }
                        }
                        let r = ui.link(&entry.title);
                        app.auto.add(&format!("projects.item.{i}.open"), r.rect, &entry.title);
                        if r.clicked() {
                            open(app, &ctx, &entry.path);
                        }
                        ui.label(format!("{} {}", label("Last opened:"), timestamp(entry.last_opened)));
                        if !entry.available && !app.ui.project_selection.loading {
                            ui.colored_label(egui::Color32::YELLOW, label("Unavailable"));
                        }
                        if list {
                            ui.add(egui::Label::new(&entry.path).wrap().selectable(true));
                            ui.label(format!(
                                "{} {}   ·   {}",
                                label("Modified:"),
                                timestamp(entry.last_modified),
                                entry.bytes.map(|b| format!("{b} bytes")).unwrap_or_else(|| "—".into())
                            ));
                        }
                        ui.horizontal(|ui| {
                            if button(app, ui, &format!("projects.item.{i}.locate"), "Locate") {
                                let previous = entry.path.clone();
                                let ctx = ctx.clone();
                                let r = app.pick(FileDialog::OpenProject, move |app, paths| {
                                    if let Some(path) = paths.into_iter().next() {
                                        let r = app
                                            .session
                                            .execute("file.locateRecentProject", json!({"previousPath": previous, "path": path}))
                                            .map_err(|e| e.to_string())?;
                                        refresh(app, &ctx);
                                        return Ok(r);
                                    }
                                    Ok(Value::Null)
                                });
                                if let Err(e) = r {
                                    fail(app, e);
                                }
                            }
                            if button(app, ui, &format!("projects.item.{i}.remove"), "Remove from Recents") {
                                match app.session.execute("file.removeRecentProject", json!({"path": entry.path})) {
                                    Ok(_) => refresh(app, &ctx),
                                    Err(e) => fail(app, e),
                                }
                            }
                        });
                    });
                });
                if (i + 1) % columns == 0 {
                    ui.end_row();
                }
            }
        });
    });
}
fn open(app: &mut FilmcraftApp, ctx: &egui::Context, path: &str) {
    if let Err(e) = crate::menus::invoke(app, ctx, "file.open", json!({"path": path})) {
        fail(app, e);
    }
}

pub fn dialog(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if !app.project_selection_runtime.enabled {
        return;
    }
    let Some(mut draft) = app.ui.project_selection.create.clone() else { return };
    let mut action = "";
    let title = if draft.save_as {
        "Save As Project Folder"
    } else if draft.template == "demo" {
        "Create Demo Project"
    } else {
        "New Project"
    };
    egui::Modal::new(egui::Id::unique("create-project")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading(label(title));
        ui.add_space(12.0);
        ui.label(label("Project name"));
        let r = ui.add(egui::TextEdit::singleline(&mut draft.name).char_limit(180));
        app.auto.add("projectCreate.name", r.rect, "Project name");
        ui.label(label("Parent directory"));
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut draft.parent_directory).desired_width(340.0));
            app.auto.add("projectCreate.parentDirectory", r.rect, "Parent directory");
            if button(app, ui, "projectCreate.browse", "Browse…") {
                action = "browse";
            }
        });
        let target = Path::new(&draft.parent_directory).join(&draft.name).join(format!("{}.fcproj", draft.name));
        let r = ui.add(egui::Label::new(target.to_string_lossy()).wrap().selectable(true));
        app.auto.add("projectCreate.path", r.rect, &target.to_string_lossy());
        ui.small(label("Imported media stays in its original location."));
        if !draft.error.is_empty() {
            let r = ui.colored_label(egui::Color32::LIGHT_RED, &draft.error);
            app.auto.add("projectCreate.error", r.rect, &draft.error);
        }
        if let Err(e) = filmcraft_engine::project_workspace::validate_name(&draft.name) {
            ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
        }
        let running = app.ui.project_selection.save_job.is_some();
        if let Some(id) = app.ui.project_selection.save_job
            && let Some(job) = app.session.jobs.iter().find(|job| job.id == id)
        {
            ui.add(egui::ProgressBar::new(job.progress.fraction().clamp(0.0, 1.0)).text(label("Saving project files…")));
        }
        ui.horizontal(|ui| {
            if button(app, ui, "projectCreate.cancel", "Cancel") {
                action = "cancel";
            }
            let valid = filmcraft_engine::project_workspace::validate_name(&draft.name).is_ok() && !draft.parent_directory.is_empty();
            let r = ui.add_enabled(valid && !running, egui::Button::new(label(if draft.save_as { "Save" } else { "Create Project" })));
            app.auto.add("projectCreate.create", r.rect, if draft.save_as { "Save" } else { "Create Project" });
            if r.clicked() {
                action = "create";
            }
        });
    });
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        action = "cancel";
    }
    app.ui.project_selection.create = Some(draft.clone());
    match action {
        "browse" => {
            let epoch = app.session.document_epoch;
            app.pick_ui(FileDialog::Folder { at: (!draft.parent_directory.is_empty()).then_some(draft.parent_directory) }, move |app, paths| {
                if app.session.document_epoch != epoch {
                    return;
                }
                if let Some(parent) = paths.into_iter().next()
                    && let Some(draft) = app.ui.project_selection.create.as_mut()
                {
                    draft.parent_directory = parent;
                    if app.project_selection_runtime.after_save.is_some()
                        && app.ui.clip_dialog.as_ref().is_some_and(|prompt| matches!(prompt.command.as_str(), "app.quit" | "file.closeProject" | "file.open"))
                    {
                        app.project_selection_runtime.after_save = app.ui.clip_dialog.take();
                    }
                }
            });
        }
        "cancel" => {
            if let Some(id) = app.ui.project_selection.save_job {
                if let Some(job) = app.session.jobs.iter().find(|j| j.id == id) {
                    job.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    app.project_selection_runtime.cancel_save = true;
                }
            } else {
                app.ui.project_selection.create = None;
                app.project_selection_runtime.queued_imports.clear();
                if let Some(prompt) = app.project_selection_runtime.after_save.take() {
                    app.ui.clip_dialog = Some(prompt);
                }
            }
        }
        "create" => {
            let command = if draft.save_as { "file.saveAsProject" } else { "file.createProject" };
            let params = json!({"name": draft.name, "parentDirectory": draft.parent_directory, "template": draft.template});
            if !draft.save_as && app.session.is_dirty() {
                app.ui.project_selection.create = None;
                app.ui.clip_dialog = Some(crate::state::ClipDialogDraft { command: command.into(), params, info: Value::Null, error: String::new() });
                return;
            }
            match app.session.execute(command, params) {
                Ok(result) => {
                    if draft.save_as {
                        app.project_selection_runtime.cancel_save = false;
                        app.ui.project_selection.save_job = result["job"].as_u64();
                    } else {
                        app.ui.project_selection.create = None;
                        app.ui.project_selection.error.clear();
                        let imports = std::mem::take(&mut app.project_selection_runtime.queued_imports);
                        if !imports.is_empty()
                            && let Err(e) = app.session.execute("file.import", json!({"paths": imports}))
                        {
                            fail(app, e);
                        }
                    }
                }
                Err(e) => {
                    if let Some(draft) = app.ui.project_selection.create.as_mut() {
                        draft.error = e.to_string();
                    }
                    fail(app, e);
                }
            }
        }
        _ => {}
    }
}
