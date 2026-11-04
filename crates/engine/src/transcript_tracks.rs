//! Editable graphics from timed transcript words. No recognition is needed to regroup text.
use crate::{EngineError, Result, Session, commands::bad};
use filmcraft_edit::transcript::{SeqWord, sequence_words};
use filmcraft_project::{ClipId, ItemId, ParamValue, Project, Track, TrackId, TrackKind, graphic};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Settings shared by transcription and Create Text Clips. Character limits include spaces;
/// a word longer than the limit stays intact. Times are always taken from the source words.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct TextTrackOptions {
    pub grouping: String,
    pub max_chars: usize,
    pub max_words: usize,
    pub max_seconds: f64,
    pub pause_seconds: f64,
    pub break_sentences: bool,
    pub font_size: f64,
    pub name: String,
}
impl Default for TextTrackOptions {
    fn default() -> Self {
        Self {
            grouping: "characters".into(),
            max_chars: 32,
            max_words: 1,
            max_seconds: 7.0,
            pause_seconds: 0.8,
            break_sentences: true,
            font_size: 72.0,
            name: "Transcript Text".into(),
        }
    }
}
impl TextTrackOptions {
    pub fn from_params(value: &Value) -> Result<Self> {
        let options: Self = serde_json::from_value(value.clone()).map_err(|e| bad("transcript.createText", e.to_string()))?;
        options.validate()?;
        Ok(options)
    }
    fn validate(&self) -> Result<()> {
        if !["characters", "words"].contains(&self.grouping.as_str())
            || !(1..=4096).contains(&self.max_chars)
            || !(1..=256).contains(&self.max_words)
            || !self.max_seconds.is_finite()
            || !(0.1..=60.0).contains(&self.max_seconds)
            || !self.pause_seconds.is_finite()
            || !(0.0..=60.0).contains(&self.pause_seconds)
            || !self.font_size.is_finite()
            || !(1.0..=1000.0).contains(&self.font_size)
            || self.name.is_empty()
            || self.name.len() > 256
        {
            return Err(bad(
                "transcript.createText",
                "invalid text settings: choose characters (1–4096) or words (1–256), duration 0.1–60 s, pause 0–60 s, font size 1–1000 and a track name",
            ));
        }
        Ok(())
    }
}

struct Block {
    start: Tick,
    end: Tick,
    text: String,
}

fn blocks(words: &[SeqWord], options: &TextTrackOptions, rate: FrameRate) -> Result<Vec<Block>> {
    options.validate()?;
    if words.len() > 100_000 || words.iter().any(|w| w.text.len() > 32768 || w.start < Tick::ZERO || w.end <= w.start) {
        return Err(bad("transcript.createText", "text output exceeds limits or has invalid word times"));
    }
    if words.iter().fold(0usize, |n, w| n.saturating_add(w.text.len())) > 16 * 1024 * 1024 {
        return Err(bad("transcript.createText", "text output exceeds 16 MiB"));
    }
    let mut output: Vec<Block> = Vec::new();
    let mut count = 0usize;
    let mut chars = 0usize;
    let max_duration = Tick::from_seconds_f64(options.max_seconds);
    let pause = Tick::from_seconds_f64(options.pause_seconds);
    for (index, word) in words.iter().enumerate() {
        let len = word.text.chars().count();
        let split = match (output.last(), index.checked_sub(1).and_then(|i| words.get(i))) {
            (Some(block), Some(previous)) => {
                let limit =
                    if options.grouping == "words" { count >= options.max_words } else { chars.saturating_add(1).saturating_add(len) > options.max_chars };
                limit
                    || word.clip != previous.clip
                    || word.speaker != previous.speaker
                    || (pause > Tick::ZERO && word.start - previous.end >= pause)
                    || word.end - block.start > max_duration
                    || (options.break_sentences && previous.text.ends_with(['.', '!', '?', '。', '！', '？']))
            }
            _ => true,
        };
        if split {
            output.push(Block { start: word.start, end: word.end, text: word.text.clone() });
            count = 1;
            chars = len;
        } else if let Some(block) = output.last_mut() {
            block.text.push(' ');
            block.text.push_str(&word.text);
            block.end = block.end.max(word.end);
            count = count.saturating_add(1);
            chars = chars.saturating_add(1).saturating_add(len);
        }
    }
    // Snap to the sequence frame grid. Never shift a group's start away from its spoken word
    // just to make it fit; report groups too dense to represent on this sequence instead.
    for block in &mut output {
        block.start = rate.snap(block.start);
        let floor = rate.snap(block.end);
        block.end = if floor < block.end {
            let next = rate.frame_at(block.end).checked_add(1).ok_or_else(|| bad("transcript.createText", "text time overflow"))?;
            let end = rate.tick_of(next);
            if end < block.end {
                return Err(bad("transcript.createText", "text time overflow"));
            }
            end
        } else {
            floor
        };
    }
    for index in 1..output.len() {
        let next = output.get(index).map(|b| b.start).ok_or_else(|| bad("transcript.createText", "missing group"))?;
        if let Some(previous) = output.get_mut(index - 1) {
            previous.end = previous.end.min(next);
        }
    }
    if output.iter().any(|b| b.end <= b.start) {
        return Err(bad("transcript.createText", "multiple text groups start within one video frame; increase the group size"));
    }
    Ok(output)
}

