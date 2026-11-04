//! The speech model catalogue and the model store.
//!
//! Models are downloaded on first use after choosing Transcribe (the picker shows size, source
//! and licence), into `<data dir>/models/<id>/`. They are never bundled with
//! FilmCraft or committed to the repository. Every file is pinned to a Hugging Face revision of
//! the publishers' repositories and checked against its SHA-256 before it is used.
//!
//! Licence: OpenAI released the Whisper code and model weights under the MIT licence
//! (<https://github.com/openai/whisper/blob/main/LICENSE>); the safetensors conversions OpenAI
//! publishes at `huggingface.co/openai/whisper-*` are labelled Apache-2.0. Both are permissive.
//! Orukeet's published weights are CC BY-SA 4.0; its model card supplies attribution and provenance.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::Digest;

/// One file of a model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// A downloadable speech model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Recognises languages other than English (and detects the language).
    pub multilingual: bool,
    pub description: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub author: &'static str,
    /// Human-readable source page.
    pub source: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelInfo {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    pub fn backend(&self) -> &'static str {
        if self.id == "orukeet" { "fastconformer-tdt" } else { "whisper" }
    }
    pub fn language_codes(&self) -> Option<&'static [&'static str]> {
        if self.id == "orukeet" { Some(ORUKEET_LANGUAGES) } else { None }
    }
    /// Conservative combined weight/working-buffer estimate, not a measured VRAM requirement.
    pub fn estimated_memory_bytes(&self) -> u64 {
        self.size().saturating_mul(4)
    }
}

pub const ORUKEET_LANGUAGES: &[&str] =
    &["bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt", "pl", "pt", "ro", "ru", "sk", "sl", "es", "sv", "uk"];

const LICENSE: &str = "MIT (OpenAI Whisper weights); Hugging Face conversion: Apache-2.0";
const LICENSE_URL: &str = "https://github.com/openai/whisper/blob/main/LICENSE";
const AUTHOR: &str = "OpenAI";

macro_rules! hf {
    ($repo:literal, $rev:literal, $name:literal, $sha:literal, $size:expr) => {
        ModelFile { name: $name, url: concat!("https://huggingface.co/openai/", $repo, "/resolve/", $rev, "/", $name), sha256: $sha, size: $size }
    };
}

const TOKENIZER_MULTI: &str = "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566";

