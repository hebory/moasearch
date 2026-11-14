//! Typed application operations used by the desktop command bridge and CLI.
//! Long-running commands belong on a worker; this layer never depends on a UI or transport.

use crate::{
    commands::Command,
    config,
    engine::EmbeddedEngine,
    indexer::Indexer,
    models, ocr,
    search::{Search, canonical_folder, folder_contains_path},
    store::Store,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct AppServices {
    pub store: Arc<Store>,
    pub engine: Arc<EmbeddedEngine>,
    pub indexer: Arc<Indexer>,
    pub search: Arc<Search>,
    download: Mutex<Value>,
    download_cancel: Arc<AtomicBool>,
    download_worker: Mutex<Option<JoinHandle<()>>>,
    shutting_down: AtomicBool,
    lifecycle: Mutex<()>,
}

impl AppServices {
    pub fn new(store: Arc<Store>, engine: Arc<EmbeddedEngine>) -> Arc<Self> {
        Arc::new(Self {
            indexer: Indexer::new(store.clone(), engine.clone()),
            search: Arc::new(Search::new(store.clone(), engine.clone())),
            store,
            engine,
            download: Mutex::new(
                json!({"running":false,"kind":"","received":0,"total":0,"error":"","message":""}),
            ),
            download_cancel: Arc::new(AtomicBool::new(false)),
            download_worker: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            lifecycle: Mutex::new(()),
        })
    }

    pub fn execute(self: &Arc<Self>, command: crate::commands::Command) -> Result<Value> {
        if self.shutting_down.load(Ordering::Acquire) && !matches!(&command, Command::Quit {}) {
            bail!("모아검색을 종료하는 중입니다.");
        }
        match command {
            Command::Status {} => self.status(),
            Command::Scope { path } => self.scope(&path),
            Command::AddRoot { path } => {
                let _lifecycle = self.lifecycle.lock().unwrap();
                anyhow::ensure!(
                    !self.shutting_down.load(Ordering::Acquire),
                    "모아검색을 종료하는 중입니다."
                );
                let _guard = self.indexer.mutation.lock().unwrap();
                self.indexer.ensure_idle()?;
                self.store.add_root(&path)
            }
            Command::RemoveRoot { id } => {
                let _lifecycle = self.lifecycle.lock().unwrap();
                anyhow::ensure!(
                    !self.shutting_down.load(Ordering::Acquire),
                    "모아검색을 종료하는 중입니다."
                );
                let _guard = self.indexer.mutation.lock().unwrap();
                self.indexer.ensure_idle()?;
                self.store.remove_root(id)?;
                Ok(json!({"ok":true}))
            }
            Command::StartIndex {} => {
                let _lifecycle = self.lifecycle.lock().unwrap();
                anyhow::ensure!(
                    !self.shutting_down.load(Ordering::Acquire),
                    "모아검색을 종료하는 중입니다."
                );
                Ok(json!({"started":self.indexer.start()}))
            }
            Command::CancelIndex {} => {
                self.indexer.cancel();
                Ok(json!({"ok":true}))
            }
            Command::UpdateSettings { settings } => self.update_settings(settings),
            Command::Search(input) => {
                input.validate()?;
                self.search.search_in(
                    &input.query,
                    input.limit,
                    input.extension.as_deref(),
                    &input.mode,
                    input.folder.as_deref(),
                )
            }
            Command::Answer(input) => {
                input.validate()?;
                self.search
                    .answer_in(&input.query, input.limit.min(8), input.folder.as_deref())
            }
            Command::File { id } => self
                .store
                .file(id)?
                .ok_or_else(|| anyhow::anyhow!("색인된 파일을 찾을 수 없습니다.")),
            Command::OpenFile { id } => {
                let path = action_path(self, id)?;
                let extension = path
                    .extension()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase();
                anyhow::ensure!(
                    [
                        "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "hwp", "hwpx", "txt",
                        "md", "csv", "png", "jpg", "jpeg", "gif", "webp", "mp3", "wav", "mp4",
                        "mkv", "bmp", "tif", "tiff"
                    ]
                    .contains(&extension.as_str()),
                    "이 형식은 폴더에서 확인하세요. 실행 파일과 스크립트는 실행하지 않습니다."
                );
                shell_open(&path.to_string_lossy())?;
                Ok(json!({"ok":true}))
            }
            Command::RevealFile { id } => {
                let path = action_path(self, id)?;
                std::process::Command::new("explorer.exe")
                    .arg("/select,")
                    .arg(path)
                    .spawn()?;
                Ok(json!({"ok":true}))
            }
            Command::DownloadModel { kind } => self.start_download(kind),
            Command::Quit {} => {
                self.begin_shutdown();
                Ok(json!({"ok":true}))
            }
        }
    }

    fn status(&self) -> Result<Value> {
        Ok(json!({
            "counts":self.store.counts()?, "roots":self.store.roots()?,
            "settings":self.store.settings()?, "index":self.indexer.status(),
            "engine":self.engine.status(), "ocr":ocr::status(&self.store.data_dir.join("models")),
            "suggestions":suggestions(), "model_download":self.download.lock().unwrap().clone()
        }))
    }

    fn scope(&self, path: &Path) -> Result<Value> {
        let folder = canonical_folder(path)?;
        let roots = self.store.roots()?;
        let covering = roots
            .iter()
            .filter_map(|root| root["path"].as_str())
            .find(|root| folder_contains_path(Path::new(root), &folder));
        let registered = roots
            .iter()
            .filter_map(|root| root["path"].as_str())
            .any(|root| {
                config::within(&folder, Path::new(root)) && config::within(Path::new(root), &folder)
            });
        Ok(
            json!({"path":folder,"indexed":self.store.has_files_in(&folder)?,
            "registered":registered,"covered":covering.is_some(),"root_path":covering}),
        )
    }

    fn update_settings(&self, settings: Value) -> Result<Value> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::Acquire),
            "모아검색을 종료하는 중입니다."
        );
        let _guard = self.indexer.mutation.lock().unwrap();
        self.indexer.ensure_idle()?;
        anyhow::ensure!(
            !self.download.lock().unwrap()["running"]
                .as_bool()
                .unwrap_or(false),
            "모델 다운로드가 끝난 뒤 설정을 저장하세요."
        );
        self.store.update_settings(&settings)?;
        self.engine.reset();
        self.store.settings()
    }

    fn start_download(self: &Arc<Self>, kind: String) -> Result<Value> {
        let _lifecycle = self.lifecycle.lock().unwrap();
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::Acquire),
            "모아검색을 종료하는 중입니다."
        );
        anyhow::ensure!(
            ["embedding", "chat", "ocr"].contains(&kind.as_str()),
            "알 수 없는 모델입니다."
        );
        let mut download = self.download.lock().unwrap();
        anyhow::ensure!(
            download["running"] != true,
            "모델을 이미 다운로드 중입니다."
        );
        if let Some(worker) = self.download_worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("모델 다운로드 thread가 비정상 종료되었습니다."))?;
        }
        self.download_cancel.store(false, Ordering::Release);
        *download = json!({"running":true,"kind":kind,"received":0,"total":0,"error":"","message":"로컬 모델 준비 중"});
        let services = self.clone();
        let worker_kind = kind.clone();
        let worker = match thread::Builder::new()
            .name("moasearch-model-download".into())
            .spawn(move || {
                let result = services.download_model_worker(&worker_kind);
                let mut state = services.download.lock().unwrap();
                state["running"] = json!(false);
                match result {
                    Ok(()) => {
                        state["message"] = json!(match worker_kind.as_str() {
                            "ocr" => "PP-OCRv5 준비 완료. 다시 색인하면 이미지와 스캔 PDF 본문을 읽습니다.",
                            "embedding" => "EmbeddingGemma 2 Q8_0 준비 완료. 다시 색인하면 의미 검색에 사용합니다.",
                            _ => "Gemma 4 E2B IT QAT Q4_0 준비 완료. 검색한 문서를 근거로 답변합니다.",
                        })
                    }
                    Err(error) => {
                        if services.download_cancel.load(Ordering::Acquire) {
                            state["message"] = json!("모델 다운로드를 취소했습니다.");
                            state["error"] = json!("");
                        } else {
                            state["error"] = json!(error.to_string());
                        }
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                download["running"] = json!(false);
                return Err(error.into());
            }
        };
        *self.download_worker.lock().unwrap() = Some(worker);
        Ok(json!({"started":true}))
    }

    fn download_model_worker(&self, kind: &str) -> Result<()> {
        let model = models::download_model_cancellable(
            kind,
            &self.store.data_dir.join("models"),
            &|received, total| {
                let mut state = self.download.lock().unwrap();
                state["received"] = json!(received);
                state["total"] = json!(total);
            },
            &|| self.download_cancel.load(Ordering::Acquire),
        )?;
        if self.download_cancel.load(Ordering::Acquire) {
            bail!("모델 다운로드를 취소했습니다.");
        }
        loop {
            if self.download_cancel.load(Ordering::Acquire) {
                bail!("모델 다운로드를 취소했습니다.");
            }
            {
                let _guard = self.indexer.mutation.lock().unwrap();
                if self.indexer.ensure_idle().is_ok() {
                    if kind != "ocr" {
                        self.store
                            .update_settings(&json!({format!("{kind}_model"):model}))?;
                        self.engine.reset();
                    }
                    return Ok(());
                }
            }
            self.download.lock().unwrap()["message"] =
                json!("다운로드 완료. 현재 색인 종료 후 적용합니다.");
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn begin_shutdown(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap();
        if !self.shutting_down.swap(true, Ordering::AcqRel) {
            self.indexer.close();
            self.download_cancel.store(true, Ordering::Release);
        }
    }

    pub fn wait_shutdown(&self) -> Result<()> {
        self.begin_shutdown();
        while self.indexer.status()["running"] == true {
            thread::sleep(Duration::from_millis(100));
        }
        if let Some(worker) = self.download_worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("모델 다운로드 thread가 비정상 종료되었습니다."))?;
        }
        Ok(())
    }
}

