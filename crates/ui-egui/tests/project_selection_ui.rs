//! Desktop project navigation, creation and folder-save regression tests.
use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::{FilmcraftApp, control::ControlRequest};
use serde_json::{Value, json};
use std::sync::mpsc::{Sender, channel};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}
impl Driver {
    fn new(session: Session) -> Self {
        let (tx, rx) = channel();
        let mut app = FilmcraftApp::new(session).with_control(rx);
        app.project_selection_runtime.enabled = true;
        let mut builder = Harness::builder().with_size(egui::vec2(1200.0, 820.0)).with_max_steps(10_000);
        if std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").is_some() {
            builder = builder.wgpu();
        }
        let harness = builder.build_eframe(move |_| app);
        let mut driver = Self { harness, tx };
        driver.frames(6);
        driver
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params);
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(value) = reply.try_recv() {
                return value;
            }
        }
        panic!("No reply to {method}")
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let value = self.call(method, params);
        assert_eq!(value["ok"], true, "{value}");
        value["result"].clone()
    }
    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id":id}));
        self.frames(4);
    }
    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }
    fn snapshot(&mut self, name: &str) {
        let Some(folder) = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR") else { return };
        self.frames(2);
        let image = self.harness.render().unwrap();
        let folder = std::path::PathBuf::from(folder);
        std::fs::create_dir_all(&folder).unwrap();
        image.save(folder.join(format!("{name}.png"))).unwrap();
    }
}
fn home(temp: &tempfile::TempDir) -> Session {
    let mut session = Session::new(std::sync::Arc::new(filmcraft_engine::FsServices));
    session.project_closed = true;
    session.prefs_path = Some(temp.path().join("preferences.json"));
    session
}

#[test]
fn home_creation_and_close_return_to_selection() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = Driver::new(home(&temp));
    assert!(d.app().ui.project_selection.active);
    let elements = d.ok("ui.elements", json!({"prefix":"projects."}));
    assert!(elements.as_array().unwrap().iter().any(|e| e["id"] == "projects.new"));
    d.snapshot("project-selection-empty");
    d.click("projects.new");
    let fields = d.ok("ui.elements", json!({"prefix":"projectCreate."}));
    let ids: Vec<_> = fields.as_array().unwrap().iter().map(|element| element["id"].as_str().unwrap()).collect();
    let unique: std::collections::BTreeSet<_> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "Project creation must draw each widget only once");
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"My Film","parentDirectory":temp.path()}}}));
    d.snapshot("project-create");
    d.click("projectCreate.create");
    assert!(!d.app().ui.project_selection.active);
    assert_eq!(d.app().session.project.name, "My Film");
    assert!(temp.path().join("My Film/My Film.fcproj").is_file());
    d.app().ui.mode = filmcraft_ui_egui::state::Mode::Export;
    d.ok("engine.execute", json!({"command":"file.closeProject"}));
    d.frames(6);
    assert!(d.app().ui.project_selection.active);
    for _ in 0..100 {
        if !d.app().ui.project_selection.loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        d.frames(1);
    }
    d.snapshot("project-selection-grid");
    d.click("projects.list");
    assert!(d.app().ui.project_selection.list_view);
    d.snapshot("project-selection-list");
    let settings = d.app().session.execute("file.recentProjects", json!({})).unwrap();
    assert_eq!(settings["listView"], true);
    d.click("projects.item.0.open");
    assert_eq!(d.app().ui.mode, filmcraft_ui_egui::state::Mode::Edit);
}

#[test]
fn invalid_creation_and_failed_open_preserve_selection_and_report_errors() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = Driver::new(home(&temp));
    let response = d.call("engine.execute", json!({"command":"file.open","params":{"path":temp.path().join("missing.fcproj")}}));
    assert_eq!(response["ok"], false);
    assert!(d.app().ui.project_selection.active);
    d.click("projects.new");
    std::fs::create_dir(temp.path().join("Occupied")).unwrap();
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"Occupied","parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    assert!(d.app().ui.project_selection.create.as_ref().is_some_and(|draft| !draft.error.is_empty()));
    assert!(d.app().ui.project_selection.active);
    d.snapshot("project-create-error");
    d.click("projectCreate.cancel");
    assert!(d.app().ui.project_selection.create.is_none());
}