static CATALOGUE: &[ModelInfo] = &[
    ModelInfo {
        id: "orukeet",
        name: "Orukeet",
        multilingual: true,
        description: "627 M parameters. Accuracy-focused multilingual recognition; GPU recommended. Supports 25 European languages.",
        license: "CC-BY-SA-4.0",
        license_url: "https://huggingface.co/oruk/orukeet#license-and-attribution",
        author: "Oruk AI and contributors",
        source: "https://huggingface.co/oruk/orukeet",
        files: &[
            ModelFile {
                name: "config.json",
                url: "https://huggingface.co/oruk/orukeet/resolve/b59c13a733fce5cf193230d0fa317ee7f145a108/config.json",
                sha256: "a0075728c5f206b298006343ffa01db0016230832f1e4022f541887d3d27ac32",
                size: 1149,
            },
            ModelFile {
                name: "processor_config.json",
                url: "https://huggingface.co/oruk/orukeet/resolve/b59c13a733fce5cf193230d0fa317ee7f145a108/processor_config.json",
                sha256: "7d1787398cffb800fa2cb58750781a78fe668dc02469750ee290840bd54cd8f1",
                size: 417,
            },
            ModelFile {
                name: "tokenizer.json",
                url: "https://huggingface.co/oruk/orukeet/resolve/b59c13a733fce5cf193230d0fa317ee7f145a108/tokenizer.json",
                sha256: "45a7f44c5e012cfd09e72b333274370ba7785740d27fb1486364e0bb7bbf8b1a",
                size: 1159960,
            },
            ModelFile {
                name: "model.safetensors",
                url: "https://huggingface.co/oruk/orukeet/resolve/b59c13a733fce5cf193230d0fa317ee7f145a108/model.safetensors",
                sha256: "092cff73445117d4d234c5e4fb37d86ad16a348be3426e10ff38c79d35758569",
                size: 2508311120,
            },
        ],
    },
    ModelInfo {
        id: "whisper-tiny",
        name: "Whisper tiny (multilingual)",
        multilingual: true,
        description: "39 M parameters. Lowest memory use; fast on CPU for clear speech, less robust on noisy audio.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-tiny",
        files: &[
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "config.json",
                "ffdccec4f3211f4c63310f2b7098f309fe70f3952cedc5e4d11e43f5b2379b98",
                1983
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "generation_config.json",
                "a5d5325911f16e74001a72fa13d6e208eee51548f994646de1f4b4cc8b35b512",
                3747
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-tiny",
                "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
                "model.safetensors",
                "7ebd0e69e78190ffe1438491fa05cc1f5c1aa3a4c4db3bc1723adbb551ea2395",
                151061672
            ),
        ],
    },
    ModelInfo {
        id: "whisper-base",
        name: "Whisper base (multilingual)",
        multilingual: true,
        description: "74 M parameters. Balances resource use and recognition quality, with broad language coverage.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-base",
        files: &[
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "config.json",
                "a153c53883a6799b6f056b4a8d1a515c9926d03994682ba88a7616618d7da0c1",
                1983
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "generation_config.json",
                "444b3f636d2fff89dd9ecf549e2a085b61f7ff0fa0246d4628bac6a3b8cc9ba4",
                3807
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-base",
                "e37978b90ca9030d5170a5c07aadb050351a65bb",
                "model.safetensors",
                "07cadb9f25677c8d50df603e66a98fbd842cce45047139baeb16e6219a1e807b",
                290403936
            ),
        ],
    },
    ModelInfo {
        id: "whisper-small",
        name: "Whisper small (multilingual)",
        multilingual: true,
        description: "244 M parameters. A more capable Whisper model for difficult audio, with higher memory and compute costs.",
        license: LICENSE,
        license_url: LICENSE_URL,
        author: AUTHOR,
        source: "https://huggingface.co/openai/whisper-small",
        files: &[
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "config.json",
                "e6a2b489da1b5aed65a8eb8d1e7466fa867ad5643a8bc138ba708bd56b2875c4",
                1967
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "generation_config.json",
                "71565b8ef50d0bf7a1193ed4bbed195b94e70c18894d81bba2f1233dcec3ab53",
                3868
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "tokenizer.json",
                "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
                2480466
            ),
            hf!(
                "whisper-small",
                "973afd24965f72e36ca33b3055d56a652f456b4d",
                "model.safetensors",
                "1d7734884874f1a1513ed9aa760a4f8e97aaa02fd6d93a3a85d27b2ae9ca596b",
                966995080
            ),
        ],
    },
];

/// The default model.
pub const DEFAULT_MODEL: &str = "orukeet";

pub fn catalogue() -> &'static [ModelInfo] {
    CATALOGUE
}

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    CATALOGUE.iter().find(|m| m.id == id)
}

/// Where model `m` lives under `models_dir`.
pub fn model_dir(models_dir: &Path, m: &ModelInfo) -> PathBuf {
    models_dir.join(m.id)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && Path::new(name).file_name().is_some_and(|part| part == name)
}

fn valid_file(path: &Path, file: &ModelFile) -> bool {
    if !valid_name(file.name) || !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.size) {
        return false;
    }
    let Ok(mut input) = std::fs::File::open(path) else { return false };
    let mut hasher = sha2::Sha256::new();
    if std::io::copy(&mut std::io::Read::take(&mut input, file.size.saturating_add(1)), &mut hasher).is_err() {
        return false;
    }
    format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(file.sha256)
}

/// All files present and matching their pinned size and SHA-256.
pub fn installed(models_dir: &Path, m: &ModelInfo) -> bool {
    let d = model_dir(models_dir, m);
    m.files.iter().all(|f| valid_file(&d.join(f.name), f))
}

