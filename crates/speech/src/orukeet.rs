//! Orukeet r3 / FastConformer-TDT inference from the published model description and config.
//! Weight layout: oruk/orukeet Transformers export b59c13a733fce5cf193230d0fa317ee7f145a108.
use crate::{
    Options, ProgressFn, SpeechError, Transcriber,
    neural::{self, Linear, Loader, Norm, err},
};
use burn::{
    backend::{NdArray, Wgpu, wgpu::WgpuDevice},
    tensor::{
        Int, Tensor, TensorData, activation,
        backend::Backend,
        module::{conv1d, conv2d},
        ops::ConvOptions,
    },
};
use filmcraft_project::{Transcript, Word};
use std::{path::Path, sync::Mutex};

const WIDTH: usize = 1024;
const HEADS: usize = 8;
const HIDDEN: usize = 640;
const VOCAB: usize = 8193;
const WINDOW: usize = 20 * 16_000;

// A width-one convolution is a matrix multiplication across channels. Using the general
// convolution kernel here repeats scalar convolution work for every channel in every block.
fn pointwise<B: Backend>(x: Tensor<B, 3>, weight: Tensor<B, 3>) -> Tensor<B, 3> {
    let [output, input, _] = weight.dims();
    x.swap_dims(1, 2).matmul(weight.reshape([output, input]).transpose().unsqueeze::<3>()).swap_dims(1, 2)
}

