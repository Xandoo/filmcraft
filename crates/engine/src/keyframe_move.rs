//! Atomic edits of keys across effect/mask parameters: movement, where the common timeline delta
//! keeps relative timing intact, including at the clip boundaries, and interpolation. Selection
//! itself is a UI concern.
use std::collections::{BTreeMap, HashSet};

use filmcraft_project::{ClipId, Keyframe, Param, Sequence, TrackItem};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, Session, commands::bad};

const COMMAND: &str = "effects.moveKeyframes";
const MAX_KEYS: usize = 4096;

pub(crate) fn set_fields(k: &mut Keyframe, p: &Value) -> Result<()> {
    const COMMAND: &str = "effects.editKeyframes";
    if let Some(value) = p.get("value") {
        k.value = crate::commands::json_to_param(&k.value, value).ok_or_else(|| bad(COMMAND, "value has the wrong type"))?;
    }
    let components = match k.value {
        filmcraft_project::ParamValue::Float(_) => 1,
        filmcraft_project::ParamValue::Vec2(_) => 2,
        filmcraft_project::ParamValue::Color(_) => 4,
        _ => 0,
    };
    for (name, target) in [("inTangents", &mut k.in_tangents), ("outTangents", &mut k.out_tangents)] {
        if let Some(raw) = p.get(name) {
            let values = raw.as_array().ok_or_else(|| bad(COMMAND, "tangents must be arrays"))?;
            if !values.is_empty() && (values.len() != components || components == 0) {
                return Err(bad(COMMAND, "tangent component count does not match property"));
            }
            let slopes: Result<Vec<f64>> =
                values.iter().map(|v| v.as_f64().filter(|v| v.is_finite() && v.abs() <= 1e12).ok_or_else(|| bad(COMMAND, "invalid tangent slope"))).collect();
            *target = slopes?;
        }
    }
    for (name, target) in [("inInfluence", &mut k.in_influence), ("outInfluence", &mut k.out_influence)] {
        if let Some(raw) = p.get(name) {
            *target = raw.as_f64().filter(|v| v.is_finite() && (0.01..=1.0).contains(v)).ok_or_else(|| bad(COMMAND, "influence must be between 0.01 and 1"))?;
        }
    }
    if let Some(raw) = p.get("tangentsLinked") {
        k.tangents_linked = raw.as_bool().ok_or_else(|| bad(COMMAND, "tangentsLinked must be boolean"))?;
    }
    if p.get("interpolation").is_some() {
        k.interp = crate::commands::interpolation_p(p);
        if matches!(k.interp, filmcraft_project::Interpolation::Linear | filmcraft_project::Interpolation::Hold) {
            k.in_tangents.clear();
            k.out_tangents.clear();
        }
    }
    if k.tangents_linked && ["inTangents", "outTangents", "tangentsLinked"].iter().any(|name| p.get(name).is_some()) {
        let slopes = if p.get("inTangents").is_some() && p.get("outTangents").is_none() {
            k.in_tangents.clone()
        } else if !k.out_tangents.is_empty() {
            k.out_tangents.clone()
        } else {
            k.in_tangents.clone()
        };
        k.in_tangents = slopes.clone();
        k.out_tangents = slopes;
    }
    for tangents in [&k.in_tangents, &k.out_tangents] {
        if !tangents.is_empty() && (tangents.len() != components || tangents.iter().any(|v| !v.is_finite() || v.abs() > 1e12)) {
            return Err(bad(COMMAND, "invalid tangent components"));
        }
    }
    let valid_value = match &k.value {
        filmcraft_project::ParamValue::Float(v) => v.is_finite(),
        filmcraft_project::ParamValue::Color(v) => v.iter().all(|v| v.is_finite()),
        filmcraft_project::ParamValue::Vec2(v) => !v.x.is_infinite() && !v.y.is_infinite(),
        _ => true,
    };
    if !valid_value {
        return Err(bad(COMMAND, "keyframe value must be finite (point coordinates may be Auto)"));
    }
    Ok(())
}

