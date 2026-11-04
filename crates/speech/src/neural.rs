//! Shared safe tensor operations for local inference. No model-specific third-party code.
use crate::SpeechError;
use burn::tensor::{Tensor, TensorData, backend::Backend};
use safetensors::{Dtype, SafeTensors};
use std::path::Path;

pub(crate) fn err(e: impl ToString) -> SpeechError {
    SpeechError::Model(e.to_string())
}
pub(crate) fn read(path: &Path, limit: u64) -> Result<Vec<u8>, SpeechError> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(err("model artifact exceeds size limit"));
    }
    let mut data = Vec::new();
    let capacity = usize::try_from(size.saturating_add(1)).map_err(err)?;
    data.try_reserve_exact(capacity).map_err(|_| err("not enough memory to load model artifact"))?;
    file.take(size.saturating_add(1)).read_to_end(&mut data)?;
    if data.len() as u64 != size {
        return Err(err("model artifact changed while loading"));
    }
    Ok(data)
}
pub(crate) struct Loader<'a, B: Backend> {
    pub tensors: SafeTensors<'a>,
    pub device: B::Device,
}
impl<'a, B: Backend> Loader<'a, B> {
    pub fn new(bytes: &'a [u8], device: B::Device) -> Result<Self, SpeechError> {
        Ok(Self { tensors: SafeTensors::deserialize(bytes).map_err(err)?, device })
    }
    pub fn weight<const D: usize>(&self, name: &str, shape: [usize; D]) -> Result<Tensor<B, D>, SpeechError> {
        let t = self.tensors.tensor(name).map_err(err)?;
        if t.shape() != shape || t.dtype() != Dtype::F32 {
            return Err(err(format!("{name}: unexpected tensor shape or type")));
        }
        let count = shape.iter().try_fold(1usize, |a, b| a.checked_mul(*b)).ok_or_else(|| err("tensor size overflow"))?;
        if count > 256 * 1024 * 1024 || t.data().len() != count.saturating_mul(4) {
            return Err(err("tensor exceeds allocation limit"));
        }
        let mut data = Vec::new();
        data.try_reserve_exact(count).map_err(|_| err("not enough memory for tensor"))?;
        data.extend(t.data().chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        if data.iter().any(|v| !v.is_finite()) {
            return Err(err(format!("{name}: non-finite weights")));
        }
        Ok(Tensor::from_data(TensorData::new(data, shape), &self.device))
    }
    pub fn linear(&self, name: &str, input: usize, output: usize, bias: bool) -> Result<Linear<B>, SpeechError> {
        Ok(Linear {
            weight: self.weight(&format!("{name}.weight"), [output, input])?,
            bias: if bias { Some(self.weight(&format!("{name}.bias"), [output])?) } else { None },
        })
    }
    pub fn norm(&self, name: &str, size: usize) -> Result<Norm<B>, SpeechError> {
        Ok(Norm { weight: self.weight(&format!("{name}.weight"), [size])?, bias: self.weight(&format!("{name}.bias"), [size])? })
    }
}
pub(crate) struct Linear<B: Backend> {
    pub weight: Tensor<B, 2>,
    pub bias: Option<Tensor<B, 1>>,
}
impl<B: Backend> Linear<B> {
    pub fn forward(&self, x: Tensor<B, 3>) -> Tensor<B, 3> {
        let y = x.matmul(self.weight.clone().transpose().unsqueeze::<3>());
        match &self.bias {
            Some(b) => y + b.clone().unsqueeze::<3>(),
            None => y,
        }
    }
}
pub(crate) struct Norm<B: Backend> {
    weight: Tensor<B, 1>,
    bias: Tensor<B, 1>,
}
impl<B: Backend> Norm<B> {
    pub fn forward(&self, x: Tensor<B, 3>) -> Tensor<B, 3> {
        let centered = x.clone() - x.mean_dim(2);
        let variance = centered.clone().powf_scalar(2.0).mean_dim(2);
        centered / (variance + 1e-5).sqrt() * self.weight.clone().unsqueeze::<3>() + self.bias.clone().unsqueeze::<3>()
    }
}
pub(crate) fn vector<B: Backend, const D: usize>(x: Tensor<B, D>) -> Result<Vec<f32>, SpeechError> {
    x.into_data().to_vec::<f32>().map_err(|e| err(format!("{e:?}")))
}
pub(crate) fn guarded<T>(f: impl FnOnce() -> Result<T, SpeechError>) -> Result<T, SpeechError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(err("inference backend failed; try CPU or a smaller model")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use burn::backend::NdArray;
    #[test]
    fn tensor_artifacts_survive_truncation_bit_flips_and_corrupt_sizes() {
        let values = [1.0f32, 2.0, 3.0, 4.0];
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let view = safetensors::tensor::TensorView::new(Dtype::F32, vec![2, 2], &data).unwrap();
        let bytes = safetensors::tensor::serialize([("weight", view)], None).unwrap();
        let good = Loader::<NdArray<f32>>::new(&bytes, Default::default()).unwrap();
        assert!(good.weight("weight", [2, 2]).is_ok());
        let check = |raw: &[u8]| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(loader) = Loader::<NdArray<f32>>::new(raw, Default::default()) {
                    let _ = loader.weight("weight", [2, 2]);
                }
            }))
            .unwrap();
        };
        for end in 0..bytes.len() {
            check(&bytes[..end]);
        }
        for index in 0..bytes.len() {
            let mut corrupt = bytes.clone();
            corrupt[index] ^= 0xff;
            check(&corrupt);
        }
        let mut corrupt = bytes;
        corrupt[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        check(&corrupt);
    }
}