struct Conv2<B: Backend> {
    weight: Tensor<B, 4>,
    bias: Tensor<B, 1>,
    stride: usize,
    pad: usize,
    groups: usize,
}
impl<B: Backend> Conv2<B> {
    fn forward(&self, x: Tensor<B, 4>) -> Tensor<B, 4> {
        conv2d(x, self.weight.clone(), Some(self.bias.clone()), ConvOptions::new([self.stride; 2], [self.pad; 2], [1; 2], self.groups))
    }
}
struct FeedForward<B: Backend> {
    norm: Norm<B>,
    up: Linear<B>,
    down: Linear<B>,
}
impl<B: Backend> FeedForward<B> {
    fn load(l: &Loader<B>, prefix: &str, which: usize) -> Result<Self, SpeechError> {
        Ok(Self {
            norm: l.norm(&format!("{prefix}.norm_feed_forward{which}"), WIDTH)?,
            up: l.linear(&format!("{prefix}.feed_forward{which}.linear1"), WIDTH, 4096, false)?,
            down: l.linear(&format!("{prefix}.feed_forward{which}.linear2"), 4096, WIDTH, false)?,
        })
    }
    fn forward(&self, x: Tensor<B, 3>) -> Tensor<B, 3> {
        self.down.forward(activation::silu(self.up.forward(self.norm.forward(x)))) * 0.5
    }
}
struct Block<B: Backend> {
    ff1: FeedForward<B>,
    ff2: FeedForward<B>,
    attention_norm: Norm<B>,
    conv_norm: Norm<B>,
    out_norm: Norm<B>,
    q: Linear<B>,
    k: Linear<B>,
    v: Linear<B>,
    o: Linear<B>,
    relative: Linear<B>,
    u: Tensor<B, 4>,
    v_bias: Tensor<B, 4>,
    point1: Tensor<B, 3>,
    depth: Tensor<B, 3>,
    point2: Tensor<B, 3>,
    bn_scale: Tensor<B, 3>,
    bn_bias: Tensor<B, 3>,
}
impl<B: Backend> Block<B> {
    fn load(l: &Loader<B>, i: usize) -> Result<Self, SpeechError> {
        let p = format!("encoder.layers.{i}");
        let attention = format!("{p}.self_attn");
        let conv = format!("{p}.conv");
        let bn_w = l.weight::<1>(&format!("{conv}.norm.weight"), [WIDTH])?;
        let bn_b = l.weight::<1>(&format!("{conv}.norm.bias"), [WIDTH])?;
        let mean = l.weight::<1>(&format!("{conv}.norm.running_mean"), [WIDTH])?;
        let variance = l.weight::<1>(&format!("{conv}.norm.running_var"), [WIDTH])?;
        let scale = bn_w / (variance + 1e-5).sqrt();
        let bias = bn_b - mean * scale.clone();
        Ok(Self {
            ff1: FeedForward::load(l, &p, 1)?,
            ff2: FeedForward::load(l, &p, 2)?,
            attention_norm: l.norm(&format!("{p}.norm_self_att"), WIDTH)?,
            conv_norm: l.norm(&format!("{p}.norm_conv"), WIDTH)?,
            out_norm: l.norm(&format!("{p}.norm_out"), WIDTH)?,
            q: l.linear(&format!("{attention}.q_proj"), WIDTH, WIDTH, false)?,
            k: l.linear(&format!("{attention}.k_proj"), WIDTH, WIDTH, false)?,
            v: l.linear(&format!("{attention}.v_proj"), WIDTH, WIDTH, false)?,
            o: l.linear(&format!("{attention}.o_proj"), WIDTH, WIDTH, false)?,
            relative: l.linear(&format!("{attention}.relative_k_proj"), WIDTH, WIDTH, false)?,
            u: l.weight::<2>(&format!("{attention}.bias_u"), [HEADS, WIDTH / HEADS])?.reshape([1, HEADS, 1, WIDTH / HEADS]),
            v_bias: l.weight::<2>(&format!("{attention}.bias_v"), [HEADS, WIDTH / HEADS])?.reshape([1, HEADS, 1, WIDTH / HEADS]),
            point1: l.weight(&format!("{conv}.pointwise_conv1.weight"), [WIDTH * 2, WIDTH, 1])?,
            depth: l.weight(&format!("{conv}.depthwise_conv.weight"), [WIDTH, 1, 9])?,
            point2: l.weight(&format!("{conv}.pointwise_conv2.weight"), [WIDTH, WIDTH, 1])?,
            bn_scale: scale.reshape([1, WIDTH, 1]),
            bn_bias: bias.reshape([1, WIDTH, 1]),
        })
    }
    fn forward(&self, mut x: Tensor<B, 3>, positions: Tensor<B, 3>, indices: Tensor<B, 4, Int>, matrix_channels: bool) -> Tensor<B, 3> {
        x = x.clone() + self.ff1.forward(x);
        let n = self.attention_norm.forward(x.clone());
        let [batch, time, _] = n.dims();
        let split = |v: Tensor<B, 3>| v.reshape([batch, time, HEADS, WIDTH / HEADS]).swap_dims(1, 2);
        let q = split(self.q.forward(n.clone()));
        let k = split(self.k.forward(n.clone()));
        let v = split(self.v.forward(n));
        let relative = self.relative.forward(positions).reshape([1, 2 * time - 1, HEADS, WIDTH / HEADS]).swap_dims(1, 2);
        let content = (q.clone() + self.u.clone()).matmul(k.swap_dims(2, 3));
        let positional = (q + self.v_bias.clone()).matmul(relative.swap_dims(2, 3)).gather(3, indices);
        let probabilities = activation::softmax((content + positional) / (128.0_f32.sqrt()), 3);
        let attended = probabilities.matmul(v).swap_dims(1, 2).reshape([batch, time, WIDTH]);
        x = x + self.o.forward(attended);
        let conv = self.conv_norm.forward(x.clone()).swap_dims(1, 2);
        let channels = |input, weight| {
            if matrix_channels { pointwise(input, weight) } else { conv1d(input, weight, None, ConvOptions::new([1], [0], [1], 1)) }
        };
        let gates = channels(conv, self.point1.clone());
        let a = gates.clone().slice([0..batch, 0..WIDTH, 0..time]);
        let b = gates.slice([0..batch, WIDTH..WIDTH * 2, 0..time]);
        let depth = conv1d(a * activation::sigmoid(b), self.depth.clone(), None, ConvOptions::new([1], [4], [1], WIDTH));
        let normed = depth * self.bn_scale.clone() + self.bn_bias.clone();
        x = x + channels(activation::silu(normed), self.point2.clone()).swap_dims(1, 2);
        x = x.clone() + self.ff2.forward(x);
        self.out_norm.forward(x)
    }
}