/// Mixed additions, deletions and component edits are one validated transaction.
pub(crate) fn edit_many(s: &mut Session, p: &Value) -> Result<Value> {
    const COMMAND: &str = "effects.editKeyframes";
    let raw = p
        .get("edits")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty() && v.len() <= MAX_KEYS)
        .ok_or_else(|| bad(COMMAND, "need between 1 and 4096 edits"))?;
    let mut edits = Vec::with_capacity(raw.len());
    let mut seen = HashSet::new();
    for entry in raw {
        let key: Key = serde_json::from_value(entry.get("key").cloned().ok_or_else(|| bad(COMMAND, "need key"))?).map_err(|_| bad(COMMAND, "invalid key"))?;
        if key.param.len() > 256 || !seen.insert(key.clone()) {
            return Err(bad(COMMAND, "duplicate or invalid key"));
        }
        edits.push((key, entry.clone()));
    }
    s.edit_sequence("Edit Keyframes", |q, _, _| {
        for (key, entry) in &edits {
            let param = param_mut(q, &(key.clip, key.effect, key.mask, key.param.clone()), COMMAND)?;
            match entry.get("operation").and_then(Value::as_str).unwrap_or("set") {
                "delete" => {
                    if !param.remove_keyframe_at(key.time) {
                        return Err(bad(COMMAND, "keyframe does not exist"));
                    }
                }
                "put" => {
                    let mut k = match entry.get("keyframe") {
                        Some(v) => serde_json::from_value::<Keyframe>(v.clone()).map_err(|_| bad(COMMAND, "invalid keyframe"))?,
                        None => Keyframe::new(key.time, param.value_at(key.time)),
                    };
                    if std::mem::discriminant(&k.value) != std::mem::discriminant(&param.value) {
                        return Err(bad(COMMAND, "keyframe value has the wrong type"));
                    }
                    k.time = key.time;
                    set_fields(&mut k, entry)?;
                    param.remove_keyframe_at(key.time);
                    let index = param.keyframes.partition_point(|v| v.time < key.time);
                    param.keyframes.insert(index, k);
                }
                "set" => {
                    let k = param.keyframes.iter_mut().find(|k| k.time == key.time).ok_or_else(|| bad(COMMAND, "keyframe does not exist"))?;
                    set_fields(k, entry)?;
                }
                _ => return Err(bad(COMMAND, "unknown keyframe operation")),
            }
        }
        Ok(())
    })?;
    Ok(json!({"edited": edits.len()}))
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
struct Key {
    clip: u64,
    effect: usize,
    param: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mask: Option<usize>,
    time: Tick,
}

type Lane = (u64, usize, Option<usize>, String);

fn param_mut<'a>(sequence: &'a mut Sequence, lane: &Lane, command: &str) -> Result<&'a mut Param> {
    let (_, it) = sequence.find_item_mut(ClipId(lane.0)).ok_or_else(|| bad(command, "clip does not exist"))?;
    let effect = it.effects.get_mut(lane.1).ok_or_else(|| bad(command, "effect does not exist"))?;
    match lane.2 {
        Some(mask) => effect.masks.get_mut(mask).and_then(|m| m.param_mut(&lane.3)),
        None => effect.params.get_mut(&lane.3),
    }
    .ok_or_else(|| bad(command, "parameter does not exist"))
}

fn direction(it: &TrackItem) -> f64 {
    it.speed.abs() * if it.reverse { -1.0 } else { 1.0 }
}

