//! Pinned, on-device Google models. Downloads are authenticated before install.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_MODEL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub const OCR_DICT: &str = include_str!("../assets/ocr/korean_ppocrv5_dict.txt");
pub struct OcrModelSpec {
    pub filename: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub url: &'static str,
}
// The same RapidOCR v3.10.0 artifacts verified by the previous implementation.
pub const OCR_DET: OcrModelSpec = OcrModelSpec {
    filename: "ch_PP-OCRv5_det_mobile.onnx",
    bytes: 4_819_576,
    sha256: "4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae",
    url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/det/ch_PP-OCRv5_det_mobile.onnx",
};
pub const OCR_REC: OcrModelSpec = OcrModelSpec {
    filename: "korean_PP-OCRv5_rec_mobile.onnx",
    bytes: 13_488_748,
    sha256: "cd6e2ea50f6943ca7271eb8c56a877a5a90720b7047fe9c41a2e541a25773c9b",
    url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/rec/korean_PP-OCRv5_rec_mobile.onnx",
};

pub fn ocr_paths(directory: &Path) -> (PathBuf, PathBuf) {
    (
        directory.join(OCR_DET.filename),
        directory.join(OCR_REC.filename),
    )
}

pub fn verify_ocr_models(directory: &Path) -> Result<(PathBuf, PathBuf)> {
    let paths = ocr_paths(directory);
    for (path, spec) in [(&paths.0, &OCR_DET), (&paths.1, &OCR_REC)] {
        let metadata = fs::symlink_metadata(path).context("OCR 모델을 먼저 준비하세요.")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != spec.bytes
        {
            bail!(
                "OCR 모델 파일 크기 또는 경로가 올바르지 않습니다: {}",
                spec.filename
            );
        }
        if sha256_file(path)? != spec.sha256 {
            bail!("OCR 모델 SHA-256 검증에 실패했습니다: {}", spec.filename);
        }
    }
    Ok(paths)
}

fn download_ocr_models(
    directory: &Path,
    progress: &dyn Fn(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let total = OCR_DET.bytes + OCR_REC.bytes;
    let mut completed = 0;
    for spec in [&OCR_DET, &OCR_REC] {
        anyhow::ensure!(!cancelled(), "모델 다운로드를 취소했습니다.");
        let target = directory.join(spec.filename);
        if fs::symlink_metadata(&target).is_ok() {
            let metadata = fs::symlink_metadata(&target)?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != spec.bytes
                || sha256_file(&target)? != spec.sha256
            {
                bail!(
                    "기존 OCR 파일이 고정 해시와 달라 덮어쓰지 않습니다: {}",
                    target.display()
                );
            }
        } else {
            let client = Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
            let mut response = client.get(spec.url).send()?.error_for_status()?;
            let temporary =
                directory.join(format!(".{}.{}.part", spec.filename, std::process::id()));
            let created = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let mut cleanup = TempFile::new(temporary.clone());
            let mut output = created;
            let mut digest = Sha256::new();
            let mut received = 0;
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                anyhow::ensure!(!cancelled(), "모델 다운로드를 취소했습니다.");
                let n = response.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                received += n as u64;
                if received > spec.bytes {
                    bail!("OCR 모델이 허용된 크기를 초과했습니다.");
                }
                digest.update(&buffer[..n]);
                output.write_all(&buffer[..n])?;
                progress(completed + received, total);
            }
            output.sync_all()?;
            drop(output);
            if received != spec.bytes || hex_digest(&digest.finalize()) != spec.sha256 {
                bail!("OCR 모델 크기 또는 SHA-256 검증에 실패했습니다.");
            }
            install_without_overwrite(&temporary, &target)?;
            cleanup.disarm();
        }
        completed += spec.bytes;
        progress(completed, total);
    }
    verify_ocr_models(directory)?;
    Ok(directory.to_path_buf())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelKind {
    Embedding,
    Chat,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelSpec {
    pub kind: ModelKind,
    pub repo: &'static str,
    pub revision: &'static str,
    pub filename: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub description: &'static str,
}

pub const EMBEDDING: ModelSpec = ModelSpec {
    kind: ModelKind::Embedding,
    repo: "ggml-org/embeddinggemma-2-GGUF",
    revision: "6ea3e3a309d12a4779cfda98b8c19800d2d5da71",
    filename: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    description: "Google EmbeddingGemma 2 Q8_0 의미 검색 모델 (약 310 MB)",
};

pub const CHAT: ModelSpec = ModelSpec {
    kind: ModelKind::Chat,
    repo: "google/gemma-4-E2B-it-qat-q4_0-gguf",
    revision: "675cff42a74c774d6cb76f76d8eacb49b48c9b93",
    filename: "gemma-4-E2B_q4_0-it.gguf",
    bytes: 3_349_516_256,
    sha256: "fa401b55b07ee70a54c6dae3903c783a6e65064312529ea57175cb5f8dec6634",
    description: "Google Gemma 4 E2B IT QAT Q4_0 답변 모델 (약 3.35 GB)",
};

impl ModelKind {
    pub fn from_str(value: &str) -> Result<Self> {
        match value {
            "embedding" => Ok(Self::Embedding),
            "chat" => Ok(Self::Chat),
            _ => bail!("모델 종류는 embedding 또는 chat이어야 합니다."),
        }
    }

    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::Embedding => &EMBEDDING,
            Self::Chat => &CHAT,
        }
    }

    pub fn settings_key(self) -> &'static str {
        match self {
            Self::Embedding => "embedding_model",
            Self::Chat => "chat_model",
        }
    }
}