struct Lstm<B: Backend> {
    input: Linear<B>,
    hidden: Linear<B>,
}
impl<B: Backend> Lstm<B> {
    fn load(l: &Loader<B>, i: usize) -> Result<Self, SpeechError> {
        let prefix = "decoder.lstm";
        Ok(Self {
            input: Linear {
                weight: l.weight(&format!("{prefix}.weight_ih_l{i}"), [HIDDEN * 4, HIDDEN])?,
                bias: Some(l.weight(&format!("{prefix}.bias_ih_l{i}"), [HIDDEN * 4])?),
            },
            hidden: Linear {
                weight: l.weight(&format!("{prefix}.weight_hh_l{i}"), [HIDDEN * 4, HIDDEN])?,
                bias: Some(l.weight(&format!("{prefix}.bias_hh_l{i}"), [HIDDEN * 4])?),
            },
        })
    }
    fn forward(&self, x: Tensor<B, 3>, h: Tensor<B, 3>, c: Tensor<B, 3>) -> (Tensor<B, 3>, Tensor<B, 3>) {
        let gates = self.input.forward(x) + self.hidden.forward(h);
        let part = |i| gates.clone().slice([0..1, 0..1, i * HIDDEN..(i + 1) * HIDDEN]);
        let c = activation::sigmoid(part(1)) * c + activation::sigmoid(part(0)) * part(2).tanh();
        let h = activation::sigmoid(part(3)) * c.clone().tanh();
        (h, c)
    }
}
struct Model<B: Backend> {
    matrix_channels: bool,
    device: B::Device,
    sub: Vec<Conv2<B>>,
    projection: Linear<B>,
    blocks: Vec<Block<B>>,
    encoder: Linear<B>,
    embedding: Tensor<B, 2>,
    lstm: Vec<Lstm<B>>,
    decoder: Linear<B>,
    head: Linear<B>,
}
impl<B: Backend> Model<B> {
    fn load(bytes: &[u8], device: B::Device, matrix_channels: bool) -> Result<Self, SpeechError> {
        let l = Loader::<B>::new(bytes, device.clone())?;
        let mut sub = Vec::new();
        for (i, input, kernel, stride, pad, groups) in
            [(0, 1, 3, 2, 1, 1), (2, 1, 3, 2, 1, 256), (3, 256, 1, 1, 0, 1), (5, 1, 3, 2, 1, 256), (6, 256, 1, 1, 0, 1)]
        {
            sub.push(Conv2 {
                weight: l.weight(&format!("encoder.subsampling.layers.{i}.weight"), [256, input, kernel, kernel])?,
                bias: l.weight(&format!("encoder.subsampling.layers.{i}.bias"), [256])?,
                stride,
                pad,
                groups,
            });
        }
        let mut blocks = Vec::with_capacity(24);
        for i in 0..24 {
            blocks.push(Block::load(&l, i)?);
        }
        Ok(Self {
            matrix_channels,
            device,
            sub,
            projection: l.linear("encoder.subsampling.linear", 4096, WIDTH, true)?,
            blocks,
            encoder: l.linear("encoder_projector", WIDTH, HIDDEN, true)?,
            embedding: l.weight("decoder.embedding.weight", [VOCAB, HIDDEN])?,
            lstm: vec![Lstm::load(&l, 0)?, Lstm::load(&l, 1)?],
            decoder: l.linear("decoder.decoder_projector", HIDDEN, HIDDEN, true)?,
            head: l.linear("joint.head", HIDDEN, VOCAB + 5, true)?,
        })
    }
    fn encode(&self, features: Vec<f32>, frames: usize, progress: &mut dyn FnMut(f32, &str) -> bool) -> Result<Tensor<B, 3>, SpeechError> {
        let mut x = Tensor::from_data(TensorData::new(features, [1, 1, frames, 128]), &self.device);
        for (i, conv) in self.sub.iter().enumerate() {
            x = conv.forward(x);
            if matches!(i, 0 | 2 | 4) {
                x = activation::relu(x);
            }
        }
        let [_, channels, time, freq] = x.dims();
        let mut x = self.projection.forward(x.swap_dims(1, 2).reshape([1, time, channels * freq]));
        let mut positional = Vec::with_capacity((time * 2 - 1) * WIDTH);
        for p in 0..time * 2 - 1 {
            let pos = time as f64 - 1.0 - p as f64;
            for i in 0..WIDTH {
                let angle = pos * (-(10000.0_f64).ln() * (i / 2 * 2) as f64 / WIDTH as f64).exp();
                positional.push(if i % 2 == 0 { angle.sin() as f32 } else { angle.cos() as f32 });
            }
        }
        let positions = Tensor::from_data(TensorData::new(positional, [1, time * 2 - 1, WIDTH]), &self.device);
        let mut indices = Vec::with_capacity(HEADS * time * time);
        for _ in 0..HEADS {
            for i in 0..time {
                for j in 0..time {
                    indices.push((time - 1 - i + j) as i32);
                }
            }
        }
        let indices = Tensor::<B, 4, Int>::from_data(TensorData::new(indices, [1, HEADS, time, time]), &self.device);
        for (i, block) in self.blocks.iter().enumerate() {
            if !progress(i as f32 / 24.0, "Encoding speech") {
                return Err(SpeechError::Cancelled);
            }
            x = block.forward(x, positions.clone(), indices.clone(), self.matrix_channels);
        }
        Ok(self.encoder.forward(x))
    }
    fn predict(&self, token: Option<usize>, state: &mut [(Tensor<B, 3>, Tensor<B, 3>)]) -> Tensor<B, 3> {
        let mut x = match token {
            Some(t) => self.embedding.clone().slice([t..t + 1, 0..HIDDEN]).reshape([1, 1, HIDDEN]),
            None => Tensor::zeros([1, 1, HIDDEN], &self.device),
        };
        for (layer, (h, c)) in self.lstm.iter().zip(state.iter_mut()) {
            let (new_h, new_c) = layer.forward(x, h.clone(), c.clone());
            *h = new_h.clone();
            *c = new_c;
            x = new_h;
        }
        self.decoder.forward(x)
    }
    fn decode(&self, encoded: Tensor<B, 3>, progress: &mut dyn FnMut(f32, &str) -> bool) -> Result<Vec<(usize, usize, usize)>, SpeechError> {
        let time = encoded.dims()[1];
        let mut state: Vec<_> = (0..2).map(|_| (Tensor::zeros([1, 1, HIDDEN], &self.device), Tensor::zeros([1, 1, HIDDEN], &self.device))).collect();
        let mut prediction = self.predict(None, &mut state);
        let (mut at, mut repeated) = (0usize, 0usize);
        let mut output = Vec::new();
        for _ in 0..time.saturating_mul(11) {
            if at >= time {
                break;
            }
            if !progress(at as f32 / time.max(1) as f32, "Decoding words") {
                return Err(SpeechError::Cancelled);
            }
            let acoustic = encoded.clone().slice([0..1, at..at + 1, 0..HIDDEN]);
            let logits = neural::vector(self.head.forward(activation::relu(acoustic + prediction.clone())))?;
            let token = logits
                .get(..VOCAB)
                .ok_or_else(|| err("invalid joint output"))?
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .ok_or_else(|| err("empty vocabulary"))?;
            let mut duration = logits
                .get(VOCAB..)
                .ok_or_else(|| err("invalid duration output"))?
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .unwrap_or(1);
            if token != VOCAB - 1 {
                output.push((token, at, duration.max(1)));
                prediction = self.predict(Some(token), &mut state);
                repeated += 1;
            }
            if token == VOCAB - 1 || repeated >= 10 {
                duration = duration.max(1);
            }
            if duration > 0 {
                at = at.saturating_add(duration);
                repeated = 0;
            }
        }
        Ok(output)
    }
}