/// Bytes still to download for `m` (files already present are skipped).
pub fn missing_bytes(models_dir: &Path, m: &ModelInfo) -> u64 {
    let d = model_dir(models_dir, m);
    m.files.iter().filter(|f| !valid_file(&d.join(f.name), f)).map(|f| f.size).sum()
}

/// The tokenizer shared by the multilingual models has one checksum.
pub fn shared_tokenizer_sha() -> &'static str {
    TOKENIZER_MULTI
}

/// Download progress: `(bytes done, bytes total, file)`; return false to cancel.
#[cfg(feature = "download")]
pub type DownloadProgress<'a> = &'a mut dyn FnMut(u64, u64, &str) -> bool;

/// Download the missing files of `m` into `models_dir/<id>/`: each file streams into a unique temporary file,
/// its SHA-256 is checked, then it is renamed into place. A wrong checksum deletes the part file
/// and fails. Pure Rust TLS (rustls + RustCrypto) with the operating system's certificate
/// verifier.
#[cfg(feature = "download")]
pub fn download(models_dir: &Path, m: &ModelInfo, progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    download_files(&model_dir(models_dir, m), m.files, progress)
}

/// [`download`] for any pinned file list (the neural voices use it too): missing files of
/// `files` go into `dir`, each verified against its SHA-256 and size before it is atomically installed into
/// place.
#[cfg(feature = "download")]
pub fn download_files(dir: &Path, files: &[ModelFile], progress: DownloadProgress) -> Result<(), crate::SpeechError> {
    use crate::SpeechError;
    if let Some(file) = files.iter().find(|file| !valid_name(file.name)) {
        return Err(SpeechError::Download(format!("invalid model artifact name: {:?}", file.name)));
    }
    if files.len() > 64 || files.iter().any(|f| f.size > 8 * 1024 * 1024 * 1024) {
        return Err(SpeechError::Download("model artifact count or size exceeds the download limit".into()));
    }
    let total = files
        .iter()
        .filter(|f| !valid_file(&dir.join(f.name), f))
        .try_fold(0u64, |sum, f| sum.checked_add(f.size))
        .filter(|total| *total <= 32 * 1024 * 1024 * 1024)
        .ok_or_else(|| SpeechError::Download("model download exceeds the total size limit".into()))?;
    if !progress(0, total, "Preparing download") {
        return Err(SpeechError::Cancelled);
    }
    std::fs::create_dir_all(dir)?;
    let agent = agent();
    let mut done = 0u64;
    for f in files {
        let dest = dir.join(f.name);
        if valid_file(&dest, f) {
            continue;
        }
        let resp = agent.get(f.url).call().map_err(|e| SpeechError::Download(format!("{}: {e}", f.url)))?;
        let mut body = resp.into_body();
        let mut reader = body.as_reader();
        receive_file(dir, f, &mut reader, progress, done, total)?;
        done += f.size;
    }
    Ok(())
}

#[cfg(feature = "download")]
fn receive_file(
    dir: &Path,
    f: &ModelFile,
    reader: &mut dyn std::io::Read,
    progress: DownloadProgress,
    done: u64,
    total: u64,
) -> Result<(), crate::SpeechError> {
    use crate::SpeechError;
    use std::io::Write;
    // Unique files prevent concurrent jobs from overwriting each other's partial download.
    // The temporary file removes itself on every error path, including full disks and cancellation.
    let mut part = tempfile::Builder::new().prefix(".filmcraft-model-").tempfile_in(dir)?;
    let mut out = std::io::BufWriter::new(part.as_file_mut());
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| SpeechError::Download(format!("{}: {e}", f.name)))?;
        if n == 0 {
            break;
        }
        let next =
            got.checked_add(n as u64).filter(|v| *v <= f.size).ok_or_else(|| SpeechError::Download(format!("{}: download exceeds pinned size", f.name)))?;
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        got = next;
        if !progress(done + got, total, f.name) {
            return Err(SpeechError::Cancelled);
        }
    }
    out.flush()?;
    drop(out);
    part.as_file().sync_all()?;
    let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if sha != f.sha256 || got != f.size {
        return Err(SpeechError::Download(format!("{}: checksum mismatch (got {sha}, {got} bytes; expected {}, {} bytes)", f.name, f.sha256, f.size)));
    }
    part.persist(dir.join(f.name)).map_err(|e| SpeechError::Download(format!("{}: cannot install verified model: {}", f.name, e.error)))?;
    Ok(())
}