fn suggestions() -> Vec<Value> {
    let mut result = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        for (name, label) in [
            ("Documents", "문서"),
            ("Desktop", "바탕 화면"),
            ("Downloads", "다운로드"),
        ] {
            let path = Path::new(&home).join(name);
            if path.is_dir() {
                result.push(json!({"label":label,"path":path}));
            }
        }
    }
    #[cfg(windows)]
    for drive in 'A'..='Z' {
        let path = format!("{drive}:\\");
        if Path::new(&path).is_dir() {
            result.push(json!({"label":format!("{drive}: 드라이브"),"path":path}));
        }
    }
    result
}

fn action_path(services: &AppServices, id: i64) -> Result<PathBuf> {
    let file = services
        .store
        .file(id)?
        .ok_or_else(|| anyhow::anyhow!("색인된 파일을 찾을 수 없습니다."))?;
    let path = Path::new(file["path"].as_str().unwrap_or(""));
    let resolved = config::canonical(path)?;
    anyhow::ensure!(
        path.is_file()
            && resolved == path
            && config::safe_metadata(&std::fs::symlink_metadata(path)?)
            && !config::excluded(&resolved),
        "파일이 이동되었거나 검색 범위 밖입니다."
    );
    anyhow::ensure!(
        services
            .store
            .roots()?
            .iter()
            .any(|root| config::within(&resolved, Path::new(root["path"].as_str().unwrap_or("")))),
        "파일이 검색 범위 밖입니다."
    );
    Ok(resolved)
}

pub fn shell_open(target: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::PCWSTR,
        };
        let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            bail!("기본 앱으로 열지 못했습니다.");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        bail!("Windows에서만 지원합니다.")
    }
}
