//! Selected-clip animation views. Gestures preview locally and commit through engine commands.
use super::effect_controls::{Interaction, timeline_time_of};
use crate::{FilmcraftApp, dock::PanelKind, state::KeyframeRef, theme::Tokens};
use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use filmcraft_project::{Keyframe, Param, ParamValue, TrackItem};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationUi {
    pub pps: f64,
    pub scroll: f64,
    pub height_zoom: f32,
    pub animated_only: bool,
    pub components: [bool; 4],
    pub active_key: Option<KeyframeRef>,
    #[serde(skip)]
    clipboard: Vec<(KeyframeRef, Keyframe, Tick)>,
    #[serde(skip)]
    preview: Option<(KeyframeRef, Value)>,
}
impl Default for AnimationUi {
    fn default() -> Self {
        Self { pps: 80.0, scroll: 0.0, height_zoom: 1.0, animated_only: true, components: [true; 4], active_key: None, clipboard: Vec::new(), preview: None }
    }
}

impl AnimationUi {
    pub(crate) fn update(&mut self, p: &Value) -> Result<(), String> {
        let mut next = self.clone();
        for (name, target, low, high) in [("pps", &mut next.pps, 1.0, 24000.0), ("scroll", &mut next.scroll, 0.0, 1e7)] {
            if let Some(raw) = p.get(name) {
                *target =
                    raw.as_f64().filter(|v| v.is_finite() && *v >= low && *v <= high).ok_or_else(|| format!("animation {name} is outside its valid range"))?;
            }
        }
        if let Some(raw) = p.get("heightZoom") {
            next.height_zoom = raw.as_f64().filter(|v| v.is_finite() && (0.5..=4.0).contains(v)).ok_or("heightZoom must be between 0.5 and 4")? as f32;
        }
        if let Some(raw) = p.get("animatedOnly") {
            next.animated_only = raw.as_bool().ok_or("animatedOnly must be boolean")?;
        }
        if let Some(raw) = p.get("components") {
            next.components = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        }
        *self = next;
        Ok(())
    }
}

#[derive(Clone)]
struct Lane {
    clip: TrackItem,
    key: KeyframeRef,
    label: String,
    param: Param,
}