pub(crate) fn move_many(s: &mut Session, p: &Value) -> Result<Value> {
    let raw = p.get("keyframes").and_then(Value::as_array).ok_or_else(|| bad(COMMAND, "need keyframes"))?;
    if raw.is_empty() || raw.len() > MAX_KEYS {
        return Err(bad(COMMAND, "keyframe count must be between 1 and 4096"));
    }
    let requested = p.get("delta").and_then(Value::as_i64).ok_or_else(|| bad(COMMAND, "need an integer timeline delta"))?;
    let mut keys: Vec<Key> = serde_json::from_value(Value::Array(raw.clone())).map_err(|_| bad(COMMAND, "invalid keyframe reference"))?;
    let mut unique = HashSet::new();
    let mut saved = Vec::with_capacity(keys.len());
    let q = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let frame = q.settings.frame_rate.frame_duration();
    let (mut low, mut high) = (i64::MIN, i64::MAX);
    for key in &keys {
        if key.param.len() > 256 || !unique.insert(key.clone()) {
            return Err(bad(COMMAND, "duplicate or invalid keyframe reference"));
        }
        let (_, it) = q.find_item(ClipId(key.clip)).ok_or_else(|| bad(COMMAND, "clip does not exist"))?;
        let effect = it.effects.get(key.effect).ok_or_else(|| bad(COMMAND, "effect does not exist"))?;
        let param = match key.mask {
            Some(mask) => effect.masks.get(mask).and_then(|m| m.param(&key.param)),
            None => effect.params.get(&key.param),
        }
        .ok_or_else(|| bad(COMMAND, "parameter does not exist"))?;
        let k = param.keyframes.iter().find(|k| k.time == key.time).ok_or_else(|| bad(COMMAND, "keyframe does not exist"))?;
        let speed = direction(it);
        if !speed.is_finite() || speed == 0.0 || it.duration < frame {
            return Err(bad(COMMAND, "clip has no movable keyframe range"));
        }
        let a = it.moving_source_time_at(it.start);
        let b = it.moving_source_time_at(it.end() - frame);
        if key.time < a.min(b) || key.time > a.max(b) {
            return Err(bad(COMMAND, "keyframe is outside the visible clip range"));
        }
        let da = (a - key.time).0 as f64 / speed;
        let db = (b - key.time).0 as f64 / speed;
        low = low.max(da.min(db).ceil() as i64);
        high = high.min(da.max(db).floor() as i64);
        saved.push((k.clone(), speed));
    }
    if low > high {
        return Err(bad(COMMAND, "keyframes have no common movement range"));
    }
    let delta = requested.clamp(low, high);
    if delta == 0 {
        return Ok(json!({"delta":0,"keyframes":keys}));
    }
    let mut lanes: BTreeMap<Lane, Vec<(Tick, Keyframe)>> = BTreeMap::new();
    for (key, (mut k, speed)) in keys.iter_mut().zip(saved) {
        let from = key.time;
        k.time += Tick((delta as f64 * speed).round() as i64);
        key.time = k.time;
        lanes.entry((key.clip, key.effect, key.mask, key.param.clone())).or_default().push((from, k));
    }
    s.edit_sequence("Move Keyframes", |sequence, _, _| {
        for (lane, moved) in lanes {
            let param = param_mut(sequence, &lane, COMMAND)?;
            let from: HashSet<_> = moved.iter().map(|(t, _)| *t).collect();
            let to: HashSet<_> = moved.iter().map(|(_, k)| k.time).collect();
            // Remove all originals before inserting any destination: selected keys can move into
            // each other's old positions without overwriting a key still waiting to be moved.
            param.keyframes.retain(|k| !from.contains(&k.time) && !to.contains(&k.time));
            param.keyframes.extend(moved.into_iter().map(|(_, k)| k));
            param.keyframes.sort_by_key(|k| k.time);
        }
        Ok(())
    })?;
    Ok(json!({"delta":delta,"keyframes":keys}))
}