#[derive(Serialize)]
struct PublicModel<'a> {
    repo: &'a str,
    revision: &'a str,
    filename: &'a str,
    bytes: u64,
    sha256: &'a str,
    description: &'a str,
    source: String,
}

pub fn catalog() -> Value {
    let public = |spec: &ModelSpec| {
        serde_json::to_value(PublicModel {
            repo: spec.repo,
            revision: spec.revision,
            filename: spec.filename,
            bytes: spec.bytes,
            sha256: spec.sha256,
            description: spec.description,
            source: format!(
                "https://huggingface.co/{}/tree/{}",
                spec.repo, spec.revision
            ),
        })
        .expect("static model catalog serializes")
    };
    json!({"embedding":public(&EMBEDDING),"chat":public(&CHAT),
        "ocr":{"description":"한국어·영어 PP-OCRv5 전용 모델 (Rust ONNX 추론)","bytes":OCR_DET.bytes+OCR_REC.bytes,
        "files":[{"filename":OCR_DET.filename,"sha256":OCR_DET.sha256,"url":OCR_DET.url},
        {"filename":OCR_REC.filename,"sha256":OCR_REC.sha256,"url":OCR_REC.url}]}})
}

pub fn model_path(directory: &Path, kind: ModelKind) -> PathBuf {
    directory.join(kind.spec().filename)
}

/// Download a fixed revision from Hugging Face and atomically install only a
/// byte-for-byte verified GGUF. Invalid same-name files are left untouched.
pub fn download_model(
    kind: &str,
    directory: &Path,
    progress: &dyn Fn(u64, u64),
) -> Result<PathBuf> {
    download_model_cancellable(kind, directory, progress, &|| false)
}