fn lanes(app: &FilmcraftApp) -> Vec<Lane> {
    let Some(q) = app.session.active_sequence() else { return Vec::new() };
    let mut out = Vec::new();
    for track in q.all_tracks() {
        for clip in &track.items {
            if !app.session.state.selection.contains(&clip.id) {
                continue;
            }
            // Validate the arithmetic once before using the shared time conversion in hot loops.
            let consumed = clip.duration.0 as f64 * clip.speed.abs();
            if clip.duration.0 <= 0
                || !clip.speed.is_finite()
                || clip.speed == 0.0
                || !consumed.is_finite()
                || !(1.0..i64::MAX as f64).contains(&consumed)
                || clip.start.0.checked_add(clip.duration.0).is_none()
            {
                continue;
            }
            if clip.source_in.0.checked_add(consumed.round() as i64).is_none() {
                continue;
            }
            for (effect, fx) in clip.effects.iter().enumerate() {
                for (name, param) in &fx.params {
                    if !valid_times(param) {
                        continue;
                    }
                    out.push(Lane {
                        clip: clip.clone(),
                        key: KeyframeRef { clip: clip.id.0, effect, param: name.clone(), mask: None, time: Tick::ZERO },
                        label: format!("{}:{} / {} / {}", clip.id.0, clip.name, fx.effect, name),
                        param: param.clone(),
                    });
                    if out.len() >= 4096 {
                        return out;
                    }
                }
                for (mask_index, mask) in fx.masks.iter().enumerate() {
                    for name in filmcraft_project::mask::MASK_PARAMS {
                        if let Some(param) = mask.param(name) {
                            if !valid_times(param) {
                                continue;
                            }
                            out.push(Lane {
                                clip: clip.clone(),
                                key: KeyframeRef { clip: clip.id.0, effect, param: name.to_string(), mask: Some(mask_index), time: Tick::ZERO },
                                label: format!("{}:{} / {} / Mask {} / {}", clip.id.0, clip.name, fx.effect, mask_index + 1, name),
                                param: param.clone(),
                            });
                            if out.len() >= 4096 {
                                return out;
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

fn valid_times(param: &Param) -> bool {
    param.keyframes.windows(2).all(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| b.time.0.checked_sub(a.time.0).is_some_and(|d| d > 0)))
}
fn visible_key(lane: &Lane, key: &Keyframe) -> bool {
    key.time >= lane.clip.source_in && key.time < lane.clip.source_out()
}

fn components(v: &ParamValue) -> Vec<f64> {
    match v {
        ParamValue::Float(v) => vec![*v],
        ParamValue::Vec2(v) => vec![v.x, v.y],
        ParamValue::Color(v) => v.iter().map(|v| *v as f64).collect(),
        _ => Vec::new(),
    }
}
fn value_json(template: &ParamValue, c: &[f64]) -> Option<Value> {
    match template {
        ParamValue::Float(_) => Some(json!(c.first()?)),
        ParamValue::Vec2(_) => Some(json!({"x":c.first()?,"y":c.get(1)?})),
        ParamValue::Color(_) => Some(json!(c)),
        _ => None,
    }
}
fn execute(app: &mut FilmcraftApp, cmd: &str, params: Value) -> Result<Value, String> {
    app.session.execute(cmd, params).map_err(|e| e.to_string())
}
fn edit(app: &mut FilmcraftApp, edits: Vec<Value>) -> Result<Value, String> {
    if edits.is_empty() {
        return Ok(Value::Null);
    }
    execute(app, "effects.editKeyframes", json!({"edits":edits}))
}
fn move_keys(app: &mut FilmcraftApp, delta: Tick) -> Result<Value, String> {
    if app.ui.keyframe_selection.is_empty() {
        return Ok(Value::Null);
    }
    let result = execute(app, "effects.moveKeyframes", json!({"keyframes":app.ui.keyframe_selection,"delta":delta.0}))?;
    app.ui.keyframe_selection = serde_json::from_value(result["keyframes"].clone()).map_err(|e| e.to_string())?;
    Ok(result)
}

fn fit_times(all: &[Lane], selected: &[KeyframeRef]) -> (Tick, Tick) {
    let times: Vec<Tick> = all
        .iter()
        .flat_map(|lane| {
            lane.param.keyframes.iter().filter_map(|key| {
                let reference = KeyframeRef { time: key.time, ..lane.key.clone() };
                (visible_key(lane, key) && selected.contains(&reference)).then(|| timeline_time_of(&lane.clip, key.time))
            })
        })
        .collect();
    if let Some(start) = times.iter().copied().min() {
        let end = times.iter().copied().max().unwrap_or(start);
        (start, end)
    } else {
        (all.iter().map(|l| l.clip.start).min().unwrap_or(Tick::ZERO), all.iter().map(|l| l.clip.end()).max().unwrap_or(Tick::ZERO))
    }
}

/// Prevent edit shortcuts in an animation panel from operating on timeline clips.
pub(crate) fn command(app: &mut FilmcraftApp, id: &str) -> Option<Result<Value, String>> {
    if !matches!(app.ui.focused, PanelKind::DopeSheet | PanelKind::CurveEditor) {
        return None;
    }
    let all = lanes(app);
    match id {
        "edit.selectAll" => {
            app.ui.keyframe_selection = all
                .iter()
                .flat_map(|l| l.param.keyframes.iter().filter(|k| visible_key(l, k)).map(|k| KeyframeRef { time: k.time, ..l.key.clone() }))
                .take(4096)
                .collect();
            Some(Ok(Value::Null))
        }
        "edit.deselectAll" => {
            app.ui.keyframe_selection.clear();
            Some(Ok(Value::Null))
        }
        "edit.copy" | "edit.cut" => {
            app.ui.animation.clipboard = all
                .iter()
                .flat_map(|l| {
                    l.param
                        .keyframes
                        .iter()
                        .filter(|k| visible_key(l, k))
                        .map(|k| (KeyframeRef { time: k.time, ..l.key.clone() }, k.clone(), timeline_time_of(&l.clip, k.time)))
                })
                .filter(|(key, _, _)| app.ui.keyframe_selection.contains(key))
                .take(4096)
                .collect();
            if id == "edit.cut" { command(app, "edit.clear") } else { Some(Ok(Value::Null)) }
        }
        "edit.clear" | "edit.rippleDelete" => {
            let edits = app.ui.keyframe_selection.iter().map(|key| json!({"key":key,"operation":"delete"})).collect();
            let result = edit(app, edits);
            if result.is_ok() {
                app.ui.keyframe_selection.clear();
            }
            Some(result)
        }
        "edit.paste" => {
            let first = app.ui.animation.clipboard.iter().map(|(_, _, t)| *t).min().unwrap_or(Tick::ZERO);
            let mut edits = Vec::new();
            let mut keys = Vec::new();
            for (original, k, time) in &app.ui.animation.clipboard {
                let Some(lane) = all
                    .iter()
                    .find(|l| l.key.clip == original.clip && l.key.effect == original.effect && l.key.mask == original.mask && l.key.param == original.param)
                else {
                    continue;
                };
                let delta = time.0.checked_sub(first.0).and_then(|v| app.session.playhead().0.checked_add(v));
                let Some(t) = delta.map(Tick) else {
                    return Some(Err("keyframe paste time overflows".into()));
                };
                if t < lane.clip.start || t >= lane.clip.end() {
                    return Some(Err("pasted keys must fit inside their clips".into()));
                }
                let key = KeyframeRef { time: lane.clip.effect_time_at(t), ..original.clone() };
                edits.push(json!({"key":key,"operation":"put","keyframe":k}));
                keys.push(key);
            }
            let result = edit(app, edits);
            if result.is_ok() {
                app.ui.keyframe_selection = keys;
            }
            Some(result)
        }
        _ => None,
    }
}

pub(crate) fn command_enabled(app: &FilmcraftApp, id: &str) -> Option<bool> {
    if !matches!(app.ui.focused, PanelKind::DopeSheet | PanelKind::CurveEditor) {
        return None;
    }
    match id {
        "edit.copy" | "edit.cut" | "edit.clear" | "edit.rippleDelete" => Some(!app.ui.keyframe_selection.is_empty()),
        "edit.paste" => Some(!app.ui.animation.clipboard.is_empty()),
        "edit.selectAll" | "edit.deselectAll" => Some(true),
        _ => None,
    }
}

pub(crate) fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect, curves: bool) {
    if !rect.is_finite() || rect.width() <= 12.0 || rect.height() <= 12.0 {
        return;
    }
    let id = if curves { "curveEditor" } else { "dopeSheet" };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(6.0)).id_salt(id));
    if !app.ui.animation.pps.is_finite() {
        app.ui.animation.pps = 80.0;
    }
    app.ui.animation.pps = app.ui.animation.pps.clamp(1.0, 24000.0);
    if !app.ui.animation.height_zoom.is_finite() {
        app.ui.animation.height_zoom = 1.0;
    }
    app.ui.animation.height_zoom = app.ui.animation.height_zoom.clamp(0.5, 4.0);
    if !app.ui.animation.scroll.is_finite() {
        app.ui.animation.scroll = 0.0;
    }
    app.ui.animation.scroll = app.ui.animation.scroll.clamp(0.0, 1e7);
    if child.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.ui.animation.preview = None;
    }
    let all = lanes(app);
    if all.is_empty() {
        child.label(tl!("Select clips to edit their animation"));
        return;
    }
    app.ui.keyframe_selection.retain(|key| {
        all.iter().any(|l| {
            l.key.clip == key.clip
                && l.key.effect == key.effect
                && l.key.mask == key.mask
                && l.key.param == key.param
                && l.param.keyframes.iter().any(|k| k.time == key.time && visible_key(l, k))
        })
    });
    let mut error = None;
    child.horizontal_wrapped(|ui| {
        let filter = ui.checkbox(&mut app.ui.animation.animated_only, tl!("Animated only"));
        app.auto.add(&format!("{id}.animatedOnly"), filter.rect, "Show animated properties only");
        let fit = ui.button(tl!("Fit selection"));
        app.auto.add(&format!("{id}.fit"), fit.rect, "Fit selected keys, or selected clips when no keys are selected");
        if fit.clicked() {
            let (start, end) = fit_times(&all, &app.ui.keyframe_selection);
            let span = (end - start).seconds().max(0.1);
            app.ui.animation.scroll = (start.seconds() - span * 0.05).max(0.0);
            app.ui.animation.pps = ((rect.width() - 230.0).max(20.0) as f64 / (span * 1.1)).clamp(1.0, 24000.0);
        }
        for (label, command_id) in [(tl!("Copy"), "edit.copy"), (tl!("Paste"), "edit.paste"), (tl!("Delete"), "edit.clear")] {
            let r = ui.button(label);
            app.auto.add(&format!("{id}.{command_id}"), r.rect, label);
            if r.clicked()
                && let Some(Err(e)) = command(app, command_id)
            {
                error = Some(e);
            }
        }
        let earlier = crate::icons::button(ui, crate::icons::Icon::StepBack, 24.0, false, &app.tokens, tl!("Move keys one frame earlier"));
        app.auto.add(&format!("{id}.nudgeEarlier"), earlier.rect, "Move keys one frame earlier");
        if earlier.clicked() {
            error = move_keys(app, -app.session.sequence_rate().frame_duration()).err();
        }
        let later = crate::icons::button(ui, crate::icons::Icon::StepFwd, 24.0, false, &app.tokens, tl!("Move keys one frame later"));
        app.auto.add(&format!("{id}.nudgeLater"), later.rect, "Move keys one frame later");
        if later.clicked() {
            error = move_keys(app, app.session.sequence_rate().frame_duration()).err();
        }
        let easing = craft_ui::ComboBox::from_id_salt((id, "ease")).selected_text(tl!("Easing")).show_ui(ui, |ui| {
            for (name, value) in [
                ("Linear", "linear"),
                ("Hold", "hold"),
                ("Ease In", "easeIn"),
                ("Ease Out", "easeOut"),
                ("Ease In-Out", "easeInOut"),
                ("Auto Bézier", "autoBezier"),
                ("Manual Bézier", "bezier"),
            ] {
                let r = ui.button(name);
                app.auto.add(&format!("{id}.easing.{value}"), r.rect, name);
                if r.clicked() {
                    error = execute(app, "effects.setInterpolation", json!({"keyframes":app.ui.keyframe_selection,"interpolation":value})).err();
                    ui.close();
                }
            }
        });
        app.auto.add(&format!("{id}.easing"), easing.response.rect, "Keyframe easing");
        if curves {
            for (i, label) in ["X / R", "Y / G", "B", "A"].iter().enumerate() {
                let r = ui.checkbox(&mut app.ui.animation.components[i], *label);
                app.auto.add(&format!("{id}.component.{i}"), r.rect, label);
            }
        }
    });
    child.horizontal_wrapped(|ui| {
        for (suffix, label, factor) in [("zoomOut", "-", 0.5), ("zoomIn", "+", 2.0)] {
            let r = ui.button(label).on_hover_text(if factor > 1.0 { "Zoom in time" } else { "Zoom out time" });
            app.auto.add(&format!("{id}.{suffix}"), r.rect, "Time zoom");
            if r.clicked() {
                app.ui.animation.pps = (app.ui.animation.pps * factor).clamp(1.0, 24000.0);
            }
        }
        let r = ui.add(egui::Slider::new(&mut app.ui.animation.pps, 1.0..=24000.0).logarithmic(true).text(tl!("Time zoom")));
        app.auto.add(&format!("{id}.zoom"), r.rect, "Time zoom (pixels per second)");
        let r = ui.add(egui::Slider::new(&mut app.ui.animation.height_zoom, 0.5..=4.0).text(tl!("Height zoom")));
        app.auto.add(&format!("{id}.heightZoom"), r.rect, "Expand rows for detailed value editing");
        let end = all.iter().map(|l| l.clip.end().seconds()).fold(1.0_f64, f64::max).min(1e7);
        let r = ui.add(egui::Slider::new(&mut app.ui.animation.scroll, 0.0..=end).text(tl!("Scroll")));
        app.auto.add(&format!("{id}.scroll"), r.rect, "Scroll animation in time");
    });
    numeric_editor(app, &mut child, &all, &mut error, id);
    let pps = app.ui.animation.pps;
    let scroll = app.ui.animation.scroll;
    time_ruler(app, &mut child, id, pps, scroll);
    let remaining = child.available_rect_before_wrap();
    if child.rect_contains_pointer(remaining) {
        let delta = child.input(|i| i.smooth_scroll_delta);
        if child.input(|i| i.modifiers.ctrl || i.modifiers.command) {
            app.ui.animation.pps = (pps * (delta.y as f64 * 0.01).exp()).clamp(1.0, 24000.0);
        } else if delta.x != 0.0 || child.input(|i| i.modifiers.shift) {
            app.ui.animation.scroll = (scroll - (delta.x + delta.y) as f64 / pps).max(0.0);
        }
    }
    let mut actions = Vec::new();
    egui::ScrollArea::vertical().id_salt((id, "rows")).show(&mut child, |ui| {
        let area = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let mut interaction = Interaction::begin(app, ui, area, if curves { u64::MAX - 1 } else { u64::MAX });
        // Hierarchy uses persistent collapsible headers without duplicating the shared key state.
        let mut clips = Vec::new();
        for lane in &all {
            if app.ui.animation.animated_only && !lane.param.is_animated() {
                continue;
            }
            if !clips.contains(&lane.key.clip) {
                clips.push(lane.key.clip);
            }
        }
        for clip in clips {
            let name = all.iter().find(|l| l.key.clip == clip).map(|l| l.clip.name.as_str()).unwrap_or("Clip");
            let group = egui::CollapsingHeader::new(name).id_salt((id, clip)).default_open(true).show(ui, |ui| {
                let mut effects = Vec::new();
                for l in all.iter().filter(|l| l.key.clip == clip) {
                    if app.ui.animation.animated_only && !l.param.is_animated() {
                        continue;
                    }
                    if !effects.contains(&l.key.effect) {
                        effects.push(l.key.effect);
                    }
                }
                for effect in effects {
                    let effect_name = all
                        .iter()
                        .find(|l| l.key.clip == clip && l.key.effect == effect)
                        .and_then(|l| l.clip.effects.get(effect))
                        .map(|e| e.effect.as_str())
                        .unwrap_or("Effect");
                    let display_name = filmcraft_project::find_effect(effect_name).map(|d| d.name).unwrap_or(effect_name);
                    let effect_group =
                        egui::CollapsingHeader::new(crate::i18n::t(display_name)).id_salt((id, clip, effect)).default_open(true).show(ui, |ui| {
                            let animated_only = app.ui.animation.animated_only;
                            for lane in all.iter().filter(|l| l.key.clip == clip && l.key.effect == effect && (!animated_only || l.param.is_animated())) {
                                draw_lane(app, ui, lane, curves, pps, scroll, &mut interaction, &mut actions);
                            }
                        });
                    app.auto.add(&format!("{id}.clip.{clip}.effect.{effect}"), effect_group.header_response.rect, display_name);
                }
            });
            app.auto.add(&format!("{id}.clip.{clip}"), group.header_response.rect, name);
        }
        interaction.finish(app, ui);
    });
    for (command, params) in actions {
        if let Err(e) = execute(app, command, params) {
            error = Some(e);
        }
    }
    if let Some(e) = error {
        app.ui.status = e;
    }
}

fn time_ruler(app: &mut FilmcraftApp, ui: &mut egui::Ui, prefix: &str, pps: f64, scroll: f64) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let indent = ui.spacing().indent * 2.0;
    let label = ((row.width() - indent) * 0.3).clamp(20.0, 190.0);
    let rect = Rect::from_min_max(pos2(row.min.x + indent + label, row.min.y), row.max - vec2(3.0, 0.0));
    if rect.width() <= 0.0 {
        return;
    }
    let t = app.tokens;
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let step = (80.0 / pps).max(0.01);
    for i in 0..=((rect.width() / 80.0).ceil() as usize).min(64) {
        let time = ((scroll / step).floor() + i as f64) * step;
        let x = rect.min.x + ((time - scroll) * pps) as f32;
        painter.text(pos2(x + 2.0, rect.min.y + 2.0), Align2::LEFT_TOP, format!("{time:.2}s"), Tokens::ui(10.0), t.text_dim);
        painter.line_segment([pos2(x, rect.max.y - 5.0), pos2(x, rect.max.y)], Stroke::new(1.0, t.separator));
    }
    let x = rect.min.x + ((app.session.playhead().seconds() - scroll) * pps) as f32;
    painter.line_segment([pos2(x, rect.min.y), pos2(x, rect.max.y)], Stroke::new(1.5, t.playhead));
    let r = ui.interact(rect, egui::Id::unique((prefix, "time-ruler")), Sense::click_and_drag());
    app.auto.add(&format!("{prefix}.ruler"), rect, "Animation time ruler");
    if (r.clicked() || r.dragged())
        && let Some(pos) = r.interact_pointer_pos()
    {
        let time = Tick::from_seconds_f64((scroll + (pos.x - rect.min.x) as f64 / pps).max(0.0));
        app.stop_for_scrub();
        app.session.set_playhead(app.session.sequence_rate().snap_nearest(time));
    }
}

fn numeric_editor(app: &mut FilmcraftApp, ui: &mut egui::Ui, all: &[Lane], error: &mut Option<String>, prefix: &str) {
    let Some(key) = app.ui.animation.active_key.as_ref().filter(|k| app.ui.keyframe_selection.contains(k)).or(app.ui.keyframe_selection.first()).cloned()
    else {
        return;
    };
    let Some(lane) = all.iter().find(|l| l.key.clip == key.clip && l.key.effect == key.effect && l.key.param == key.param && l.key.mask == key.mask) else {
        return;
    };
    let Some(k) = lane.param.keyframes.iter().find(|k| k.time == key.time) else { return };
    ui.horizontal_wrapped(|ui| {
        ui.label(tl!("Frame:"));
        let rate = app.session.sequence_rate();
        let old = rate.frame_at(timeline_time_of(&lane.clip, key.time));
        let numeric_id = ui.scope_id().with(("animation-numeric", key.clip, key.effect, &key.param, key.mask, key.time.0));
        let (mut frame, mut c) = ui.data(|d| d.get_temp::<(i64, Vec<f64>)>(numeric_id)).unwrap_or_else(|| (old, components(&k.value)));
        let r = ui.add(egui::DragValue::new(&mut frame).range(0..=i32::MAX as i64));
        app.auto.add(&format!("{prefix}.keyframe.frame"), r.rect, "Keyframe frame");
        let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let commit_time = !cancelled && ((r.changed() && !r.dragged()) || r.drag_stopped());
        if commit_time && frame != old {
            *error = move_keys(app, rate.tick_of(frame) - rate.tick_of(old)).err();
        }
        let mut changed = false;
        for (i, v) in c.iter_mut().enumerate() {
            if !v.is_finite() {
                ui.label(tl!("Auto"));
                continue;
            }
            let r = ui.add(egui::DragValue::new(v).speed(0.1).range(-1e9..=1e9));
            app.auto.add(&format!("{prefix}.keyframe.value.{i}"), r.rect, "Component value");
            changed |= r.changed() && !r.dragged();
            // DragValue's release commits the value displayed during the gesture.
            changed |= r.drag_stopped();
        }
        if changed
            && !cancelled
            && let Some(value) = value_json(&k.value, &c)
        {
            *error = edit(app, vec![json!({"key":key,"value":value})]).err();
        }
        if cancelled || changed || commit_time || (!ui.input(|i| i.pointer.any_down()) && !ui.memory(|m| m.focused().is_some())) {
            ui.data_mut(|d| d.remove::<(i64, Vec<f64>)>(numeric_id));
        } else {
            ui.data_mut(|d| d.insert_temp(numeric_id, (frame, c)));
        }
        let mut linked = k.tangents_linked;
        let r = ui.checkbox(&mut linked, tl!("Linked tangents"));
        app.auto.add(&format!("{prefix}.keyframe.linkedTangents"), r.rect, "Link tangent slopes");
        if r.changed() {
            *error = edit(app, vec![json!({"key":key,"tangentsLinked":linked})]).err();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_lane(
    app: &mut FilmcraftApp,
    ui: &mut egui::Ui,
    lane: &Lane,
    curves: bool,
    pps: f64,
    scroll: f64,
    interaction: &mut Interaction,
    actions: &mut Vec<(&'static str, Value)>,
) {
    let t = app.tokens;
    let graphable = !components(&lane.param.value).is_empty();
    let height = (if curves && graphable { 150.0 } else { 30.0 }) * app.ui.animation.height_zoom;
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width().max(40.0), height), Sense::hover());
    if !row.intersects(ui.clip_rect()) {
        return;
    }
    let prefix = if curves { "curveEditor" } else { "dopeSheet" };
    let mask = lane.key.mask.map(|i| format!("mask{i}.")).unwrap_or_default();
    let lane_id = format!("{prefix}.{}.{}.{mask}{}", lane.key.clip, lane.key.effect, lane.key.param);
    let label_w = (row.width() * 0.3).clamp(20.0, 190.0);
    let graph = Rect::from_min_max(pos2(row.min.x + label_w, row.min.y + 4.0), row.max - vec2(3.0, 4.0));
    if graph.width() < 8.0 {
        return;
    }
    let painter = ui.painter().with_clip_rect(row.intersect(ui.clip_rect()));
    let display_name = lane
        .clip
        .effects
        .get(lane.key.effect)
        .and_then(|fx| filmcraft_project::find_effect(&fx.effect))
        .and_then(|d| d.params.iter().find(|p| p.id == lane.key.param))
        .map(|p| p.label)
        .unwrap_or(&lane.key.param);
    painter.text(row.left_top() + vec2(3.0, 5.0), Align2::LEFT_TOP, crate::i18n::t(display_name), Tokens::ui(11.0), t.text);
    let add_rect = Rect::from_min_size(pos2(row.min.x + label_w - 24.0, row.min.y), vec2(20.0, 22.0));
    let add = ui.interact(add_rect, egui::Id::unique(("animation-add", &lane.label)), Sense::click()).on_hover_text(tl!("Add keyframe at playhead"));
    crate::icons::paint(&painter, add_rect.shrink(4.0), crate::icons::Icon::Plus, t.text_dim);
    app.auto.add(&format!("{lane_id}.add"), add_rect, "Add keyframe");
    if add.clicked() && app.session.playhead() >= lane.clip.start && app.session.playhead() < lane.clip.end() {
        let key = KeyframeRef { time: lane.clip.effect_time_at(app.session.playhead()), ..lane.key.clone() };
        actions.push(("effects.editKeyframes", json!({"edits":[{"key":key,"operation":"put"}]})));
    }
    let x_of = |time: Tick| graph.min.x + ((time.seconds() - scroll) * pps) as f32;
    let tick_at = |x: f32| Tick::from_seconds_f64(scroll + (x - graph.min.x) as f64 / pps);
    let gp = painter.with_clip_rect(graph.intersect(ui.clip_rect()));
    gp.rect_filled(graph, 2.0, t.panel_bg);
    // Time grid is shared between rows and stays aligned across clips.
    let step = (80.0 / pps).max(0.01);
    for i in 0..=((graph.width() / 80.0).ceil() as usize).min(64) {
        let time = ((scroll / step).floor() + i as f64) * step;
        let x = x_of(Tick::from_seconds_f64(time));
        gp.line_segment([pos2(x, graph.min.y), pos2(x, graph.max.y)], Stroke::new(1.0, t.separator));
        if curves {
            gp.text(pos2(x + 2.0, graph.min.y), Align2::LEFT_TOP, format!("{time:.2}s"), Tokens::ui(9.0), t.text_dim);
        }
    }
    let mut param = lane.param.clone();
    if let Some((key, p)) = &app.ui.animation.preview
        && key.clip == lane.key.clip
        && key.effect == lane.key.effect
        && key.param == lane.key.param
        && key.mask == lane.key.mask
        && let Some(k) = param.keyframes.iter_mut().find(|k| k.time == key.time)
    {
        apply_preview(k, p);
    }
    let values: Vec<f64> = param.keyframes.iter().flat_map(|k| components(&k.value)).filter(|v| v.is_finite() && v.abs() <= 1e12).collect();
    let low = values.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let high = values.iter().copied().reduce(f64::max).unwrap_or(1.0);
    let range = (high - low).max(1.0);
    let min = low - range * 0.2;
    let max = high + range * 0.2;
    let y_of = |v: f64| graph.max.y - 8.0 - ((v - min) / (max - min)) as f32 * (graph.height() - 24.0);
    let value_at_y = |y: f32| min + (graph.max.y - 8.0 - y) as f64 / (graph.height() - 24.0).max(1.0) as f64 * (max - min);
    let colors = [t.accent, Color32::from_rgb(90, 200, 125), Color32::from_rgb(240, 150, 80), Color32::from_rgb(200, 120, 225)];
    if curves && graphable {
        painter.text(pos2(graph.min.x - 6.0, graph.min.y + 20.0), Align2::RIGHT_TOP, format!("{max:.2}"), Tokens::ui(9.0), t.text_dim);
        painter.text(pos2(graph.min.x - 6.0, graph.max.y - 6.0), Align2::RIGHT_BOTTOM, format!("{min:.2}"), Tokens::ui(9.0), t.text_dim);
    }
    if curves && graphable {
        let count = (graph.width().ceil() as usize).clamp(2, 512);
        for (ci, color) in colors.iter().enumerate().take(components(&param.value).len()) {
            if !app.ui.animation.components[ci] {
                continue;
            }
            let points: Vec<_> = (0..=count)
                .filter_map(|i| {
                    let x = graph.min.x + graph.width() * i as f32 / count as f32;
                    let time = tick_at(x);
                    if time < lane.clip.start || time >= lane.clip.end() {
                        return None;
                    }
                    let c = components(&param.value_at(lane.clip.effect_time_at(time)));
                    c.get(ci).copied().filter(|v| v.is_finite() && v.abs() <= 1e12).map(|v| pos2(x, y_of(v)))
                })
                .collect();
            if points.len() > 1 {
                gp.add(egui::Shape::line(points, Stroke::new(1.5, *color)));
            }
        }
    }
    let scale_lane = Rect::from_min_size(graph.min, vec2((lane.clip.duration.seconds() * pps) as f32, graph.height()));
    for (index, k) in param.keyframes.iter().enumerate() {
        if !visible_key(lane, k) {
            continue;
        }
        let key = KeyframeRef { time: k.time, ..lane.key.clone() };
        let x = x_of(timeline_time_of(&lane.clip, k.time));
        if x < graph.min.x || x > graph.max.x {
            continue;
        }
        let c = components(&k.value);
        let n = if curves && graphable { c.len() } else { 1 };
        for ci in 0..n {
            if curves && graphable && !app.ui.animation.components[ci] {
                continue;
            }
            if curves && graphable && c.get(ci).is_none_or(|v| !v.is_finite() || v.abs() > 1e12) {
                continue;
            }
            let y = if curves && graphable { y_of(*c.get(ci).unwrap_or(&0.0)) } else { graph.center().y };
            let kr = Rect::from_center_size(pos2(x, y), vec2(9.0, 9.0));
            let response = ui.interact(kr.expand(2.0), egui::Id::unique(("animation-key", curves, &lane.label, k.time.0, ci)), Sense::click_and_drag());
            if response.clicked() || response.drag_started() {
                app.ui.animation.active_key = Some(key.clone());
            }
            let kr = interaction.key(app, ui, key.clone(), &lane.clip, scale_lane, kr, &response, k.interp);
            app.auto.add(&format!("{lane_id}.key.{}.{}", key.time.0, ci), kr, "Keyframe — drag time, Shift to extend selection");
            super::effect_controls::selection::paint(&gp, kr, k.interp, if app.ui.keyframe_selection.contains(&key) { t.hot_text } else { t.accent });
            if curves && graphable && app.ui.keyframe_selection.contains(&key) {
                draw_handles(
                    app,
                    ui,
                    HandleSpace { lane, param: &param, index, component: ci, graph, x_of: &x_of, y_of: &y_of, value_at_y: &value_at_y, lane_id: &lane_id, pps },
                    actions,
                );
            }
        }
    }
    let ph = x_of(app.session.playhead());
    gp.line_segment([pos2(ph, graph.min.y), pos2(ph, graph.max.y)], Stroke::new(1.0, t.playhead));
    let ruler = Rect::from_min_max(graph.min, pos2(graph.max.x, graph.min.y + 12.0));
    let seek = ui.interact(ruler, egui::Id::unique(("animation-seek", &lane.label)), Sense::click());
    app.auto.add(&format!("{lane_id}.ruler"), ruler, "Animation time ruler");
    if seek.clicked()
        && let Some(p) = seek.interact_pointer_pos()
    {
        actions.push(("playhead.set", json!({"time":app.session.sequence_rate().snap_nearest(tick_at(p.x).max(Tick::ZERO)).0})));
    }
}

fn apply_preview(k: &mut Keyframe, p: &Value) {
    if let Some(v) = p.get("inTangents").and_then(Value::as_array) {
        k.in_tangents = v.iter().filter_map(Value::as_f64).collect();
    }
    if let Some(v) = p.get("outTangents").and_then(Value::as_array) {
        k.out_tangents = v.iter().filter_map(Value::as_f64).collect();
    }
    if let Some(v) = p.get("inInfluence").and_then(Value::as_f64) {
        k.in_influence = v;
    }
    if let Some(v) = p.get("outInfluence").and_then(Value::as_f64) {
        k.out_influence = v;
    }
    k.interp = filmcraft_project::Interpolation::Bezier;
}

struct HandleSpace<'a> {
    lane: &'a Lane,
    param: &'a Param,
    index: usize,
    component: usize,
    graph: Rect,
    x_of: &'a dyn Fn(Tick) -> f32,
    y_of: &'a dyn Fn(f64) -> f32,
    value_at_y: &'a dyn Fn(f32) -> f64,
    lane_id: &'a str,
    pps: f64,
}
fn draw_handles(app: &mut FilmcraftApp, ui: &mut egui::Ui, space: HandleSpace<'_>, actions: &mut Vec<(&'static str, Value)>) {
    let HandleSpace { lane, param, index, component: ci, graph, x_of, y_of, value_at_y, lane_id, pps } = space;
    let Some(k) = param.keyframes.get(index) else { return };
    let c = components(&k.value);
    let Some(value) = c.get(ci) else { return };
    let key = KeyframeRef { time: k.time, ..lane.key.clone() };
    let x = x_of(timeline_time_of(&lane.clip, k.time));
    let y = y_of(*value);
    let gp = ui.painter().with_clip_rect(graph.intersect(ui.clip_rect()));
    let t = app.tokens;
    let colors = [t.accent, Color32::from_rgb(90, 200, 125), Color32::from_rgb(240, 150, 80), Color32::from_rgb(200, 120, 225)];
    for outgoing in [false, true] {
        let neighbor = if outgoing { param.keyframes.get(index + 1) } else { index.checked_sub(1).and_then(|i| param.keyframes.get(i)) };
        let Some(nb) = neighbor else { continue };
        let Some(delta) = nb.time.0.checked_sub(k.time.0) else { continue };
        let seconds = Tick(delta).seconds();
        if seconds == 0.0 {
            continue;
        }
        let influence = if outgoing { k.out_influence } else { k.in_influence };
        let influence = if influence.is_finite() { influence.clamp(0.01, 1.0) } else { 1.0 / 3.0 };
        let slopes = if outgoing { &k.out_tangents } else { &k.in_tangents };
        let slope = param.tangent_at(index, ci, outgoing).unwrap_or(0.0);
        let hv = c[ci] + slope * seconds * influence;
        let distance = (seconds / lane.clip.speed.abs() * if lane.clip.reverse { -pps } else { pps }) as f32;
        if !distance.is_finite() {
            continue;
        }
        let hx = x + distance * influence as f32;
        let hp = pos2(hx, y_of(hv));
        gp.line_segment([pos2(x, y), hp], Stroke::new(1.0, t.text_dim));
        gp.circle_filled(hp, 3.5, colors[ci]);
        let hr = Rect::from_center_size(hp, vec2(10.0, 10.0));
        let response = ui.interact(hr, egui::Id::unique(("animation-handle", &lane.label, k.time.0, ci, outgoing)), Sense::drag());
        if response.is_pointer_button_down_on() {
            app.ui.animation.active_key = Some(key.clone());
        }
        app.auto.add(&format!("{lane_id}.handle.{}.{}.{outgoing}", key.time.0, ci), hr, "Bézier handle — drag time and slope");
        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let influence = if distance.abs() > 0.01 { ((pointer.x - x) / distance).clamp(0.01, 1.0) as f64 } else { 1.0 / 3.0 };
            let mut slopes = slopes.clone();
            slopes.resize(c.len(), 0.0);
            slopes[ci] = ((value_at_y(pointer.y) - c[ci]) / (seconds * influence)).clamp(-1e12, 1e12);
            let mut p = json!({"key":key,"interpolation":"bezier"});
            p[if outgoing { "outInfluence" } else { "inInfluence" }] = json!(influence);
            p[if outgoing { "outTangents" } else { "inTangents" }] = json!(slopes);
            if k.tangents_linked {
                p[if outgoing { "inTangents" } else { "outTangents" }] = json!(slopes);
            }
            app.ui.animation.preview = Some((key.clone(), p));
        }
        if response.drag_stopped()
            && let Some((_, p)) = app.ui.animation.preview.take()
            && !ui.input(|i| i.key_pressed(egui::Key::Escape))
        {
            actions.push(("effects.editKeyframes", json!({"edits":[p]})));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn height_zoom_rejects_hostile_values_without_changing_view() {
        let mut view = super::AnimationUi::default();
        for value in [serde_json::json!(-1), serde_json::json!(0), serde_json::json!(5), serde_json::json!("large"), serde_json::Value::Null] {
            assert!(view.update(&serde_json::json!({"pps":160,"heightZoom":value})).is_err());
            assert_eq!(view.pps, 80.0);
            assert_eq!(view.height_zoom, 1.0);
        }
        view.update(&serde_json::json!({"heightZoom":4})).unwrap();
        assert_eq!(view.height_zoom, 4.0);
    }
    use super::*;
    fn fixture() -> FilmcraftApp {
        let mut session = filmcraft_engine::Session::default();
        session.execute("file.openDemoProject", json!({})).unwrap();
        let clip = session.active_sequence().unwrap().video_tracks[0].items[0].id;
        session.state.selection = vec![clip];
        session
            .edit_sequence("Trimmed animation", |q, _, _| {
                let it = q.find_item_mut(clip).unwrap().1;
                it.source_in = Tick::from_seconds_f64(1.0);
                it.duration = Tick::from_seconds_f64(2.0);
                it.speed = 1.0;
                it.reverse = false;
                let param = it.effects.iter_mut().find(|fx| fx.effect == "motion").unwrap().params.get_mut("scale").unwrap();
                param.put_keyframe(Tick::ZERO, ParamValue::Float(0.0));
                param.put_keyframe(Tick::from_seconds_f64(5.0), ParamValue::Float(100.0));
                Ok(())
            })
            .unwrap();
        FilmcraftApp::new(session)
    }
    #[test]
    fn trimmed_animation_keeps_offscreen_keys_for_evaluation() {
        let app = fixture();
        let all = lanes(&app);
        let lane = all.iter().find(|l| l.key.param == "scale").unwrap();
        assert_eq!(lane.param.keyframes.len(), 2);
        assert_eq!(lane.param.f64_at(Tick::from_seconds_f64(2.0)), 40.0);
        assert!(lane.param.keyframes.iter().all(|k| !visible_key(lane, k)));
    }
    #[test]
    fn corrupt_animation_times_and_view_state_do_not_panic() {
        let mut app = fixture();
        let clip = app.session.state.selection[0];
        let seq = app.session.state.active_sequence.unwrap();
        let project = std::sync::Arc::make_mut(&mut app.session.project);
        let param = project
            .sequence_mut(seq)
            .unwrap()
            .find_item_mut(clip)
            .unwrap()
            .1
            .effects
            .iter_mut()
            .find(|fx| fx.effect == "motion")
            .unwrap()
            .params
            .get_mut("scale")
            .unwrap();
        param.keyframes[0].time = Tick::MIN;
        param.keyframes[1].time = Tick::MAX;
        app.ui.animation.pps = f64::NAN;
        app.ui.animation.scroll = f64::INFINITY;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx, &app.tokens);
        for curves in [false, true] {
            let mut out = ctx.run_ui(Default::default(), |ui| show(&mut app, ui, Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0)), curves));
            out.textures_delta.clear();
        }
        assert!(app.ui.animation.pps.is_finite() && app.ui.animation.scroll.is_finite());
    }
}