#[cfg(feature = "download")]
fn agent() -> ureq::Agent {
    let provider = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_resolve(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_per_read(Some(std::time::Duration::from_secs(10)))
        .timeout_global(Some(std::time::Duration::from_secs(3600)))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_is_pinned_and_checksummed() {
        assert!(find(DEFAULT_MODEL).is_some());
        for m in catalogue() {
            assert!(m.size() > 100_000_000, "{}", m.id);
            for f in m.files {
                assert!(f.url.starts_with("https://huggingface.co/openai/whisper-") || f.url.starts_with("https://huggingface.co/oruk/orukeet/"), "{}", f.url);
                assert!(f.url.contains("/resolve/") && f.url.ends_with(f.name));
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                if f.name == "tokenizer.json" && m.id.starts_with("whisper-") {
                    assert_eq!(f.sha256, shared_tokenizer_sha());
                }
            }
        }
    }

    #[test]
    fn installed_checks_sizes() {
        let dir = std::env::temp_dir().join(format!("filmcraft-speech-models-{}", std::process::id()));
        let m = find("whisper-tiny").unwrap();
        assert!(!installed(&dir, m));
        assert_eq!(missing_bytes(&dir, m), m.size());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_names_stay_inside_the_model_directory() {
        for name in ["../escape", "/tmp/escape", r"..\escape", r"C:\escape", "", ".", ".."] {
            assert!(!valid_name(name), "accepted {name:?}");
        }
        assert!(valid_name("model.safetensors"));
    }

    #[test]
    fn same_length_corruption_is_not_installed() {
        static FILES: &[ModelFile] = &[ModelFile {
            name: "tiny.bin",
            url: "https://example.invalid/tiny.bin",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            size: 3,
        }];
        let model = ModelInfo {
            id: "integrity-test",
            name: "integrity test",
            multilingual: false,
            description: "",
            license: "",
            license_url: "",
            author: "",
            source: "",
            files: FILES,
        };
        let root = tempfile::tempdir().unwrap();
        let directory = model_dir(root.path(), &model);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("tiny.bin"), b"bad").unwrap();
        assert!(!installed(root.path(), &model));
        assert_eq!(missing_bytes(root.path(), &model), 3);
        std::fs::write(directory.join("tiny.bin"), b"abc").unwrap();
        assert!(installed(root.path(), &model));
    }

    #[cfg(feature = "download")]
    #[test]
    fn downloads_clean_up_interruption_corruption_and_cancellation_then_retry() {
        let root = tempfile::tempdir().unwrap();
        let file = ModelFile {
            name: "tiny.bin",
            url: "https://example.invalid/weights",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        for (body, cancel) in [(b"ab".as_slice(), false), (b"bad".as_slice(), false), (b"abc".as_slice(), true), (b"abcd".as_slice(), false)] {
            let result = receive_file(root.path(), &file, &mut std::io::Cursor::new(body), &mut |done, _, _| !cancel || done == 0, 0, 3);
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0, "partial artifacts must be removed");
        }
        // A retry must replace a corrupt existing destination, including on Windows.
        std::fs::write(root.path().join("tiny.bin"), b"bad").unwrap();
        receive_file(root.path(), &file, &mut std::io::Cursor::new(b"abc"), &mut |_, _, _| true, 0, 3).unwrap();
        assert_eq!(std::fs::read(root.path().join("tiny.bin")).unwrap(), b"abc");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
