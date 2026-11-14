//! Bounded, atomic batches of completed files; one writer and cancellable scans.
use crate::{
    config,
    engine::Embedder,
    extract,
    store::{FileRecord, ReusedContent, Store},
};
use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, Metadata},
    io::Read,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const CONTENT_KEY_VERSION: &str = "moasearch-content-v1";
const PASSAGE_VECTOR_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
struct PassageVectorCache {
    values: HashMap<String, Vec<f32>>,
    fifo: VecDeque<String>,
    bytes: usize,
    dimension: Option<usize>,
}

impl PassageVectorCache {
    fn get(&self, key: &str) -> Option<Vec<f32>> {
        self.values.get(key).cloned()
    }

    fn insert(&mut self, key: String, vector: Vec<f32>) -> bool {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return false;
        }
        if self
            .dimension
            .is_some_and(|dimension| dimension != vector.len())
        {
            return false;
        }
        if self.values.contains_key(&key) {
            return false;
        }
        let key_bytes = key.len().saturating_mul(2);
        let vector_bytes = vector.len().saturating_mul(std::mem::size_of::<f32>());
        // Count the key in both HashMap and FIFO, vector bytes, and a fixed
        // allowance for each entry's container metadata.
        let entry_bytes = key_bytes.saturating_add(vector_bytes).saturating_add(64);
        if entry_bytes > PASSAGE_VECTOR_CACHE_BYTES {
            return false;
        }
        while self.bytes.saturating_add(entry_bytes) > PASSAGE_VECTOR_CACHE_BYTES {
            let Some(oldest) = self.fifo.pop_front() else {
                break;
            };
            if let Some(value) = self.values.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(
                    oldest
                        .len()
                        .saturating_mul(2)
                        .saturating_add(value.len().saturating_mul(std::mem::size_of::<f32>()))
                        .saturating_add(64),
                );
            }
        }
        self.dimension.get_or_insert(vector.len());
        self.bytes += entry_bytes;
        self.fifo.push_back(key.clone());
        self.values.insert(key, vector);
        true
    }
}

fn pending_content(pending: &[FileRecord], key: &str) -> Option<ReusedContent> {
    pending
        .iter()
        .find(|record| record.content_key.as_deref() == Some(key) && record.error.is_none())
        .map(|record| ReusedContent {
            passages: record.passages.clone(),
            vectors: record.vectors.clone(),
            model_id: record.model_id.clone(),
        })
}

fn seed_passage_vectors(
    cache: &mut PassageVectorCache,
    content: &ReusedContent,
    model_id: Option<&str>,
    context_size: u64,
) {
    let (Some(model_id), Some(vectors)) = (model_id, content.vectors.as_ref()) else {
        return;
    };
    if content.model_id.as_deref() != Some(model_id) || vectors.len() != content.passages.len() {
        return;
    }
    for (passage, vector) in content.passages.iter().zip(vectors) {
        cache.insert(
            passage_vector_key(model_id, context_size, &passage.text),
            vector.clone(),
        );
    }
}

fn bump_reused_chunks(indexer: &Indexer, count: usize) {
    if count == 0 {
        return;
    }
    let mut state = indexer.state.lock().unwrap();
    state["reused_chunks"] = json!(state["reused_chunks"].as_u64().unwrap_or(0) + count as u64);
}

fn content_key_prefix(
    reader: &str,
    extension: &str,
    model_id: Option<&str>,
    context_size: u64,
) -> String {
    format!(
        "{CONTENT_KEY_VERSION}|reader={reader}|extension={extension}|model={}|context={context_size}|sha256=",
        model_id.unwrap_or("none")
    )
}

