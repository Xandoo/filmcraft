//! Local model selection shared by the Text panel and Transcribe Sequence dialog.
use crate::FilmcraftApp;
use egui::Ui;
use serde_json::json;

/// Reusable, serializable settings for editable text output in either transcription entry point.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub(crate) struct TextOutput {
    pub enabled: bool,
    pub options: filmcraft_engine::transcript::TextTrackOptions,
}

pub(crate) fn text_settings(app: &mut FilmcraftApp, ui: &mut Ui, toggle: bool) {
    let mut output = app.ui.transcript_output.clone();
    if toggle {
        let r = ui.checkbox(&mut output.enabled, "Create editable text clips after transcription");
        app.auto.add("transcription.text.enabled", r.rect, "Create editable text clips after transcription");
    }
    ui.add_enabled_ui(!toggle || output.enabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Group by");
            let combo = craft_ui::ComboBox::from_id_salt("transcription-text-grouping")
                .selected_text(if output.options.grouping == "words" { "Words" } else { "Characters" })
                .show_ui(ui, |ui| {
                    for (key, label) in [("characters", "Characters"), ("words", "Words")] {
                        let r = ui.selectable_value(&mut output.options.grouping, key.into(), label);
                        app.auto.add(&format!("transcription.text.grouping.{key}"), r.rect, label);
                    }
                });
            app.auto.add("transcription.text.grouping", combo.response.rect, "Group text by");
            let words = output.options.grouping == "words";
            let value = if words { &mut output.options.max_words } else { &mut output.options.max_chars };
            let r = ui.add(egui::DragValue::new(value).range(1..=if words { 256 } else { 4096 }).prefix("Maximum: "));
            app.auto.add(if words { "transcription.text.maxWords" } else { "transcription.text.maxChars" }, r.rect, "Maximum per text clip");
        });
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(egui::DragValue::new(&mut output.options.max_seconds).range(0.1..=60.0).speed(0.1).prefix("Max duration: ").suffix(" s"));
            app.auto.add("transcription.text.maxSeconds", r.rect, "Maximum text clip duration");
            let r = ui.add(egui::DragValue::new(&mut output.options.pause_seconds).range(0.0..=60.0).speed(0.1).prefix("Split at pause: ").suffix(" s"));
            app.auto.add("transcription.text.pauseSeconds", r.rect, "Split at pauses; zero disables");
        });
        let r = ui.checkbox(&mut output.options.break_sentences, "Start a new clip after each sentence");
        app.auto.add("transcription.text.breakSentences", r.rect, "Split sentences");
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(egui::DragValue::new(&mut output.options.font_size).range(1.0..=1000.0).prefix("Text size: ").suffix(" px"));
            app.auto.add("transcription.text.fontSize", r.rect, "Text size");
            ui.label("Track name");
            let r = ui.text_edit_singleline(&mut output.options.name);
            app.auto.add("transcription.text.name", r.rect, "New video track name");
        });
        ui.label(
            egui::RichText::new("Each clip starts at its first spoken word on the frame grid. Long words stay intact. Text remains editable in Graphics.")
                .small()
                .weak(),
        );
    });
    app.ui.transcript_output = output;
}

pub(crate) fn picker(app: &mut FilmcraftApp, ui: &mut Ui, selected: &mut String) {
    let directory = filmcraft_engine::transcript::models_dir();
    let before = selected.clone();
    let current = filmcraft_speech::models::find(selected).map(|m| m.name).unwrap_or("Choose model");
    let response = craft_ui::ComboBox::from_id_salt("transcription-model").selected_text(current).width(260.0).show_ui(ui, |ui| {
        for model in filmcraft_speech::models::catalogue() {
            ui.vertical(|ui| {
                let r = ui.selectable_value(selected, model.id.to_string(), model.name);
                app.auto.add(&format!("transcription.model.{}", model.id), r.rect, model.name);
                ui.label(egui::RichText::new(model.description).small());
                let source = ui.hyperlink_to("Model card and licence", model.source);
                app.auto.add(&format!("transcription.model.{}.source", model.id), source.rect, "Model card and licence");
                // Presence is deliberately cheap on the UI thread; the worker verifies hashes.
                let present = directory.as_ref().is_some_and(|dir| {
                    model.files.iter().all(|f| std::fs::metadata(filmcraft_speech::models::model_dir(dir, model).join(f.name)).is_ok_and(|v| v.len() == f.size))
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{:.0} MB download · {} · {}",
                        model.size() as f64 / 1e6,
                        if model.multilingual { "Multilingual" } else { "English" },
                        if present { "Downloaded" } else { "Downloads automatically" }
                    ))
                    .small()
                    .weak(),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "Memory estimate: {:.1} GB (weights and working buffers; varies with audio)",
                        model.estimated_memory_bytes() as f64 / 1e9
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.separator();
        }
    });
    app.auto.add("transcription.model", response.response.rect, "Local transcription model");
    if *selected != before
        && let Err(e) = app.session.execute("prefs.set", json!({"key":"mediaAnalysis.whisperModel","value":selected}))
    {
        app.ui.status = e.to_string();
    }
    let old_device = app.session.prefs.media_analysis.speech_device.clone();
    let mut device = old_device.clone();
    let device_picker = craft_ui::ComboBox::from_id_salt("transcription-device").selected_text(format!("Device: {device}")).show_ui(ui, |ui| {
        for (id, label) in [("auto", "Auto — prefer GPU"), ("gpu", "GPU — required"), ("cpu", "CPU")] {
            let r = ui.selectable_value(&mut device, id.to_string(), label);
            app.auto.add(&format!("transcription.device.{id}"), r.rect, label);
        }
    });
    app.auto.add("transcription.device", device_picker.response.rect, "Transcription device");
    if device != old_device
        && let Err(e) = app.session.execute("prefs.set", json!({"key":"mediaAnalysis.speechDevice","value":device}))
    {
        app.ui.status = e.to_string();
    }
}
