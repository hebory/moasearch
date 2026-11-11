//! Bounded, atomic batches of completed files; one writer and cancellable scans.
use crate::{
    config,
    engine::Embedder,
    extract,
    store::{FileRecord, Store},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Indexer {
    store: Arc<Store>,
    engine: Arc<dyn Embedder>,
    state: Mutex<Value>,
    pub cancel_flag: AtomicBool,
    pub mutation: Mutex<()>,
    stop: AtomicBool,
}

fn initial() -> Value {
    json!({"running":false,"scanned":0,"indexed":0,"skipped":0,"errors":0,
    "current_path":"","current_page":0,"total_pages":0,"last_error":"","phase":"idle"})
}

impl Indexer {
    pub fn new(store: Arc<Store>, engine: Arc<dyn Embedder>) -> Arc<Self> {
        Arc::new(Self {
            store,
            engine,
            state: Mutex::new(initial()),
            cancel_flag: AtomicBool::new(false),
            mutation: Mutex::new(()),
            stop: AtomicBool::new(false),
        })
    }
    pub fn status(&self) -> Value {
        self.state.lock().unwrap().clone()
    }
    fn update(&self, values: Value) {
        let mut state = self.state.lock().unwrap();
        for (key, value) in values.as_object().unwrap() {
            state[key] = value.clone();
        }
    }
    fn error(&self, error: impl std::fmt::Display) {
        let mut state = self.state.lock().unwrap();
        state["errors"] = json!(state["errors"].as_u64().unwrap_or(0) + 1);
        state["last_error"] = json!(error.to_string().chars().take(500).collect::<String>());
    }
    pub fn ensure_idle(&self) -> Result<()> {
        anyhow::ensure!(
            !self.status()["running"].as_bool().unwrap_or(false),
            "색인이 끝나거나 취소된 뒤 설정과 폴더를 변경하세요."
        );
        Ok(())
    }
    pub fn start(self: &Arc<Self>) -> bool {
        let _guard = self.mutation.lock().unwrap();
        if self.ensure_idle().is_err() {
            return false;
        }
        self.cancel_flag.store(false, Ordering::Relaxed);
        *self.state.lock().unwrap() = initial();
        self.update(json!({"running":true,"phase":"scanning"}));
        let this = self.clone();
        std::thread::spawn(move || {
            let result = this.run();
            if let Err(error) = &result {
                this.error(error);
            }
            let phase = if this.cancelled() {
                "cancelled"
            } else if result.is_err() {
                "error"
            } else {
                "complete"
            };
            this.update(json!({"running":false,"phase":phase,"current_path":"","current_page":0,"total_pages":0}));
        });
        true
    }
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }
    fn cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::Relaxed)
    }
    pub fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cancel();
    }
    pub fn start_scheduler(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !this.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                let interval = this
                    .store
                    .settings()
                    .ok()
                    .and_then(|s| s["scan_interval"].as_u64())
                    .unwrap_or(0);
                if interval > 0 && last.elapsed() >= Duration::from_secs(interval) {
                    this.start();
                    last = Instant::now();
                }
            }
        });
    }
    fn flush(
        &self,
        db: &mut rusqlite::Connection,
        pending: &mut Vec<FileRecord>,
        indexed: &mut u64,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let records = std::mem::take(pending);
        self.store.replace_files_on(db, &records)?;
        *indexed += records.len() as u64;
        self.update(json!({"indexed":*indexed}));
        Ok(())
    }
    fn run(&self) -> Result<()> {
        let settings = self.store.settings()?;
        let mut model_id = self.engine.model_id();
        let file_limit = settings["max_file_mb"].as_u64().unwrap_or(32) * 1024 * 1024;
        let mut db = self.store.connect()?;
        let (mut scanned, mut skipped, mut indexed) = (0u64, 0u64, 0u64);
        for root in self.store.roots()? {
            if self.cancelled() {
                break;
            }
            let base = config::canonical(Path::new(root["path"].as_str().unwrap_or("")));
            let base = match base {
                Ok(p) if p.is_dir() => p,
                _ => {
                    self.error(format!("폴더에 접근할 수 없습니다: {}", root["path"]));
                    continue;
                }
            };
            let root_id = root["id"].as_i64().unwrap();
            let mut seen = HashSet::new();
            let mut failed = false;
            let mut directories = vec![base.clone()];
            let mut pending = Vec::new();
            let (mut pending_chunks, mut pending_bytes) = (0usize, 0usize);
            let mut since = Instant::now();
            while let Some(directory) = directories.pop() {
                if self.cancelled() {
                    break;
                }
                let entries = match std::fs::read_dir(&directory) {
                    Ok(e) => e,
                    Err(e) => {
                        failed = true;
                        self.error(e);
                        continue;
                    }
                };
                for entry in entries {
                    if self.cancelled() {
                        break;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            failed = true;
                            self.error(e);
                            continue;
                        }
                    };
                    let path = entry.path();
                    if config::excluded(&path) || config::within(&path, &self.store.data_dir) {
                        continue;
                    }
                    let metadata = match std::fs::symlink_metadata(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            failed = true;
                            self.error(e);
                            continue;
                        }
                    };
                    if !config::safe_metadata(&metadata) {
                        continue;
                    }
                    if metadata.is_dir() {
                        directories.push(path);
                        continue;
                    }
                    if !metadata.is_file() {
                        continue;
                    }
                    seen.insert(path.clone());
                    if !pending.is_empty() && since.elapsed() >= Duration::from_millis(500) {
                        self.flush(&mut db, &mut pending, &mut indexed)?;
                        pending_chunks = 0;
                        pending_bytes = 0;
                        since = Instant::now();
                    }
                    scanned += 1;
                    self.update(json!({"scanned":scanned,"current_path":path,"current_page":0,"total_pages":0}));
                    let modified = config::modified_ns(&metadata);
                    let reader = reader_id(&path);
                    let reader = if metadata.len() > file_limit {
                        format!("size-limit-v1:{}:{reader}", file_limit)
                    } else {
                        reader.to_owned()
                    };
                    let old = Store::file_by_path_on(&db, &path)?;
                    if let Some(old) = old {
                        if old["modified"].as_i64() == Some(modified)
                            && old["size"].as_u64() == Some(metadata.len())
                            && old["status"] != "error"
                            && old["reader_id"].as_str() == Some(&reader)
                            && (model_id.is_none()
                                || old["model_id"].as_str() == model_id.as_deref()
                                || old["status"] == "metadata")
                        {
                            skipped += 1;
                            self.update(json!({"skipped":skipped}));
                            continue;
                        }
                    }
                    let (mut passages, mut vectors, mut error) = (Vec::new(), None, None);
                    if metadata.len() <= file_limit && extract::supports_content(&path) {
                        let result = (|| -> Result<_> {
                            let resolved = config::canonical(&path)?;
                            anyhow::ensure!(
                                config::within(&resolved, &base) && !config::excluded(&resolved),
                                "색인 범위 밖으로 변경된 파일입니다."
                            );
                            let items = extract::extract_with_models(
                                &resolved,
                                200_000,
                                &self.cancel_flag,
                                &|page, total, phase| {
                                    self.update(json!({"current_page":page,"total_pages":total,"phase":phase}))
                                },
                                &self.store.data_dir.join("models"),
                            )?;
                            extract::chunk_passages(items, 1200, 160)
                        })();
                        match result {
                            Ok(items) => passages = items,
                            Err(e) => {
                                if self.cancelled() {
                                    break;
                                }
                                if let Some(partial) = e.downcast_ref::<extract::ExtractionError>()
                                {
                                    passages = extract::chunk_passages(
                                        partial.passages.clone(),
                                        1200,
                                        160,
                                    )?;
                                }
                                error = Some(e.to_string());
                                self.error(format!("{}: {e}", path.display()));
                            }
                        }
                        if error.is_none()
                            && !passages.is_empty()
                            && model_id.is_some()
                            && !self.cancelled()
                        {
                            self.update(json!({"phase":"embedding"}));
                            let mut all = Vec::new();
                            for group in passages.chunks(4) {
                                if self.cancelled() {
                                    break;
                                }
                                match self.engine.embed(
                                    &group.iter().map(|p| p.text.clone()).collect::<Vec<_>>(),
                                    false,
                                ) {
                                    Ok(values) if values.len() == group.len() => all.extend(values),
                                    result => {
                                        self.error(format!(
                                            "의미 색인 실패, 본문은 저장합니다: {}",
                                            result.err().map(|e| e.to_string()).unwrap_or_else(
                                                || "벡터 수가 일치하지 않습니다.".into()
                                            )
                                        ));
                                        model_id = None;
                                        all.clear();
                                        break;
                                    }
                                }
                            }
                            if !all.is_empty() {
                                vectors = Some(all);
                            }
                        }
                        self.update(json!({"phase":"scanning","current_page":0,"total_pages":0}));
                        match std::fs::symlink_metadata(&path) {
                            Ok(after)
                                if config::safe_metadata(&after)
                                    && config::modified_ns(&after) == modified
                                    && after.len() == metadata.len() => {}
                            _ => {
                                self.error(format!(
                                    "읽는 중 변경되어 다음 색인으로 미룹니다: {}",
                                    path.display()
                                ));
                                continue;
                            }
                        }
                    }
                    if self.cancelled() {
                        break;
                    }
                    pending_chunks += passages.len();
                    pending_bytes += passages
                        .iter()
                        .map(|p| p.text.len() + p.location.len())
                        .sum::<usize>()
                        + vectors
                            .as_ref()
                            .map(|v| v.iter().map(|v| v.len() * 4).sum::<usize>())
                            .unwrap_or(0);
                    if pending.is_empty() {
                        since = Instant::now();
                    }
                    let record_model = if vectors.is_some() {
                        model_id.clone()
                    } else {
                        None
                    };
                    pending.push(FileRecord {
                        root_id,
                        path,
                        size: metadata.len() as i64,
                        modified,
                        passages,
                        vectors,
                        model_id: record_model,
                        error,
                        reader_id: reader,
                    });
                    if pending.len() >= 32
                        || pending_chunks >= 256
                        || pending_bytes >= 4 * 1024 * 1024
                    {
                        self.flush(&mut db, &mut pending, &mut indexed)?;
                        pending_chunks = 0;
                        pending_bytes = 0;
                        since = Instant::now();
                    }
                }
            }
            self.flush(&mut db, &mut pending, &mut indexed)?;
            if !failed && !self.cancelled() {
                self.store.prune_on(&mut db, root_id, &seen)?;
            }
        }
        Ok(())
    }
}

fn reader_id(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "pdf" => "rust-pdf-ppocr-v1",
        "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp" => "rust-ppocrv5-v1",
        _ => "rust-content-v1",
    }
}