enum Runtime {
    Cpu(Box<Model<NdArray<f32>>>),
    Gpu(Box<Model<Wgpu<f32, i32>>>),
}
pub struct Orukeet {
    model: Mutex<Runtime>,
    vocabulary: Vec<String>,
    device: &'static str,
}
impl Orukeet {
    pub fn load(directory: &Path, gpu: bool) -> Result<Self, SpeechError> {
        neural::guarded(|| {
            let config: serde_json::Value = serde_json::from_slice(&neural::read(&directory.join("config.json"), 65536)?).map_err(err)?;
            if config["model_type"] != "parakeet_tdt"
                || config["encoder_config"]["hidden_size"] != 1024
                || config["encoder_config"]["num_hidden_layers"] != 24
                || config["encoder_config"]["num_attention_heads"] != 8
                || config["encoder_config"]["num_mel_bins"] != 128
                || config["decoder_hidden_size"] != 640
                || config["num_decoder_layers"] != 2
                || config["blank_token_id"] != 8192
                || config["durations"] != serde_json::json!([0, 1, 2, 3, 4])
                || config["vocab_size"] != 8193
            {
                return Err(err("unsupported Orukeet architecture"));
            }
            let tokens: serde_json::Value = serde_json::from_slice(&neural::read(&directory.join("tokenizer.json"), 4 * 1024 * 1024)?).map_err(err)?;
            let raw = tokens["model"]["vocab"].as_object().ok_or_else(|| err("invalid Orukeet vocabulary"))?;
            let mut vocabulary = vec![String::new(); VOCAB];
            for (text, index) in raw {
                let i = index.as_u64().and_then(|v| usize::try_from(v).ok()).filter(|i| *i < VOCAB).ok_or_else(|| err("vocabulary index outside model"))?;
                if text.len() > 4096 {
                    return Err(err("oversized token"));
                }
                if let Some(slot) = vocabulary.get_mut(i) {
                    *slot = text.clone();
                }
            }
            let weights = neural::read(&directory.join("model.safetensors"), 3_000_000_000)?;
            let (model, device) = if gpu {
                (Runtime::Gpu(Box::new(Model::load(&weights, WgpuDevice::DiscreteGpu(0), false)?)), "GPU (wgpu)")
            } else {
                (Runtime::Cpu(Box::new(Model::load(&weights, Default::default(), true)?)), "CPU")
            };
            Ok(Self { model: Mutex::new(model), vocabulary, device })
        })
    }
}
impl Transcriber for Orukeet {
    fn id(&self) -> String {
        "orukeet".into()
    }
    fn device(&self) -> &str {
        self.device
    }
    fn transcribe(&self, audio: &[f32], options: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        neural::guarded(|| {
            if audio.len() > 60 * 16000 || audio.iter().any(|v| !v.is_finite()) {
                return Err(err("Orukeet expects at most 60 seconds of finite audio per chunk"));
            }
            if let Some(language) = &options.language
                && ![
                    "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt", "pl", "pt", "ro", "ru", "sk", "sl", "es",
                    "sv", "uk",
                ]
                .contains(&language.as_str())
            {
                return Err(err("Orukeet does not support the selected language; choose Whisper"));
            }
            let model = self.model.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut transcript = Transcript { language: options.language.clone().unwrap_or_default(), source: self.id(), ..Default::default() };
            let chunks = audio.len().div_ceil(WINDOW);
            for (chunk, part) in audio.chunks(WINDOW).enumerate() {
                if part.len() < 160 {
                    continue;
                }
                let (frames, features) = features(part)?;
                let mut report = |f: f32, s: &str| progress((chunk as f32 + 0.8 * f) / chunks.max(1) as f32, s);
                let tokens = match &*model {
                    Runtime::Cpu(m) => {
                        let encoded = m.encode(features, frames, &mut report)?;
                        m.decode(encoded, &mut |f, s| progress((chunk as f32 + 0.8 + 0.2 * f) / chunks.max(1) as f32, s))?
                    }
                    Runtime::Gpu(m) => {
                        let encoded = m.encode(features, frames, &mut report)?;
                        m.decode(encoded, &mut |f, s| progress((chunk as f32 + 0.8 + 0.2 * f) / chunks.max(1) as f32, s))?
                    }
                };
                for (token, time, duration) in tokens {
                    let Some(piece) = self.vocabulary.get(token) else {
                        return Err(err("token outside vocabulary"));
                    };
                    if piece.starts_with('<') {
                        continue;
                    }
                    let start = crate::sample_tick(chunk.saturating_mul(WINDOW) as i64) + crate::seconds_tick(time as f64 * 0.08);
                    let end = (start + crate::seconds_tick(duration as f64 * 0.08)).min(crate::sample_tick(audio.len() as i64));
                    let new_word = piece.starts_with('▁');
                    let text = piece.replace('▁', " ");
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if !new_word && let Some(word) = transcript.words.last_mut() {
                        word.text.push_str(text);
                        word.end = end.max(word.end);
                    } else {
                        transcript.words.push(Word::new(text, start, end.max(start + crate::sample_tick(1))));
                    }
                }
            }
            transcript.normalize();
            if options.diarize {
                crate::diarize::diarize(audio, &mut transcript, &crate::diarize::Params { max_speakers: options.max_speakers, ..Default::default() });
            }
            Ok(transcript)
        })
    }
}

