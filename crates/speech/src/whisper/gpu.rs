//! wgpu execution of Whisper's published network, sharing the existing tokenizer and alignment.
use super::model::Config;
use crate::{
    SpeechError,
    neural::{self, Linear, Loader, Norm, err},
};
use burn::{
    backend::{Wgpu, wgpu::WgpuDevice},
    tensor::{
        Int, Tensor, TensorData, activation,
        module::{conv1d, embedding},
        ops::ConvOptions,
    },
};
type B = Wgpu<f32, i32>;
type T = Tensor<B, 3>;

fn candle_error(e: SpeechError) -> candle_core::Error {
    candle_core::Error::Msg(e.to_string())
}
fn upload(x: &candle_core::Tensor, device: &WgpuDevice) -> Result<T, SpeechError> {
    let (a, b, c) = x.dims3().map_err(err)?;
    let values = x.flatten_all().and_then(|v| v.to_vec1::<f32>()).map_err(err)?;
    Ok(Tensor::from_data(TensorData::new(values, [a, b, c]), device))
}
fn download(x: T) -> Result<candle_core::Tensor, SpeechError> {
    let shape = x.dims();
    candle_core::Tensor::from_vec(neural::vector(x)?, (shape[0], shape[1], shape[2]), &candle_core::Device::Cpu).map_err(err)
}

