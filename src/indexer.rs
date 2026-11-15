//! Bounded, atomic batches of completed files; one writer and cancellable scans.
use crate::{
    changes::FolderChanges,
    config,
    engine::Embedder,
    extract::{self, Passage},
    store::{EmbeddingUpdate, IndexedFile, InventoryRecord, ReusedContent, Store},
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
const PASSAGE_TEXT_CACHE_BYTES: usize = 128 * 1024 * 1024;
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(700);
const CHANGE_MAX_WAIT: Duration = Duration::from_secs(30);

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

#[derive(Default)]
struct PassageTextCache {
    values: HashMap<String, Vec<Passage>>,
    bytes: usize,
}

impl PassageTextCache {
    fn get(&self, key: &str) -> Option<Vec<Passage>> {
        self.values.get(key).cloned()
    }

    fn insert(&mut self, key: String, passages: &[Passage]) {
        if self.values.contains_key(&key) {
            return;
        }
        let size = key.len().saturating_mul(2).saturating_add(
            passages
                .iter()
                .map(|passage| passage.text.len().saturating_add(passage.location.len()))
                .sum::<usize>(),
        );
        if self.bytes.saturating_add(size) > PASSAGE_TEXT_CACHE_BYTES {
            return;
        }
        self.bytes += size;
        self.values.insert(key, passages.to_vec());
    }
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

fn should_reconcile(
    dirty_since: Option<Instant>,
    last_change: Option<Instant>,
    periodic_due: bool,
    now: Instant,
) -> bool {
    periodic_due
        || dirty_since.is_some_and(|dirty| now.duration_since(dirty) >= CHANGE_MAX_WAIT)
        || last_change.is_some_and(|change| now.duration_since(change) >= CHANGE_DEBOUNCE)
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
    "metadata_total":0,"metadata_done":0,"content_total":0,"content_done":0,
    "embedding_total_chunks":0,"embedding_done_chunks":0,"embedding_state":"idle",
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
            } else if this.status()["errors"].as_u64().unwrap_or(0) > 0
                || this.status()["embedding_state"] == "pending"
            {
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
            const POLL_INTERVAL: Duration = Duration::from_secs(2);
            const WATCH_RETRY: Duration = Duration::from_secs(30);
            let mut watcher = FolderChanges::default();
            let mut monitoring = false;
            let mut dirty_since = None;
            let mut last_change = None;
            let mut last_scan = Instant::now();
            let mut last_watch_retry = Instant::now();
            while !this.stop.load(Ordering::Acquire) {
                std::thread::sleep(POLL_INTERVAL);
                if this.stop.load(Ordering::Acquire) {
                    break;
                }
                let interval = this
                    .store
                    .settings()
                    .ok()
                    .and_then(|s| s["scan_interval"].as_u64())
                    .unwrap_or(0);
                if interval == 0 {
                    watcher.clear();
                    monitoring = false;
                    dirty_since = None;
                    last_change = None;
                    last_scan = Instant::now();
                    continue;
                }

                let roots: Vec<_> = this
                    .store
                    .roots()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|root| root["path"].as_str().map(Path::new).map(Path::to_path_buf))
                    .collect();
                if !monitoring {
                    watcher.sync_roots(&roots);
                    monitoring = true;
                    if roots.is_empty() {
                        last_scan = Instant::now();
                        continue;
                    }
                    // Reconcile once after watches are installed to catch up
                    // changes made while the application was not running.
                    if this.start() {
                        last_scan = Instant::now();
                    } else if this.status()["running"].as_bool().unwrap_or(false) {
                        let now = Instant::now();
                        dirty_since = Some(now);
                        last_change = Some(now);
                    }
                    continue;
                }

                let (roots_changed, available) = watcher.sync_roots(&roots);
                let (changed, poll_failed) = watcher.poll();
                if roots.is_empty() {
                    dirty_since = None;
                    last_change = None;
                    last_scan = Instant::now();
                    continue;
                }
                if roots_changed || changed {
                    let now = Instant::now();
                    dirty_since.get_or_insert(now);
                    last_change = Some(now);
                }
                if (!available || poll_failed) && last_watch_retry.elapsed() >= WATCH_RETRY {
                    watcher.retry_failed();
                    last_watch_retry = Instant::now();
                }

                let interval = Duration::from_secs(interval);
                let periodic_due = last_scan.elapsed() >= interval;
                if periodic_due {
                    dirty_since.get_or_insert_with(Instant::now);
                }

                if should_reconcile(dirty_since, last_change, periodic_due, Instant::now())
                    && !this.status()["running"].as_bool().unwrap_or(false)
                    && this.start()
                {
                    dirty_since = None;
                    last_change = None;
                    last_scan = Instant::now();
                }
            }
            watcher.clear();
        });
    }
    fn flush_inventory(
        &self,
        db: &mut rusqlite::Connection,
        pending: &mut Vec<InventoryRecord>,
        metadata_total: u64,
        metadata_done: &mut u64,
        indexed: &mut u64,
        skipped: &mut u64,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let count = pending.len() as u64;
        let records = std::mem::take(pending);
        let (changed, unchanged) = Store::inventory_files_on(db, &records)?;
        *metadata_done += count;
        *indexed += changed as u64;
        *skipped += unchanged as u64;
        self.update(json!({
            "metadata_total":metadata_total,
            "metadata_done":*metadata_done,
            "indexed":*indexed,
            "skipped":*skipped
        }));
        Ok(())
    }

    fn refresh_changed_file(
        &self,
        db: &mut rusqlite::Connection,
        file: &IndexedFile,
        base: &Path,
        file_limit: u64,
        filename_only_names: &HashSet<String>,
        metadata_total: &mut u64,
        metadata_done: &mut u64,
        indexed: &mut u64,
        skipped: &mut u64,
    ) -> Result<()> {
        let current = std::fs::symlink_metadata(&file.path);
        match current {
            Ok(metadata) if config::safe_metadata(&metadata) && metadata.is_file() => {
                let resolved = config::canonical(&file.path);
                if resolved
                    .is_ok_and(|path| config::within(&path, base) && !config::excluded(&path))
                {
                    let mut reader = reader_id(&file.path).to_string();
                    if metadata.len() as u64 > file_limit {
                        reader = format!("size-limit-v1:{file_limit}:{reader}");
                    }
                    let mut record = InventoryRecord {
                        root_id: file.root_id,
                        path: file.path.clone(),
                        size: metadata.len() as i64,
                        modified: config::modified_ns(&metadata),
                        reader_id: reader,
                    };
                    if config::filename_only(&file.path, base, filename_only_names) {
                        record.reader_id = format!("filename-only-v1:{}", record.reader_id);
                    }
                    let (changed, unchanged) = Store::inventory_files_on(db, &[record])?;
                    *metadata_total += 1;
                    *metadata_done += 1;
                    *indexed += changed as u64;
                    *skipped += unchanged as u64;
                    self.update(json!({
                        "metadata_total":*metadata_total,
                        "metadata_done":*metadata_done,
                        "indexed":*indexed,
                        "skipped":*skipped
                    }));
                } else {
                    Store::delete_file_generation_on(db, file.id, file.generation)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Store::delete_file_generation_on(db, file.id, file.generation)?;
            }
            _ => {}
        }
        self.error(format!(
            "본문 색인 중 파일 버전이 바뀌어 다시 확인합니다: {}",
            file.path.display()
        ));
        Ok(())
    }

    fn run(&self) -> Result<()> {
        let settings = self.store.settings()?;
        let configured_model_id = self.engine.model_id();
        let context_size = settings["context_size"]
            .as_u64()
            .unwrap_or(4096)
            .clamp(512, 32768);
        let file_limit = settings["max_file_mb"].as_u64().unwrap_or(32) * 1024 * 1024;
        let filename_only_names = config::filename_only_names(
            settings["filename_only_dirs"]
                .as_str()
                .unwrap_or("_site,temp,vendor"),
        );
        let mut db = self.store.connect()?;
        let roots = self.store.roots()?;
        let mut root_bases = HashMap::<i64, std::path::PathBuf>::new();
        let (mut scanned, mut skipped, mut indexed) = (0u64, 0u64, 0u64);
        let (mut metadata_total, mut metadata_done) = (0u64, 0u64);
        let (mut content_total, mut content_done) = (0u64, 0u64);
        let mut reused_files = 0u64;
        let mut passage_vectors = PassageVectorCache::default();
        let mut passage_text = PassageTextCache::default();
        let mut pending_content_keys = HashMap::<(i64, i64), String>::new();

        // Phase one only enumerates filesystem metadata and commits names.
        // No reader, OCR, tokenizer, or embedding is called before every root
        // completes this pass.
        self.update(json!({"phase":"metadata"}));
        for root in &roots {
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
            root_bases.insert(root_id, base.clone());
            let mut seen = HashSet::new();
            let mut failed = false;
            let mut directories = vec![base.clone()];
            let mut pending = Vec::<InventoryRecord>::new();
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
                    scanned += 1;
                    metadata_total += 1;
                    self.update(json!({
                        "scanned":scanned,
                        "metadata_total":metadata_total,
                        "current_path":path,
                        "current_page":0,
                        "total_pages":0,
                        "phase":"metadata"
                    }));
                    let modified = config::modified_ns(&metadata);
                    let reader = reader_id(&path);
                    let reader = if metadata.len() > file_limit {
                        format!("size-limit-v1:{}:{reader}", file_limit)
                    } else {
                        reader.to_owned()
                    };
                    let filename_only = config::filename_only(&path, &base, &filename_only_names);
                    if filename_only {
                        pending.push(InventoryRecord {
                            root_id,
                            path,
                            size: metadata.len() as i64,
                            modified,
                            reader_id: format!("filename-only-v1:{reader}"),
                        });
                    } else {
                        pending.push(InventoryRecord {
                            root_id,
                            path,
                            size: metadata.len() as i64,
                            modified,
                            reader_id: reader,
                        });
                    }
                    if pending.len() >= 128 || since.elapsed() >= Duration::from_millis(250) {
                        self.flush_inventory(
                            &mut db,
                            &mut pending,
                            metadata_total,
                            &mut metadata_done,
                            &mut indexed,
                            &mut skipped,
                        )?;
                        since = Instant::now();
                    }
                }
            }
            self.flush_inventory(
                &mut db,
                &mut pending,
                metadata_total,
                &mut metadata_done,
                &mut indexed,
                &mut skipped,
            )?;
            if !failed && !self.cancelled() {
                self.store.prune_on(&mut db, root_id, &seen)?;
            }
        }

        if self.cancelled() {
            return Ok(());
        }
        self.update(json!({
            "phase":"content",
            "metadata_total":metadata_total,
            "metadata_done":metadata_done,
            "indexed":indexed,
            "skipped":skipped
        }));

        let mut content_files = Vec::<(IndexedFile, std::path::PathBuf)>::new();
        for root in &roots {
            let root_id = root["id"].as_i64().unwrap();
            let Some(base) = root_bases.get(&root_id).cloned() else {
                continue;
            };
            for file in Store::files_by_root_on(&db, root_id)? {
                if file.reader_id.starts_with("filename-only-v1:")
                    || file.size < 0
                    || file.size as u64 > file_limit
                    || !extract::supports_content(&file.path)
                {
                    continue;
                }
                content_total += 1;
                if file.status == "content" {
                    content_done += 1;
                }
                content_files.push((file, base.clone()));
            }
        }
        self.update(json!({
            "content_total":content_total,
            "content_done":content_done,
            "phase":"content"
        }));

        for (file, base) in &content_files {
            if self.cancelled() {
                break;
            }
            let metadata = match std::fs::symlink_metadata(&file.path) {
                Ok(metadata)
                    if config::safe_metadata(&metadata)
                        && metadata.is_file()
                        && metadata.len() as i64 == file.size
                        && config::modified_ns(&metadata) == file.modified =>
                {
                    metadata
                }
                _ => {
                    self.refresh_changed_file(
                        &mut db,
                        file,
                        base,
                        file_limit,
                        &filename_only_names,
                        &mut metadata_total,
                        &mut metadata_done,
                        &mut indexed,
                        &mut skipped,
                    )?;
                    if file.status == "content" {
                        content_done = content_done.saturating_sub(1);
                        self.update(json!({"content_done":content_done}));
                    }
                    continue;
                }
            };
            let resolved = config::canonical(&file.path);
            if !resolved.is_ok_and(|path| config::within(&path, base) && !config::excluded(&path)) {
                self.refresh_changed_file(
                    &mut db,
                    file,
                    base,
                    file_limit,
                    &filename_only_names,
                    &mut metadata_total,
                    &mut metadata_done,
                    &mut indexed,
                    &mut skipped,
                )?;
                if file.status == "content" {
                    content_done = content_done.saturating_sub(1);
                    self.update(json!({"content_done":content_done}));
                }
                continue;
            }
            let extension = file
                .path
                .extension()
                .map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
                .unwrap_or_default();
            let prefix = content_key_prefix(
                &file.reader_id,
                &extension,
                configured_model_id.as_deref(),
                context_size,
            );

            if file.status == "content" {
                let old_key = Store::content_key_on(&db, file.id)?;
                let key = if old_key
                    .as_deref()
                    .is_some_and(|key| key.starts_with(&prefix))
                {
                    old_key
                } else {
                    match content_hash(&file.path, base, &metadata, &self.cancel_flag) {
                        Ok(Some(hash)) => Some(format!("{prefix}{hash}")),
                        Ok(None) if self.cancelled() => break,
                        _ => None,
                    }
                };
                if !file_matches_metadata(&file.path, base, &metadata) {
                    self.refresh_changed_file(
                        &mut db,
                        file,
                        base,
                        file_limit,
                        &filename_only_names,
                        &mut metadata_total,
                        &mut metadata_done,
                        &mut indexed,
                        &mut skipped,
                    )?;
                    content_done = content_done.saturating_sub(1);
                    self.update(json!({"content_done":content_done}));
                    continue;
                }
                if let Some(key) = key {
                    if let Some(content) = Store::file_content_on(&db, file.id)? {
                        if content.model_id.as_deref() == configured_model_id.as_deref()
                            && file.embedding_context == Some(context_size)
                            && content.vectors.is_some()
                        {
                            seed_passage_vectors(
                                &mut passage_vectors,
                                &content,
                                configured_model_id.as_deref(),
                                context_size,
                            );
                        }
                    }
                    if configured_model_id.is_some() {
                        pending_content_keys.insert((file.id, file.generation), key);
                    } else {
                        Store::set_content_key_for_generation_on(
                            &db,
                            file.id,
                            file.generation,
                            None,
                            &key,
                        )?;
                    }
                }
                continue;
            }

            let key = match content_hash(&file.path, base, &metadata, &self.cancel_flag) {
                Ok(Some(hash)) => Some(format!("{prefix}{hash}")),
                Ok(None) if self.cancelled() => break,
                _ => None,
            };
            if !file_matches_metadata(&file.path, base, &metadata) {
                self.refresh_changed_file(
                    &mut db,
                    file,
                    base,
                    file_limit,
                    &filename_only_names,
                    &mut metadata_total,
                    &mut metadata_done,
                    &mut indexed,
                    &mut skipped,
                )?;
                if file.status == "content" {
                    content_done = content_done.saturating_sub(1);
                    self.update(json!({"content_done":content_done}));
                }
                continue;
            }

            let mut passages = Vec::new();
            let mut vectors = None;
            let mut error = None;
            let mut reused_content = false;
            if let Some(key) = key.as_deref() {
                if let Some(content) = Store::reusable_content_on(&db, key)? {
                    if reusable_compatible(&content, configured_model_id.as_deref()) {
                        passages = content.passages.clone();
                        vectors = content.vectors.clone();
                        seed_passage_vectors(
                            &mut passage_vectors,
                            &content,
                            configured_model_id.as_deref(),
                            context_size,
                        );
                        reused_content = true;
                    }
                }
                if !reused_content {
                    if let Some(cached) = passage_text.get(key) {
                        passages = cached;
                        reused_content = true;
                    }
                }
            }
            if !reused_content {
                let result = (|| -> Result<_> {
                    let resolved = config::canonical(&file.path)?;
                    anyhow::ensure!(
                        config::within(&resolved, base) && !config::excluded(&resolved),
                        "색인 범위 밖으로 변경된 파일입니다."
                    );
                    let items = extract::extract_with_models(
                        &resolved,
                        200_000,
                        &self.cancel_flag,
                        &|page, total, phase| {
                            self.update(json!({
                                "current_path":file.path,
                                "current_page":page,
                                "total_pages":total,
                                "content_phase":phase,
                                "phase":"content"
                            }))
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
                        if let Some(partial) = e.downcast_ref::<extract::ExtractionError>() {
                            passages =
                                extract::chunk_passages(partial.passages.clone(), 1200, 160)?;
                        }
                        error = Some(e.to_string());
                        self.error(format!("{}: {e}", file.path.display()));
                    }
                }
            }
            if self.cancelled() {
                break;
            }
            if !file_matches_metadata(&file.path, base, &metadata) {
                self.refresh_changed_file(
                    &mut db,
                    file,
                    base,
                    file_limit,
                    &filename_only_names,
                    &mut metadata_total,
                    &mut metadata_done,
                    &mut indexed,
                    &mut skipped,
                )?;
                if file.status == "content" {
                    content_done = content_done.saturating_sub(1);
                    self.update(json!({"content_done":content_done}));
                }
                continue;
            }
            if error.is_none() && reused_content {
                reused_files += 1;
                self.update(json!({"reused":reused_files}));
            }
            if error.is_none() {
                if let Some(key) = key.as_deref() {
                    passage_text.insert(key.to_string(), &passages);
                }
            }
            let stored_key = if error.is_none()
                && key.is_some()
                && (configured_model_id.is_none() || vectors.is_some() || passages.is_empty())
            {
                key.as_deref()
            } else {
                None
            };
            let record_model = vectors.as_ref().and(configured_model_id.as_deref());
            let stored = Store::write_content_on(
                &mut db,
                file.id,
                file.generation,
                &passages,
                vectors.as_deref(),
                record_model,
                context_size,
                error.as_deref(),
                stored_key,
            )?;
            if !stored {
                self.error(format!(
                    "파일이 목록 갱신 뒤 변경되어 본문을 저장하지 않았습니다: {}",
                    file.path.display()
                ));
                continue;
            }
            content_done += 1;
            self.update(json!({
                "content_done":content_done,
                "content_total":content_total,
                "current_path":file.path,
                "current_page":0,
                "total_pages":0,
                "phase":"content"
            }));
            if let Some(vectors) = vectors.as_ref() {
                bump_reused_chunks(self, vectors.len());
                let content = ReusedContent {
                    passages: passages.clone(),
                    vectors: Some(vectors.clone()),
                    model_id: record_model.map(str::to_string),
                };
                seed_passage_vectors(
                    &mut passage_vectors,
                    &content,
                    configured_model_id.as_deref(),
                    context_size,
                );
            } else if error.is_none() && configured_model_id.is_some() && !passages.is_empty() {
                if let Some(key) = key {
                    pending_content_keys.insert((file.id, file.generation), key);
                }
            }
        }

        if self.cancelled() {
            return Ok(());
        }
        let Some(embedding_model) = configured_model_id.as_deref() else {
            self.update(json!({
                "phase":"complete",
                "embedding_state":"disabled",
                "embedding_total_chunks":0,
                "embedding_done_chunks":0,
                "current_chunk":0,
                "total_chunks":0
            }));
            return Ok(());
        };

        self.update(json!({"phase":"embedding","embedding_state":"running"}));
        Store::reset_invalid_vectors_on(&mut db, embedding_model)?;
        for file in &content_files {
            let (file, _) = file;
            if let Some(key) = pending_content_keys.get(&(file.id, file.generation)) {
                Store::complete_embedding_file_on(
                    &mut db,
                    file.id,
                    file.generation,
                    embedding_model,
                    context_size,
                    Some(key),
                )?;
            }
        }
        let (embedding_total, mut embedding_done) =
            Store::embedding_counts_on(&db, embedding_model, context_size)?;
        self.update(json!({
            "phase":"embedding",
            "embedding_state":"running",
            "embedding_total_chunks":embedding_total,
            "embedding_done_chunks":embedding_done,
            "current_chunk":embedding_done,
            "total_chunks":embedding_total
        }));

        let mut cursor = 0i64;
        let mut embedding_failed = false;
        loop {
            if self.cancelled() {
                break;
            }
            let chunks =
                Store::pending_embedding_chunks_on(&db, embedding_model, context_size, cursor, 4)?;
            if chunks.is_empty() {
                break;
            }
            cursor = chunks.last().expect("nonempty embedding batch").id;
            let keys: Vec<_> = chunks
                .iter()
                .map(|chunk| passage_vector_key(embedding_model, context_size, &chunk.text))
                .collect();
            let mut ordered = vec![None; chunks.len()];
            let mut misses = Vec::new();
            let mut miss_keys = Vec::new();
            let mut miss_indices = Vec::<Vec<usize>>::new();
            let mut miss_positions = HashMap::<String, usize>::new();
            let mut reused_in_group = 0usize;
            for (index, key) in keys.iter().enumerate() {
                if let Some(vector) = passage_vectors.get(key) {
                    ordered[index] = Some(vector);
                    reused_in_group += 1;
                } else if let Some(position) = miss_positions.get(key) {
                    miss_indices[*position].push(index);
                    reused_in_group += 1;
                } else {
                    let position = misses.len();
                    miss_positions.insert(key.clone(), position);
                    misses.push(chunks[index].text.clone());
                    miss_keys.push(key.clone());
                    miss_indices.push(vec![index]);
                }
            }
            if !misses.is_empty() {
                match self.engine.embed(&misses, false) {
                    Ok(values)
                        if values.len() == misses.len()
                            && values.iter().all(|vector| {
                                !vector.is_empty()
                                    && vector.iter().all(|value| value.is_finite())
                                    && vector.iter().map(|value| value * value).sum::<f32>()
                                        > f32::EPSILON
                            }) =>
                    {
                        for ((key, indices), vector) in
                            miss_keys.iter().zip(miss_indices).zip(values)
                        {
                            passage_vectors.insert(key.clone(), vector.clone());
                            for index in indices {
                                ordered[index] = Some(vector.clone());
                            }
                        }
                    }
                    result => {
                        self.error(format!(
                            "의미 색인 실패, 본문은 검색할 수 있습니다: {}",
                            result
                                .err()
                                .map(|error| error.to_string())
                                .unwrap_or_else(|| {
                                    "벡터 수 또는 벡터 값이 올바르지 않습니다.".into()
                                })
                        ));
                        embedding_failed = true;
                    }
                }
            }
            let updates: Vec<_> = chunks
                .iter()
                .zip(ordered)
                .filter_map(|(chunk, vector)| {
                    vector.map(|vector| EmbeddingUpdate {
                        id: chunk.id,
                        file_id: chunk.file_id,
                        generation: chunk.generation,
                        vector,
                    })
                })
                .collect();
            let (updated, affected) = Store::update_embedding_vectors_on(
                &mut db,
                &updates,
                embedding_model,
                context_size,
            )?;
            if updated > 0 {
                bump_reused_chunks(self, reused_in_group);
                embedding_done = embedding_done.saturating_add(updated as u64);
                for (file_id, generation) in affected {
                    let key = pending_content_keys.get(&(file_id, generation));
                    Store::complete_embedding_file_on(
                        &mut db,
                        file_id,
                        generation,
                        embedding_model,
                        context_size,
                        key.map(String::as_str),
                    )?;
                }
                self.update(json!({
                    "embedding_done_chunks":embedding_done,
                    "embedding_total_chunks":embedding_total,
                    "current_chunk":embedding_done,
                    "total_chunks":embedding_total
                }));
            }
            if embedding_failed {
                break;
            }
        }
        let (_, final_done) = Store::embedding_counts_on(&db, embedding_model, context_size)?;
        let embedding_state = if self.cancelled() {
            "pending"
        } else if final_done >= embedding_total {
            "complete"
        } else {
            "pending"
        };
        self.update(json!({
            "embedding_state":embedding_state,
            "embedding_done_chunks":final_done,
            "embedding_total_chunks":embedding_total,
            "current_chunk":final_done,
            "total_chunks":embedding_total
        }));
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
    use super::{content_key_prefix, should_reconcile};
    use std::time::{Duration, Instant};

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

    #[test]
    fn directory_changes_reconcile_after_quiet_period_or_max_wait() {
        let start = Instant::now();
        assert!(!should_reconcile(
            Some(start),
            Some(start + Duration::from_millis(650)),
            false,
            start + Duration::from_secs(1),
        ));
        assert!(should_reconcile(
            Some(start),
            Some(start + Duration::from_millis(650)),
            false,
            start + Duration::from_millis(1400),
        ));
        assert!(should_reconcile(
            Some(start),
            Some(start + Duration::from_secs(29)),
            false,
            start + Duration::from_secs(30),
        ));
        assert!(should_reconcile(Some(start), None, true, start));
    }
}