fn content_hash(
    path: &Path,
    base: &Path,
    expected: &Metadata,
    cancelled: &AtomicBool,
) -> Result<Option<String>> {
    let resolved = config::canonical(path)?;
    if !config::within(&resolved, base)
        || config::excluded(&resolved)
        || !file_matches_metadata(path, base, expected)
    {
        return Ok(None);
    }
    let file = File::open(&resolved)?;
    if !config::safe_metadata(&file.metadata()?) {
        return Ok(None);
    }
    let mut file = file.take(expected.len().saturating_add(1));
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected.len() {
            return Ok(None);
        }
        digest.update(&buffer[..read]);
    }
    if cancelled.load(Ordering::Acquire)
        || total != expected.len()
        || !file_matches_metadata(path, base, expected)
    {
        return Ok(None);
    }
    let current = config::canonical(path)?;
    if current != resolved {
        return Ok(None);
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn file_matches_metadata(path: &Path, base: &Path, expected: &Metadata) -> bool {
    let Ok(resolved) = config::canonical(path) else {
        return false;
    };
    if !config::within(&resolved, base) || config::excluded(&resolved) {
        return false;
    }
    let Ok(current) = fs::symlink_metadata(path) else {
        return false;
    };
    config::safe_metadata(&current)
        && current.is_file()
        && current.len() == expected.len()
        && config::modified_ns(&current) == config::modified_ns(expected)
}

fn passage_vector_key(model_id: &str, context_size: u64, text: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(model_id.as_bytes());
    digest.update([0]);
    digest.update(context_size.to_le_bytes());
    digest.update([0]);
    digest.update(text.as_bytes());
    format!("{:x}", digest.finalize())
}

fn reusable_compatible(content: &ReusedContent, model_id: Option<&str>) -> bool {
    match (&content.vectors, model_id) {
        (Some(vectors), Some(model_id)) => {
            content.model_id.as_deref() == Some(model_id)
                && vectors.len() == content.passages.len()
                && vectors.first().is_some_and(|first| !first.is_empty())
                && vectors.iter().all(|vector| {
                    vector.len() == vectors[0].len()
                        && vector.iter().all(|value| value.is_finite())
                        && vector.iter().map(|value| value * value).sum::<f32>() > f32::EPSILON
                })
        }
        (None, None) => true,
        (None, Some(_)) => content.passages.is_empty(),
        (Some(_), None) => false,
    }
}

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
    "current_path":"","current_page":0,"total_pages":0,"current_chunk":0,"total_chunks":0,
    "reused":0,"reused_chunks":0,"last_error":"","phase":"idle"})
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
        if self.stop.load(Ordering::Acquire) || self.ensure_idle().is_err() {
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
            } else if this.status()["errors"].as_u64().unwrap_or(0) > 0 {
                "partial"
            } else {
                "complete"
            };
            this.update(json!({"running":false,"phase":phase,"current_path":"","current_page":0,"total_pages":0,"current_chunk":0,"total_chunks":0}));
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
        // Serialize shutdown with start so a late scheduler/UI command cannot
        // launch a new scan after close has started.
        let _guard = self.mutation.lock().unwrap();
        self.stop.store(true, Ordering::Release);
        self.cancel();
    }
    pub fn start_scheduler(self: &Arc<Self>) {
        let this = self.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !this.stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_secs(2));
                if this.stop.load(Ordering::Acquire) {
                    break;
                }
                let interval = this
                    .store
                    .settings()
                    .ok()
                    .and_then(|s| s["scan_interval"].as_u64())
                    .unwrap_or(0);
                if interval > 0 && last.elapsed() >= Duration::from_secs(interval) {
                    if this.stop.load(Ordering::Acquire) {
                        break;
                    }
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
        let configured_model_id = model_id.clone();
        let context_size = settings["context_size"]
            .as_u64()
            .unwrap_or(4096)
            .clamp(512, 32768);
        let file_limit = settings["max_file_mb"].as_u64().unwrap_or(32) * 1024 * 1024;
        let mut db = self.store.connect()?;
        let (mut scanned, mut skipped, mut indexed) = (0u64, 0u64, 0u64);
        let mut reused_files = 0u64;
        let mut passage_vectors = PassageVectorCache::default();
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
                    let content_eligible =
                        metadata.len() <= file_limit && extract::supports_content(&path);
                    let extension = path
                        .extension()
                        .map(|extension| format!(".{}", extension.to_string_lossy().to_lowercase()))
                        .unwrap_or_default();
                    let key_prefix = content_key_prefix(
                        &reader,
                        &extension,
                        configured_model_id.as_deref(),
                        context_size,
                    );
                    let unchanged = old.as_ref().is_some_and(|old| {
                        old["modified"].as_i64() == Some(modified)
                            && old["size"].as_u64() == Some(metadata.len())
                            && old["status"] != "error"
                            && old["reader_id"].as_str() == Some(&reader)
                            && (model_id.is_none()
                                || old["model_id"].as_str() == model_id.as_deref()
                                || old["status"] == "metadata")
                    });
                    let mut content_key = None;
                    if unchanged {
                        let old = old.as_ref().expect("unchanged file has an old record");
                        if !content_eligible {
                            skipped += 1;
                            self.update(json!({"skipped":skipped}));
                            continue;
                        }
                        let file_id = old["id"].as_i64().expect("stored file has an id");
                        let old_key = Store::content_key_on(&db, file_id)?;
                        if let Some(old_key) =
                            old_key.as_ref().filter(|key| key.starts_with(&key_prefix))
                        {
                            let valid_content = if let Some(content) =
                                Store::reusable_content_on(&db, old_key)?
                            {
                                let valid =
                                    reusable_compatible(&content, configured_model_id.as_deref());
                                if valid
                                    && configured_model_id.is_some()
                                    && old["model_id"].as_str() == configured_model_id.as_deref()
                                {
                                    seed_passage_vectors(
                                        &mut passage_vectors,
                                        &content,
                                        configured_model_id.as_deref(),
                                        context_size,
                                    );
                                }
                                valid
                            } else {
                                false
                            };
                            if valid_content {
                                skipped += 1;
                                self.update(json!({"skipped":skipped}));
                                continue;
                            }
                        }
                        if old_key.is_none() {
                            let hash =
                                match content_hash(&path, &base, &metadata, &self.cancel_flag) {
                                    Ok(hash) => hash,
                                    Err(_) => None,
                                };
                            if self.cancelled() {
                                break;
                            }
                            if !file_matches_metadata(&path, &base, &metadata) {
                                self.error(format!(
                                    "해시를 확인하는 동안 변경되어 다음 색인으로 미룹니다: {}",
                                    path.display()
                                ));
                                continue;
                            }
                            let mut skip_legacy = true;
                            if let Some(hash) = hash {
                                let key = format!("{key_prefix}{hash}");
                                let can_backfill = old["status"] == "metadata"
                                    || old["model_id"].as_str() == configured_model_id.as_deref();
                                if can_backfill {
                                    Store::set_content_key_on(&db, file_id, &key, old)?;
                                    if Store::content_key_on(&db, file_id)?.as_deref()
                                        == Some(key.as_str())
                                    {
                                        content_key = Some(key.clone());
                                        let valid_content = if let Some(content) =
                                            Store::reusable_content_on(&db, &key)?
                                        {
                                            let valid = reusable_compatible(
                                                &content,
                                                configured_model_id.as_deref(),
                                            );
                                            if valid && configured_model_id.is_some() {
                                                seed_passage_vectors(
                                                    &mut passage_vectors,
                                                    &content,
                                                    configured_model_id.as_deref(),
                                                    context_size,
                                                );
                                            }
                                            valid
                                        } else {
                                            false
                                        };
                                        if !valid_content {
                                            skip_legacy = false;
                                        }
                                    } else {
                                        skip_legacy = false;
                                    }
                                }
                            }
                            if skip_legacy {
                                skipped += 1;
                                self.update(json!({"skipped":skipped}));
                                continue;
                            }
                        }
                    }

                    if content_eligible && content_key.is_none() {
                        match content_hash(&path, &base, &metadata, &self.cancel_flag) {
                            Ok(Some(hash)) => content_key = Some(format!("{key_prefix}{hash}")),
                            Ok(None) if self.cancelled() => break,
                            Ok(None) => {
                                if !file_matches_metadata(&path, &base, &metadata) {
                                    self.error(format!(
                                        "해시를 확인하는 동안 변경되어 다음 색인으로 미룹니다: {}",
                                        path.display()
                                    ));
                                    continue;
                                }
                            }
                            Err(_) => {
                                if !file_matches_metadata(&path, &base, &metadata) {
                                    self.error(format!(
                                        "해시를 확인하는 동안 변경되어 다음 색인으로 미룹니다: {}",
                                        path.display()
                                    ));
                                    continue;
                                }
                            }
                        }
                    }

                    let mut reused_content = None;
                    if let Some(key) = content_key.as_deref() {
                        if let Some(content) = pending_content(&pending, key) {
                            if reusable_compatible(&content, configured_model_id.as_deref()) {
                                reused_content = Some(content);
                            }
                        }
                        if reused_content.is_none() {
                            if let Some(content) = Store::reusable_content_on(&db, key)? {
                                if reusable_compatible(&content, configured_model_id.as_deref()) {
                                    reused_content = Some(content);
                                }
                            }
                        }
                    }

                    let (mut passages, mut vectors, mut error) = (Vec::new(), None, None);
                    if let Some(content) = reused_content {
                        passages = content.passages.clone();
                        vectors = content.vectors.clone();
                        let chunks = vectors.as_ref().map_or(0, Vec::len);
                        bump_reused_chunks(self, chunks);
                        seed_passage_vectors(
                            &mut passage_vectors,
                            &content,
                            configured_model_id.as_deref(),
                            context_size,
                        );
                        reused_files += 1;
                        self.update(json!({"reused":reused_files}));
                    } else if content_eligible {
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
                            let embedding_model = configured_model_id
                                .as_deref()
                                .expect("active model id has a configured identity");
                            self.update(json!({
                                "phase":"embedding",
                                "current_chunk":0,
                                "total_chunks":passages.len()
                            }));
                            let mut all = Vec::with_capacity(passages.len());
                            for group in passages.chunks(4) {
                                if self.cancelled() {
                                    break;
                                }
                                let keys: Vec<_> = group
                                    .iter()
                                    .map(|passage| {
                                        passage_vector_key(
                                            embedding_model,
                                            context_size,
                                            &passage.text,
                                        )
                                    })
                                    .collect();
                                let mut ordered = vec![None; group.len()];
                                let mut misses = Vec::new();
                                let mut miss_indices = Vec::new();
                                let mut reused_in_group = 0usize;
                                for (index, key) in keys.iter().enumerate() {
                                    if let Some(vector) = passage_vectors.get(key) {
                                        ordered[index] = Some(vector);
                                        reused_in_group += 1;
                                    } else {
                                        misses.push(group[index].text.clone());
                                        miss_indices.push(index);
                                    }
                                }
                                if !misses.is_empty() {
                                    match self.engine.embed(&misses, false) {
                                        Ok(values) if values.len() == misses.len() => {
                                            for (index, vector) in
                                                miss_indices.iter().copied().zip(values)
                                            {
                                                passage_vectors
                                                    .insert(keys[index].clone(), vector.clone());
                                                ordered[index] = Some(vector);
                                            }
                                        }
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
                                if ordered.iter().any(Option::is_none) {
                                    self.error("의미 색인 결과 벡터가 비어 있습니다.");
                                    model_id = None;
                                    all.clear();
                                    break;
                                }
                                all.extend(ordered.into_iter().flatten());
                                bump_reused_chunks(self, reused_in_group);
                                self.update(json!({"current_chunk":all.len()}));
                            }
                            if !all.is_empty() {
                                vectors = Some(all);
                            }
                        }
                    }

                    if content_eligible {
                        self.update(json!({"phase":"scanning","current_page":0,"total_pages":0,"current_chunk":0,"total_chunks":0}));
                        if !file_matches_metadata(&path, &base, &metadata) {
                            self.error(format!(
                                "읽는 중 변경되어 다음 색인으로 미룹니다: {}",
                                path.display()
                            ));
                            continue;
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
                        configured_model_id.clone()
                    } else {
                        None
                    };
                    let content_key = if error.is_none()
                        && content_eligible
                        && content_key.is_some()
                        && (passages.is_empty()
                            || (configured_model_id.is_none() && vectors.is_none())
                            || (configured_model_id.is_some()
                                && vectors.is_some()
                                && record_model == configured_model_id))
                    {
                        content_key
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
                        content_key,
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

#[cfg(test)]
mod content_identity_tests {
    use super::content_key_prefix;

    #[test]
    fn content_key_identity_includes_reader_extension_model_and_context() {
        let base = content_key_prefix("reader-v1", ".txt", Some("google-a"), 4096);
        assert_ne!(
            base,
            content_key_prefix("reader-v2", ".txt", Some("google-a"), 4096)
        );
        assert_ne!(
            base,
            content_key_prefix("reader-v1", ".md", Some("google-a"), 4096)
        );
        assert_ne!(
            base,
            content_key_prefix("reader-v1", ".txt", Some("google-b"), 4096)
        );
        assert_ne!(
            base,
            content_key_prefix("reader-v1", ".txt", Some("google-a"), 8192)
        );
    }
}