/// Published Parakeet preprocessing: 512 FFT, centered 400-sample Hann, pre-emphasis,
/// Slaney mel bands, natural log and per-feature utterance normalization.
fn features(audio: &[f32]) -> Result<(usize, Vec<f32>), SpeechError> {
    use rustfft::{FftPlanner, num_complex::Complex};
    if audio.is_empty() || audio.len() > WINDOW {
        return Err(err("invalid acoustic window"));
    }
    let frames = audio.len() / 160 + 1;
    let bins = 257;
    let filters = crate::mel::mel_filters_for(128, 512);
    let signal: Vec<_> = audio.iter().enumerate().map(|(i, x)| *x - 0.97 * audio.get(i.wrapping_sub(1)).copied().unwrap_or(0.0)).collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(512);
    let mut output = vec![0.0f32; frames * 128];
    let mut buffer = vec![Complex::new(0.0, 0.0); 512];
    for frame in 0..frames {
        for (i, value) in buffer.iter_mut().enumerate() {
            let raw = frame as i64 * 160 + i as i64 - 256;
            let period = (signal.len().saturating_sub(1) * 2).max(1) as i64;
            let mut position = raw.rem_euclid(period);
            if position >= signal.len() as i64 {
                position = period - position;
            }
            let window = if (56..456).contains(&i) { 0.5 - 0.5 * (std::f32::consts::TAU * (i - 56) as f32 / 400.0).cos() } else { 0.0 };
            *value = Complex::new(signal.get(position as usize).copied().unwrap_or(0.0) * window, 0.0);
        }
        fft.process(&mut buffer);
        for band in 0..128 {
            let energy = (0..bins).map(|k| buffer[k].norm_sqr() * filters[band * bins + k]).sum::<f32>();
            output[frame * 128 + band] = (energy + 1e-5).ln();
        }
    }
    for band in 0..128 {
        let mean = (0..frames).map(|f| output[f * 128 + band]).sum::<f32>() / frames as f32;
        let variance = (0..frames).map(|f| (output[f * 128 + band] - mean).powi(2)).sum::<f32>() / frames.saturating_sub(1).max(1) as f32;
        for f in 0..frames {
            output[f * 128 + band] = (output[f * 128 + band] - mean) / (variance.sqrt() + 1e-5);
        }
    }
    Ok((frames, output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointwise_matmul_matches_channel_convolution_for_multiple_batches() {
        type B = NdArray<f32>;
        let device = Default::default();
        let input = Tensor::<B, 3>::from_data(TensorData::new((0..30).map(|i| (i as f32 - 11.0) / 7.0).collect::<Vec<_>>(), [2, 3, 5]), &device);
        let weight = Tensor::<B, 3>::from_data(TensorData::new((0..12).map(|i| (i as f32 - 4.0) / 9.0).collect::<Vec<_>>(), [4, 3, 1]), &device);
        let expected = neural::vector(conv1d(input.clone(), weight.clone(), None, ConvOptions::new([1], [0], [1], 1))).unwrap();
        let actual = neural::vector(pointwise(input, weight)).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected) {
            assert!((a - b).abs() < 1e-5, "{a} != {b}");
        }
    }
}