/// Mutates the caller's transactional project copy. The new video track is above existing video.
pub(crate) fn append(project: &mut Project, sequence: ItemId, words: &[SeqWord], options: &TextTrackOptions) -> Result<Value> {
    let q = project.sequence(sequence).ok_or(EngineError::NoSequence)?;
    let (width, height, rate) = (q.settings.width, q.settings.height, q.settings.frame_rate);
    let groups = blocks(words, options, rate)?;
    if groups.is_empty() {
        return Ok(json!({"clips":[],"count":0}));
    }
    let needed = (groups.len() as u64).checked_mul(2).and_then(|v| v.checked_add(3)).ok_or_else(|| bad("transcript.createText", "identity limit"))?;
    if project.next_id.checked_add(needed).is_none() {
        return Err(bad("transcript.createText", "identity limit"));
    }
    let source = crate::graphics::graphic_source(project, width, height, rate);
    let track_id = TrackId(project.alloc_id());
    let mut track = Track::new(track_id, TrackKind::Video, options.name.clone());
    let mut ids = Vec::new();
    for block in groups {
        let mut item = project
            .make_track_item(source, TrackKind::Video, block.start, TimeRange::new(Tick::ZERO, block.end - block.start), rate)
            .ok_or_else(|| bad("transcript.createText", "cannot create graphic source"))?;
        item.duration = block.end - block.start;
        item.name = block.text.chars().take(80).collect();
        let mut layer = graphic::new_text_layer(&block.text, filmcraft_geom::Vec2::new(width as f64 * 0.5, height as f64 * 0.85), options.font_size);
        if let Some(align) = layer.params.get_mut("align") {
            align.value = ParamValue::Choice(1);
        }
        item.effects.push(layer);
        graphic::ensure_layer_uids(&mut item.effects);
        for effect in &mut item.effects {
            filmcraft_project::resolve_auto_points(effect, (width, height), (width, height));
        }
        ids.push(item.id.0);
        track.items.push(item);
    }
    let q = project.sequence_mut(sequence).ok_or(EngineError::NoSequence)?;
    q.video_tracks.push(track);
    q.check().map_err(EngineError::Other)?;
    Ok(json!({"track":track_id.0,"count":ids.len(),"clips":ids}))
}

pub(crate) fn create(s: &mut Session, p: &Value) -> Result<Value> {
    let options = TextTrackOptions::from_params(p)?;
    let seq = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let words = sequence_words(s.active_sequence().ok_or(EngineError::NoSequence)?, &s.project.transcripts);
    if words.is_empty() {
        return Err(bad("transcript.createText", "the sequence has no transcript"));
    }
    s.edit("Create Text Clips", |project, state| {
        let result = append(project, seq, &words, &options)?;
        state.selection = result["clips"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(ClipId).collect();
        state.caption_selection.clear();
        state.graphic_layers.clear();
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(texts: &[&str]) -> Vec<SeqWord> {
        texts
            .iter()
            .enumerate()
            .map(|(i, text)| SeqWord {
                text: text.to_string(),
                start: Tick::from_seconds_f64(i as f64 * 0.5),
                end: Tick::from_seconds_f64(i as f64 * 0.5 + 0.4),
                clip: ClipId(1),
                item: ItemId(2),
                index: i,
                track: 0,
                speaker: None,
                confidence: 1.0,
            })
            .collect()
    }

    #[test]
    fn character_limits_count_unicode_and_spaces_and_keep_long_words() {
        let options = TextTrackOptions { max_chars: 5, break_sentences: false, ..Default::default() };
        let result = blocks(&words(&["éé", "字字", "lengthy", "x"]), &options, FrameRate::FPS_24).unwrap();
        assert_eq!(result.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(), ["éé 字字", "lengthy", "x"]);
        assert_eq!(result[1].start, Tick::from_seconds_f64(1.0));
    }

    #[test]
    fn word_groups_split_at_speakers_clips_pauses_sentences_and_duration() {
        let options = TextTrackOptions { grouping: "words".into(), max_words: 10, max_seconds: 1.0, ..Default::default() };
        let mut input = words(&["one", "two", "three.", "four", "five", "six", "seven"]);
        input[4].speaker = Some("Other".into());
        input[5].clip = ClipId(3);
        input[6].start = Tick::from_seconds_f64(8.0);
        input[6].end = Tick::from_seconds_f64(8.4);
        let result = blocks(&input, &options, FrameRate::FPS_24).unwrap();
        assert_eq!(result.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(), ["one two", "three.", "four", "five", "six", "seven"]);
    }

    #[test]
    fn dense_groups_and_extreme_times_return_errors_without_panicking() {
        let options = TextTrackOptions { grouping: "words".into(), ..Default::default() };
        let mut input = words(&["one", "two"]);
        input[1].start = Tick(1);
        assert!(blocks(&input, &options, FrameRate::FPS_24).is_err());
        input[1].start = Tick(i64::MAX - 1);
        input[1].end = Tick::MAX;
        let result = std::panic::catch_unwind(|| blocks(&input, &options, FrameRate::FPS_24));
        assert!(result.is_ok());
    }
}