/// `effects.setInterpolation` given a `keyframes` list: every listed key changes in one undo step,
/// so a menu applies to the whole selection (#1916).
pub(crate) fn interpolate_many(s: &mut Session, p: &Value) -> Result<Value> {
    const SET: &str = "effects.setInterpolation";
    let raw = p.get("keyframes").and_then(Value::as_array).ok_or_else(|| bad(SET, "need keyframes"))?;
    if raw.is_empty() || raw.len() > MAX_KEYS {
        return Err(bad(SET, "keyframe count must be between 1 and 4096"));
    }
    let keys: Vec<Key> = serde_json::from_value(Value::Array(raw.clone())).map_err(|_| bad(SET, "invalid keyframe reference"))?;
    let kind = crate::commands::interpolation_p(p);
    s.edit_sequence("Keyframe Interpolation", |sequence, _, _| {
        for key in &keys {
            let lane = (key.clip, key.effect, key.mask, key.param.clone());
            let param = param_mut(sequence, &lane, SET)?;
            let index = param.keyframes.iter().position(|k| k.time == key.time).ok_or_else(|| bad(SET, "keyframe does not exist"))?;
            let slopes = if kind == filmcraft_project::Interpolation::AutoBezier {
                let a = param.keyframes.get(index.saturating_sub(1));
                let b = param.keyframes.get((index + 1).min(param.keyframes.len().saturating_sub(1)));
                match (a, b) {
                    (Some(a), Some(b)) if b.time > a.time => {
                        let components = |v: &filmcraft_project::ParamValue| match v {
                            filmcraft_project::ParamValue::Float(v) => vec![*v],
                            filmcraft_project::ParamValue::Vec2(v) => vec![v.x, v.y],
                            filmcraft_project::ParamValue::Color(v) => v.iter().map(|v| *v as f64).collect(),
                            _ => Vec::new(),
                        };
                        let seconds = (b.time - a.time).seconds();
                        components(&a.value)
                            .iter()
                            .zip(components(&b.value))
                            .map(|(a, b)| {
                                let slope = (b - a) / seconds;
                                if slope.is_finite() { slope.clamp(-1e12, 1e12) } else { 0.0 }
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let k = param.keyframes.get_mut(index).ok_or_else(|| bad(SET, "keyframe does not exist"))?;
            k.interp = kind;
            k.in_tangents = slopes.clone();
            k.out_tangents = slopes;
        }
        Ok(())
    })?;
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::Interpolation;
    use std::sync::Arc;

    fn fixture() -> (Session, Vec<Value>) {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
        let mut keys = Vec::new();
        for (param, seconds) in [("scale", 0.5), ("scale", 1.0), ("rotation", 1.5)] {
            s.execute("playhead.set", json!({"seconds":seconds})).unwrap();
            s.execute("effects.addKeyframe", json!({"clip":clip.0,"effect":"motion","param":param})).unwrap();
            let it = s.active_sequence().unwrap().find_item(clip).unwrap().1;
            let effect = it.effects.iter().position(|e| e.effect == "motion").unwrap();
            keys.push(json!({"clip":clip.0,"effect":effect,"param":param,"time":it.source_time_at(s.playhead()).0}));
        }
        (s, keys)
    }

    fn values(s: &Session, key: &Value) -> Vec<Keyframe> {
        let it = s.active_sequence().unwrap().find_item(ClipId(key["clip"].as_u64().unwrap())).unwrap().1;
        it.effects[key["effect"].as_u64().unwrap() as usize].params[key["param"].as_str().unwrap()].keyframes.clone()
    }

    #[test]
    fn batch_tangent_edits_are_atomic_and_reject_hostile_values() {
        let (mut s, keys) = fixture();
        let before = s.project.to_json();
        let undo = s.history.undo.len();
        let invalid = json!({"clip":u64::MAX,"effect":0,"param":"scale","time":0});
        for edits in [
            json!([{"key":keys[0],"outTangents":[1e30]}]),
            json!([{"key":keys[0],"outTangents":[1,2]}]),
            json!([{"key":keys[0],"outInfluence":0}]),
            json!([{"key":keys[0],"outTangents":[1]},{"key":invalid,"value":2}]),
        ] {
            assert!(s.execute("effects.editKeyframes", json!({"edits":edits})).is_err());
            assert_eq!(s.project.to_json(), before);
            assert_eq!(s.history.undo.len(), undo);
        }
        s.execute("effects.editKeyframes", json!({"edits":[{"key":keys[0],"outTangents":[7.0],"inTangents":[7.0],"interpolation":"bezier"}]})).unwrap();
        assert_eq!(values(&s, &keys[0])[0].out_tangents, vec![7.0]);
        s.undo();
        assert_eq!(s.project.to_json(), before);
    }

    #[test]
    fn linked_tangent_edits_share_slopes_and_broken_tangents_keep_corners() {
        let mut key = Keyframe::new(Tick::ZERO, filmcraft_project::ParamValue::Float(0.0));
        set_fields(&mut key, &json!({"outTangents":[3.0]})).unwrap();
        assert_eq!(key.in_tangents, vec![3.0]);
        set_fields(&mut key, &json!({"tangentsLinked":false,"inTangents":[-2.0]})).unwrap();
        assert_eq!(key.in_tangents, vec![-2.0]);
        assert_eq!(key.out_tangents, vec![3.0]);
        set_fields(&mut key, &json!({"tangentsLinked":true})).unwrap();
        assert_eq!(key.in_tangents, key.out_tangents);
    }

    #[test]
    fn automatic_point_coordinates_never_create_unsaveable_tangents() {
        let (mut s, keys) = fixture();
        let clip = ClipId(keys[0]["clip"].as_u64().unwrap());
        let effect = keys[0]["effect"].as_u64().unwrap() as usize;
        let second = Tick::from_seconds_f64(1.0);
        s.edit_sequence("Auto coordinates", |q, _, _| {
            let p = q.find_item_mut(clip).unwrap().1.effects[effect].params.get_mut("position").unwrap();
            p.keyframes = [Tick::ZERO, second]
                .into_iter()
                .map(|time| Keyframe::new(time, filmcraft_project::ParamValue::Vec2(filmcraft_geom::Vec2::new(f64::NAN, f64::NAN))))
                .collect();
            Ok(())
        })
        .unwrap();
        let keys: Vec<Value> = [Tick::ZERO, second].into_iter().map(|time| json!({"clip":clip.0,"effect":effect,"param":"position","time":time.0})).collect();
        s.execute("effects.setInterpolation", json!({"keyframes":keys,"interpolation":"autoBezier"})).unwrap();
        for key in &s.active_sequence().unwrap().find_item(clip).unwrap().1.effects[effect].params["position"].keyframes {
            assert!(key.in_tangents.iter().chain(&key.out_tangents).all(|slope| slope.is_finite()));
            let _: Keyframe = serde_json::from_value(serde_json::to_value(key).unwrap()).unwrap();
        }
    }

    #[test]
    fn movement_across_lanes_is_atomic_and_one_undo_step() {
        let (mut s, keys) = fixture();
        let project = s.project.clone();
        let before = values(&s, &keys[0]);
        let undo = s.history.undo.len();
        // Scale's first key moves into the second one's old position. Both must survive.
        let delta = before[1].time - before[0].time;
        let result = s.execute(COMMAND, json!({"keyframes":keys,"delta":delta.0})).unwrap();
        let after = values(&s, &keys[0]);
        assert_eq!(after.len(), 2);
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(new.time, old.time + delta);
            assert_eq!(serde_json::to_value(&new.value).unwrap(), serde_json::to_value(&old.value).unwrap());
            assert_eq!(new.interp, old.interp);
            assert_eq!(new.in_influence, old.in_influence);
            assert_eq!(new.out_influence, old.out_influence);
        }
        assert_eq!(result["keyframes"][2]["time"].as_i64().unwrap(), keys[2]["time"].as_i64().unwrap() + delta.0);
        assert_eq!(s.history.undo.len(), undo + 1);
        let moved = serde_json::to_value(&s.project).unwrap();
        s.undo();
        assert_eq!(serde_json::to_value(&s.project).unwrap(), serde_json::to_value(project).unwrap());
        s.redo();
        assert_eq!(serde_json::to_value(&s.project).unwrap(), moved);
    }

    #[test]
    fn bounds_clamp_the_group_without_changing_its_spacing() {
        let (mut s, keys) = fixture();
        let undo = s.history.undo.len();
        let result = s.execute(COMMAND, json!({"keyframes":keys,"delta":i64::MAX})).unwrap();
        let delta = result["delta"].as_i64().unwrap();
        assert!(delta > 0 && delta < i64::MAX);
        for (old, new) in keys.iter().zip(result["keyframes"].as_array().unwrap()) {
            assert_eq!(new["time"].as_i64().unwrap() - old["time"].as_i64().unwrap(), delta);
        }
        assert_eq!(s.history.undo.len(), undo + 1);
        s.execute(COMMAND, json!({"keyframes":result["keyframes"],"delta":i64::MAX})).unwrap();
        assert_eq!(s.history.undo.len(), undo + 1, "a boundary no-op creates no edit");
    }

    #[test]
    fn invalid_references_and_hostile_requests_cannot_make_partial_edits() {
        let (mut s, keys) = fixture();
        let before = s.project.clone();
        let undo = s.history.undo.len();
        let mut invalid = keys[1].clone();
        invalid["param"] = json!("missing");
        for request in [
            json!({"keyframes":[keys[0],invalid],"delta":100}),
            json!({"keyframes":[keys[0],keys[0]],"delta":100}),
            json!({"keyframes":[],"delta":100}),
            json!({"keyframes":vec![keys[0].clone();MAX_KEYS+1],"delta":100}),
            json!({"keyframes":[{"clip":keys[0]["clip"],"effect":u64::MAX,"param":"scale","time":0}],"delta":i64::MIN}),
            json!({"keyframes":keys,"delta":"fast"}),
        ] {
            assert!(s.execute(COMMAND, request).is_err());
            assert!(Arc::ptr_eq(&before, &s.project));
            assert_eq!(s.history.undo.len(), undo);
        }
    }

    #[test]
    fn interpolation_of_listed_keys_across_lanes_is_one_undo_step() {
        const SET: &str = "effects.setInterpolation";
        let (mut s, keys) = fixture();
        let project = s.project.clone();
        let undo = s.history.undo.len();
        s.execute(SET, json!({"keyframes":[keys[0],keys[2]],"interpolation":"hold"})).unwrap();
        let interps = |s: &Session, key: &Value| values(s, key).iter().map(|k| k.interp).collect::<Vec<_>>();
        assert_eq!(interps(&s, &keys[0]), [Interpolation::Hold, Interpolation::Linear], "the unlisted Scale key stays linear");
        assert_eq!(interps(&s, &keys[2]), [Interpolation::Hold]);
        assert_eq!(s.history.undo.len(), undo + 1);
        s.undo();
        assert_eq!(serde_json::to_value(&s.project).unwrap(), serde_json::to_value(&project).unwrap());

        let before = s.project.clone();
        let mut invalid = keys[1].clone();
        invalid["time"] = json!(invalid["time"].as_i64().unwrap() + 1);
        for request in [
            json!({"keyframes":[keys[0],invalid],"interpolation":"hold"}),
            json!({"keyframes":[],"interpolation":"hold"}),
            json!({"keyframes":vec![keys[0].clone();MAX_KEYS+1],"interpolation":"hold"}),
            json!({"keyframes":[{"clip":keys[0]["clip"],"effect":u64::MAX,"param":"scale","time":0}],"interpolation":"hold"}),
            json!({"keyframes":[{"clip":keys[0]["clip"],"effect":0,"param":"scale","mask":3,"time":0}],"interpolation":"hold"}),
            json!({"keyframes":"all","interpolation":"hold"}),
        ] {
            assert!(s.execute(SET, request).is_err());
            assert!(Arc::ptr_eq(&before, &s.project));
        }
    }

    #[test]
    fn media_times_follow_clip_speed_and_reverse() {
        for reverse in [false, true] {
            let (mut s, keys) = fixture();
            let clip = ClipId(keys[0]["clip"].as_u64().unwrap());
            s.edit_sequence("test speed", |q, _, _| {
                let it = q.find_item_mut(clip).unwrap().1;
                it.speed = 2.0;
                it.reverse = reverse;
                Ok(())
            })
            .unwrap();
            let delta = Tick::from_seconds_f64(0.1);
            let result = s.execute(COMMAND, json!({"keyframes":keys,"delta":delta.0})).unwrap();
            for (old, new) in keys.iter().zip(result["keyframes"].as_array().unwrap()) {
                let diff = new["time"].as_i64().unwrap() - old["time"].as_i64().unwrap();
                assert_eq!(diff, delta.0 * 2 * if reverse { -1 } else { 1 });
            }
        }
    }
}