#[test]
fn demo_button_uses_saved_creation_and_new_project_asks_before_discarding() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = Driver::new(home(&temp));
    d.click("projects.demo");
    assert_eq!(d.app().ui.project_selection.create.as_ref().unwrap().template, "demo");
    d.ok("ui.set", json!({"projectSelection":{"create":{"parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    assert!(d.app().session.state.active_sequence.is_some());
    assert!(d.app().session.path.is_some());
    d.ok("engine.execute", json!({"command":"file.newBin","params":{"name":"Unsaved"}}));
    let before = d.app().session.project.clone();
    d.ok("ui.menu.invoke", json!({"id":"file.newProject"}));
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"Replacement","parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    assert!(d.app().ui.clip_dialog.as_ref().is_some_and(|dialog| dialog.command == "file.createProject"));
    assert!(std::sync::Arc::ptr_eq(&before, &d.app().session.project));
    d.click("createProject.cancel");
    assert!(std::sync::Arc::ptr_eq(&before, &d.app().session.project));
}

#[test]
fn no_project_blocks_edits_and_media_import_waits_for_creation() {
    let temp = tempfile::tempdir().unwrap();
    let mut d = Driver::new(home(&temp));
    let response = d.call("engine.execute", json!({"command":"file.newBin","params":{"name":"Invisible"}}));
    assert_eq!(response["ok"], false);
    let file = temp.path().join("source.wav");
    std::fs::write(&file, filmcraft_media::wav::write_wav16(&[0.0; 480], 1, 48_000)).unwrap();
    d.ok("engine.execute", json!({"command":"file.import","params":{"paths":[file]}}));
    assert!(d.app().session.project.items.is_empty());
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"Imported","parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    assert_eq!(d.app().session.project.items.values().filter(|i| i.as_media().is_some()).count(), 1);
}

#[test]
fn save_as_completes_the_folder_dialog_and_adopts_the_copied_project() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = home(&temp);
    session.execute("file.createProject", json!({"name":"Original","parentDirectory":temp.path()})).unwrap();
    let internal = temp.path().join("Original/voice.wav");
    std::fs::write(&internal, filmcraft_media::wav::write_wav16(&[0.0; 480], 1, 48_000)).unwrap();
    session.execute("file.import", json!({"paths":[internal]})).unwrap();
    let mut d = Driver::new(session);
    d.ok("ui.menu.invoke", json!({"id":"file.saveAs"}));
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"Copied","parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while d.app().ui.project_selection.create.is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        d.frames(1);
    }
    assert!(d.app().ui.project_selection.create.is_none(), "{:?}", d.app().ui.project_selection);
    assert!(d.app().ui.project_selection.save_job.is_none());
    assert_eq!(d.app().session.project.name, "Copied");
    assert!(temp.path().join("Copied/Copied.fcproj").is_file());
    assert!(temp.path().join("Copied/voice.wav").is_file());
    assert!(!d.app().session.is_dirty());
}

#[test]
fn a_folder_save_draft_cannot_quit_or_save_a_different_project() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = home(&temp);
    session.execute("file.newProject", json!({"name":"Transient"})).unwrap();
    session.execute("file.newBin", json!({"name":"Unsaved"})).unwrap();
    let mut d = Driver::new(session);
    d.harness.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    d.frames(3);
    d.click("quit.save");
    assert!(d.app().ui.project_selection.create.as_ref().is_some_and(|draft| draft.save_as));
    assert!(d.app().project_selection_runtime.after_save.is_some());
    d.ok("engine.execute", json!({"command":"file.newProject","params":{"name":"Different"}}));
    d.frames(3);
    assert_eq!(d.app().session.project.name, "Different");
    assert!(d.app().ui.project_selection.create.is_none());
    assert!(d.app().project_selection_runtime.after_save.is_none());
    assert!(!d.app().quit_confirmed);
}

#[test]
fn quit_save_on_a_transient_project_creates_its_folder_before_quitting() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = home(&temp);
    session.execute("file.newProject", json!({"name":"Transient"})).unwrap();
    session.execute("file.newBin", json!({"name":"Unsaved work"})).unwrap();
    let mut d = Driver::new(session);
    d.harness.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    d.frames(3);
    d.click("quit.save");
    assert!(!d.app().quit_confirmed);
    d.ok("ui.set", json!({"projectSelection":{"create":{"name":"Saved Transient","parentDirectory":temp.path()}}}));
    d.click("projectCreate.create");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !d.app().quit_confirmed && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        d.frames(1);
    }
    assert!(d.app().quit_confirmed, "{:?}", d.app().ui.project_selection);
    assert!(temp.path().join("Saved Transient/Saved Transient.fcproj").is_file());
    assert!(!d.app().session.is_dirty());
}