/// Download the same pinned models as `download_model`, checking cancellation
/// between reads and using bounded network waits so shutdown can join promptly.
pub fn download_model_cancellable(
    kind: &str,
    directory: &Path,
    progress: &dyn Fn(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> Result<PathBuf> {
    if kind == "ocr" {
        return download_ocr_models(directory, progress, cancelled);
    }
    anyhow::ensure!(!cancelled(), "모델 다운로드를 취소했습니다.");
    let kind = ModelKind::from_str(kind)?;
    fs::create_dir_all(directory)
        .with_context(|| format!("모델 폴더를 만들 수 없습니다: {}", directory.display()))?;
    let spec = kind.spec();
    let target = directory.join(spec.filename);

    if fs::symlink_metadata(&target).is_ok() {
        verify_model(&target, kind)?;
        progress(spec.bytes, spec.bytes);
        return Ok(target);
    }

    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}?download=true",
        spec.repo, spec.revision, spec.filename
    );
    let client = Client::builder()
        .user_agent(concat!("MoaSearch/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut response = client.get(url).send()?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length != spec.bytes)
    {
        bail!("모델 서버가 예상하지 않은 파일 크기를 반환했습니다.");
    }

    let temp = directory.join(format!(".{}.{}.part", spec.filename, std::process::id()));
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("임시 모델 파일을 만들 수 없습니다: {}", temp.display()))?;
    // Own cleanup only after create_new succeeds; preserve an existing .part.
    // Declare the output last so its handle closes before cleanup on errors.
    let mut cleanup = TempFile::new(temp.clone());
    let mut output = created;
    let mut digest = Sha256::new();
    let mut received = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        anyhow::ensure!(!cancelled(), "모델 다운로드를 취소했습니다.");
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| anyhow!("모델 크기가 범위를 넘었습니다."))?;
        if received > spec.bytes || received > MAX_MODEL_BYTES {
            bail!("다운로드 모델이 허용된 크기를 초과했습니다.");
        }
        digest.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
        progress(received, spec.bytes);
    }
    output.sync_all()?;
    drop(output);
    if received != spec.bytes {
        bail!(
            "다운로드 모델 크기가 올바르지 않습니다. ({received} / {})",
            spec.bytes
        );
    }
    let actual = hex_digest(digest.finalize().as_slice());
    if actual != spec.sha256 {
        bail!("{} SHA-256 검증에 실패했습니다.", spec.filename);
    }
    verify_gguf_magic(&temp)?;
    install_without_overwrite(&temp, &target)?;
    cleanup.disarm();
    Ok(target)
}

/// Authenticate a configured file against the exact pinned Google artifact.
pub fn verify_model(path: &Path, kind: ModelKind) -> Result<()> {
    let spec = kind.spec();
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{} 모델 파일을 찾을 수 없습니다.", kind.settings_key()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("기호 링크 또는 일반 파일이 아닌 모델은 허용하지 않습니다.");
    }
    if metadata.len() != spec.bytes || metadata.len() > MAX_MODEL_BYTES {
        bail!("{}의 고정 파일 크기와 일치하지 않습니다.", spec.filename);
    }
    verify_gguf_magic(path)?;
    let actual = sha256_file(path)?;
    if actual != spec.sha256 {
        bail!("{} SHA-256 검증에 실패했습니다.", spec.filename);
    }
    Ok(())
}

pub fn is_pinned_file(path: &Path, kind: ModelKind) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() == kind.spec().bytes
            && verify_gguf_magic(path).is_ok()
    })
}

fn verify_gguf_magic(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        bail!("모델 파일의 GGUF 서명이 올바르지 않습니다.");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex_digest(digest.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn install_without_overwrite(temp: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // Hard-link creation fails if a target appeared after download; it
        // never replaces an existing file and is atomic on one volume.
        fs::hard_link(temp, target).with_context(|| {
            format!(
                "같은 이름의 기존 파일이 있어 모델을 덮어쓰지 않았습니다: {}",
                target.display()
            )
        })?;
        fs::remove_file(temp)?;
    }
    #[cfg(not(windows))]
    fs::rename(temp, target)
        .with_context(|| format!("모델 파일을 설치할 수 없습니다: {}", target.display()))?;
    Ok(())
}

struct TempFile {
    path: PathBuf,
    armed: bool,
}
impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_existing_model_is_rejected_without_network_or_overwrite() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join(EMBEDDING.filename);
        fs::write(&target, b"GGUF-untrusted-existing-file")?;
        let result = download_model("embedding", directory.path(), &|_, _| {});
        assert!(result.is_err());
        assert_eq!(fs::read(target)?, b"GGUF-untrusted-existing-file");
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn concurrent_install_cannot_replace_existing_model() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("model.gguf");
        let temporary = directory.path().join("download.part");
        fs::write(&target, b"existing")?;
        fs::write(&temporary, b"new")?;
        assert!(install_without_overwrite(&temporary, &target).is_err());
        assert_eq!(fs::read(target)?, b"existing");
        assert_eq!(fs::read(temporary)?, b"new");
        Ok(())
    }
}
