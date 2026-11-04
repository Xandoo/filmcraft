//! Local GPU qualification: MODEL_DIRECTORY AUDIO.f32 [--cpu] [--reference=TEXT_FILE].
use filmcraft_speech::Options;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let directory = args.next().ok_or("need model directory")?;
    let audio = args.next().ok_or("need mono 16 kHz f32 file")?;
    let rest: Vec<_> = args.collect();
    let gpu = !rest.iter().any(|v| v == "--cpu");
    let loading = std::time::Instant::now();
    let model_directory = std::path::Path::new(&directory);
    let id = model_directory.file_name().and_then(|v| v.to_str()).ok_or("invalid model directory")?;
    let parent = model_directory.parent().ok_or("model directory needs a parent")?;
    let model = filmcraft_speech::load_on_device(parent, id, if gpu { "gpu" } else { "cpu" })?;
    eprintln!("loaded on {} in {:.2}s", model.device(), loading.elapsed().as_secs_f64());
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(audio)?.take(60 * 16000 * 4 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 60 * 16000 * 4 || bytes.len() % 4 != 0 {
        return Err("audio must be at most 60 seconds of f32 samples".into());
    }
    let samples: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|v| f32::from_le_bytes(*v)).collect();
    let start = std::time::Instant::now();
    let transcript = model.transcribe(&samples, &Options { language: Some("en".into()), diarize: false, ..Default::default() }, &mut |_, _| true)?;
    let seconds = start.elapsed().as_secs_f64();
    let duration = samples.len() as f64 / 16000.0;
    let mut result = serde_json::json!({"device":model.device(),"text":transcript.text(),"seconds":seconds,"audioSeconds":duration,"realTimeFactor":seconds/duration.max(1e-6),"words":transcript.words});
    if let Some(path) = rest.iter().find_map(|v| v.strip_prefix("--reference=")) {
        result["wer"] = serde_json::json!(filmcraft_speech::word_error_rate(&std::fs::read_to_string(path)?, &transcript.text()));
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