struct Attention {
    q: Linear<B>,
    k: Linear<B>,
    v: Linear<B>,
    out: Linear<B>,
    heads: usize,
    width: usize,
    cache: Option<(Tensor<B, 4>, Tensor<B, 4>)>,
}
impl Attention {
    fn load(l: &Loader<B>, name: &str, width: usize, heads: usize) -> Result<Self, SpeechError> {
        Ok(Self {
            q: l.linear(&format!("{name}.q_proj"), width, width, true)?,
            k: l.linear(&format!("{name}.k_proj"), width, width, false)?,
            v: l.linear(&format!("{name}.v_proj"), width, width, true)?,
            out: l.linear(&format!("{name}.out_proj"), width, width, true)?,
            heads,
            width,
            cache: None,
        })
    }
    fn split(&self, x: T) -> Tensor<B, 4> {
        let [batch, time, _] = x.dims();
        x.reshape([batch, time, self.heads, self.width / self.heads]).swap_dims(1, 2)
    }
    fn forward(&mut self, x: T, audio: Option<T>, cached: bool, mask: Option<Tensor<B, 4>>) -> (T, Tensor<B, 4>) {
        let [batch, time, _] = x.dims();
        let q = self.split(self.q.forward(x.clone())) / ((self.width / self.heads) as f32).sqrt();
        let (k, v) = if let Some(audio) = audio {
            if cached && let Some((k, v)) = &self.cache {
                (k.clone(), v.clone())
            } else {
                (self.split(self.k.forward(audio.clone())), self.split(self.v.forward(audio)))
            }
        } else {
            let k = self.split(self.k.forward(x.clone()));
            let v = self.split(self.v.forward(x));
            if cached && let Some((oldk, oldv)) = &self.cache { (Tensor::cat(vec![oldk.clone(), k], 2), Tensor::cat(vec![oldv.clone(), v], 2)) } else { (k, v) }
        };
        if cached {
            self.cache = Some((k.clone(), v.clone()));
        }
        let mut logits = q.matmul(k.swap_dims(2, 3));
        if let Some(mask) = mask {
            logits = logits + mask;
        }
        let output = activation::softmax(logits.clone(), 3).matmul(v).swap_dims(1, 2).reshape([batch, time, self.width]);
        (self.out.forward(output), logits)
    }
}
struct Block {
    attention: Attention,
    attention_norm: Norm<B>,
    cross: Option<(Attention, Norm<B>)>,
    up: Linear<B>,
    down: Linear<B>,
    norm: Norm<B>,
}
impl Block {
    fn load(l: &Loader<B>, name: &str, width: usize, heads: usize, cross: bool) -> Result<Self, SpeechError> {
        Ok(Self {
            attention: Attention::load(l, &format!("{name}.self_attn"), width, heads)?,
            attention_norm: l.norm(&format!("{name}.self_attn_layer_norm"), width)?,
            cross: if cross {
                Some((Attention::load(l, &format!("{name}.encoder_attn"), width, heads)?, l.norm(&format!("{name}.encoder_attn_layer_norm"), width)?))
            } else {
                None
            },
            up: l.linear(&format!("{name}.fc1"), width, width * 4, true)?,
            down: l.linear(&format!("{name}.fc2"), width * 4, width, true)?,
            norm: l.norm(&format!("{name}.final_layer_norm"), width)?,
        })
    }
    fn forward(&mut self, mut x: T, audio: Option<T>, cached: bool, mask: Option<Tensor<B, 4>>) -> (T, Option<Tensor<B, 4>>) {
        let (attention, _) = self.attention.forward(self.attention_norm.forward(x.clone()), None, cached, mask);
        x = x + attention;
        let mut picked = None;
        if let (Some((cross, norm)), Some(audio)) = (&mut self.cross, audio) {
            let (output, qk) = cross.forward(norm.forward(x.clone()), Some(audio), cached, None);
            x = x + output;
            picked = Some(qk);
        }
        let y = self.down.forward(activation::gelu(self.up.forward(self.norm.forward(x.clone()))));
        (x + y, picked)
    }
    fn reset(&mut self) {
        self.attention.cache = None;
        if let Some((cross, _)) = &mut self.cross {
            cross.cache = None;
        }
    }
}
pub(crate) struct Encoder {
    first: Tensor<B, 3>,
    first_bias: Tensor<B, 1>,
    second: Tensor<B, 3>,
    second_bias: Tensor<B, 1>,
    position: Tensor<B, 2>,
    blocks: Vec<Block>,
    norm: Norm<B>,
    device: WgpuDevice,
}
impl Encoder {
    fn load(l: &Loader<B>, cfg: &Config) -> Result<Self, SpeechError> {
        let name = "model.encoder";
        let d = cfg.d_model;
        let mut blocks = Vec::new();
        for i in 0..cfg.encoder_layers {
            blocks.push(Block::load(l, &format!("{name}.layers.{i}"), d, cfg.encoder_attention_heads, false)?);
        }
        Ok(Self {
            first: l.weight(&format!("{name}.conv1.weight"), [d, cfg.num_mel_bins, 3])?,
            first_bias: l.weight(&format!("{name}.conv1.bias"), [d])?,
            second: l.weight(&format!("{name}.conv2.weight"), [d, d, 3])?,
            second_bias: l.weight(&format!("{name}.conv2.bias"), [d])?,
            position: l.weight(&format!("{name}.embed_positions.weight"), [cfg.max_source_positions, d])?,
            blocks,
            norm: l.norm(&format!("{name}.layer_norm"), d)?,
            device: l.device.clone(),
        })
    }
    pub fn forward(&mut self, input: &candle_core::Tensor) -> candle_core::Result<candle_core::Tensor> {
        neural::guarded(|| {
            let x = upload(input, &self.device)?;
            let x = activation::gelu(conv1d(x, self.first.clone(), Some(self.first_bias.clone()), ConvOptions::new([1], [1], [1], 1)));
            let x = activation::gelu(conv1d(x, self.second.clone(), Some(self.second_bias.clone()), ConvOptions::new([2], [1], [1], 1))).swap_dims(1, 2);
            let [_, time, width] = x.dims();
            if time > self.position.dims()[0] {
                return Err(err("audio exceeds encoder context"));
            }
            let mut x = x + self.position.clone().slice([0..time, 0..width]).unsqueeze::<3>();
            for block in &mut self.blocks {
                x = block.forward(x, None, false, None).0;
            }
            download(self.norm.forward(x))
        })
        .map_err(candle_error)
    }
}
pub(crate) struct Decoder {
    embedding: Tensor<B, 2>,
    position: Tensor<B, 2>,
    blocks: Vec<Block>,
    norm: Norm<B>,
    device: WgpuDevice,
    offset: usize,
    width: usize,
    vocab: usize,
    audio: Option<T>,
}
impl Decoder {
    fn load(l: &Loader<B>, cfg: &Config) -> Result<Self, SpeechError> {
        let name = "model.decoder";
        let d = cfg.d_model;
        let mut blocks = Vec::new();
        for i in 0..cfg.decoder_layers {
            blocks.push(Block::load(l, &format!("{name}.layers.{i}"), d, cfg.decoder_attention_heads, true)?);
        }
        Ok(Self {
            embedding: l.weight(&format!("{name}.embed_tokens.weight"), [cfg.vocab_size, d])?,
            position: l.weight(&format!("{name}.embed_positions.weight"), [cfg.max_target_positions, d])?,
            blocks,
            norm: l.norm(&format!("{name}.layer_norm"), d)?,
            device: l.device.clone(),
            offset: 0,
            width: d,
            vocab: cfg.vocab_size,
            audio: None,
        })
    }
    pub fn reset(&mut self) {
        self.offset = 0;
        self.audio = None;
        for block in &mut self.blocks {
            block.reset();
        }
    }
    pub fn forward(
        &mut self,
        tokens: &[u32],
        xa: &candle_core::Tensor,
        cached: bool,
        heads: Option<&[(usize, usize)]>,
    ) -> candle_core::Result<(candle_core::Tensor, Option<candle_core::Tensor>)> {
        neural::guarded(|| {
            if !cached {
                self.reset();
            }
            let time = tokens.len();
            let end = self.offset.checked_add(time).ok_or_else(|| err("token context overflow"))?;
            if time == 0 || end > self.position.dims()[0] || tokens.iter().any(|v| *v as usize >= self.vocab) {
                return Err(err("token outside decoder context"));
            }
            let ids = Tensor::<B, 2, Int>::from_data(TensorData::new(tokens.iter().map(|v| *v as i32).collect::<Vec<_>>(), [1, time]), &self.device);
            let mut x = embedding(self.embedding.clone(), ids) + self.position.clone().slice([self.offset..end, 0..self.width]).unsqueeze::<3>();
            let mask = if time > 1 {
                let mut mask = Vec::with_capacity(time * end);
                for i in 0..time {
                    for j in 0..end {
                        mask.push(if j > self.offset + i { -1e9f32 } else { 0.0 });
                    }
                }
                Some(Tensor::from_data(TensorData::new(mask, [1, 1, time, end]), &self.device))
            } else {
                None
            };
            if self.audio.is_none() {
                self.audio = Some(upload(xa, &self.device)?);
            }
            let mut picked = Vec::new();
            for (index, block) in self.blocks.iter_mut().enumerate() {
                let (next, qk) = block.forward(x, self.audio.clone(), cached, mask.clone());
                x = next;
                if let (Some(heads), Some(qk)) = (heads, qk) {
                    let [_, count, _, frames] = qk.dims();
                    for &(layer, head) in heads {
                        if layer == index {
                            if head >= count {
                                return Err(err("alignment head outside decoder"));
                            }
                            picked.push(qk.clone().slice([0..1, head..head + 1, 0..time, 0..frames]).reshape([1, time, frames]));
                        }
                    }
                }
            }
            if cached {
                self.offset = end;
            }
            let logits = download(self.norm.forward(x).matmul(self.embedding.clone().transpose().unsqueeze::<3>()))?;
            let qk = if picked.is_empty() { None } else { Some(download(Tensor::cat(picked, 0))?) };
            Ok((logits, qk))
        })
        .map_err(candle_error)
    }
}
pub(crate) fn load(cfg: &Config, bytes: &[u8]) -> Result<(Encoder, Decoder), SpeechError> {
    neural::guarded(|| {
        if !(1..=32).contains(&cfg.encoder_layers)
            || !(1..=32).contains(&cfg.decoder_layers)
            || cfg.d_model == 0
            || cfg.d_model > 1536
            || cfg.encoder_attention_heads == 0
            || cfg.decoder_attention_heads == 0
            || !cfg.d_model.is_multiple_of(cfg.encoder_attention_heads)
            || !cfg.d_model.is_multiple_of(cfg.decoder_attention_heads)
            || cfg.max_source_positions > 1500
            || cfg.max_target_positions > 1024
            || cfg.vocab_size > 60000
        {
            return Err(err("unsupported Whisper GPU architecture"));
        }
        let loader = Loader::<B>::new(bytes, WgpuDevice::DiscreteGpu(0))?;
        Ok((Encoder::load(&loader, cfg)?, Decoder::load(&loader, cfg)?))
    })
}
