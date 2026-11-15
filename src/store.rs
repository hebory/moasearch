//! SQLite storage shared by indexing and search.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::config::{canonical, excluded, within};
use crate::extract::Passage;

/// A complete file replacement. A record is published only after extraction and
/// embedding have completed, so readers never observe a half-written document.
#[derive(Clone, Debug)]
pub struct FileRecord {
    pub root_id: i64,
    pub path: PathBuf,
    pub size: i64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
    pub passages: Vec<Passage>,
    /// One vector per passage. `None` means keyword-only indexing.
    pub vectors: Option<Vec<Vec<f32>>>,
    pub model_id: Option<String>,
    pub error: Option<String>,
    pub reader_id: String,
    /// Opaque identity for reusing extracted passages and embeddings of equal content.
    pub content_key: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct InventoryRecord {
    pub root_id: i64,
    pub path: PathBuf,
    pub size: i64,
    pub modified: i64,
    pub reader_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct IndexedFile {
    pub id: i64,
    pub root_id: i64,
    pub path: PathBuf,
    pub size: i64,
    pub modified: i64,
    pub status: String,
    pub model_id: Option<String>,
    pub reader_id: String,
    pub generation: i64,
    pub embedding_context: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct EmbeddingChunk {
    pub id: i64,
    pub file_id: i64,
    pub generation: i64,
    pub text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct EmbeddingUpdate {
    pub id: i64,
    pub file_id: i64,
    pub generation: i64,
    pub vector: Vec<f32>,
}

pub(crate) struct ReusedContent {
    pub passages: Vec<Passage>,
    pub vectors: Option<Vec<Vec<f32>>>,
    pub model_id: Option<String>,
}

/// The persisted database. Connections are short-lived except while an index
/// worker explicitly reuses one via `connect` and the `_on` methods.
#[derive(Clone, Debug)]
pub struct Store {
    pub data_dir: PathBuf,
    pub path: PathBuf,
}

impl Store {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "색인 데이터 폴더를 만들 수 없습니다: {}",
                data_dir.display()
            )
        })?;
        let data_dir = canonical(&data_dir)?;
        let store = Self {
            path: data_dir.join("index.sqlite3"),
            data_dir,
        };
        let mut db = store.connect()?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS roots (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL);
             CREATE TABLE IF NOT EXISTS files (
                 id INTEGER PRIMARY KEY,
                 root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
                 path TEXT UNIQUE NOT NULL, name TEXT NOT NULL, extension TEXT NOT NULL,
                 size INTEGER NOT NULL, modified INTEGER NOT NULL, status TEXT NOT NULL,
                 error TEXT, model_id TEXT, reader_id TEXT NOT NULL DEFAULT 'legacy-v1');
             CREATE INDEX IF NOT EXISTS files_root ON files(root_id);
             CREATE TABLE IF NOT EXISTS chunks (
                 id INTEGER PRIMARY KEY,
                 file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 text TEXT NOT NULL, location TEXT NOT NULL, vector BLOB, model_id TEXT);
             CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
             CREATE TABLE IF NOT EXISTS file_content_keys (
                 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
                 content_key TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS file_content_keys_key ON file_content_keys(content_key);
             CREATE TABLE IF NOT EXISTS file_generations (
                 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
                 generation INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS file_embedding_context (
                 file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
                 generation INTEGER NOT NULL,
                 model_id TEXT NOT NULL,
                 context_size INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS chunk_embedding_context (
                 chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
                 context_size INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                 text, content='chunks', content_rowid='id', tokenize='trigram');
             CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
                 INSERT INTO chunks_fts(rowid,text) VALUES(new.id,new.text);
             END;
             CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
                 INSERT INTO chunks_fts(chunks_fts,rowid,text) VALUES('delete',old.id,old.text);
             END;",
        )?;
        let columns = {
            let mut stmt = db.prepare("PRAGMA table_info(files)")?;
            stmt.query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<HashSet<_>>>()?
        };
        if !columns.contains("reader_id") {
            db.execute_batch(
                "ALTER TABLE files ADD COLUMN reader_id TEXT NOT NULL DEFAULT 'legacy-v1'",
            )?;
        }
        db.execute(
            "INSERT OR IGNORE INTO file_generations(file_id,generation)
             SELECT id,0 FROM files",
            [],
        )?;
        Self::backfill_embedding_contexts(&mut db)?;
        Self::prepare_search_metadata(&mut db)?;
        Ok(store)
    }

    fn backfill_embedding_contexts(db: &mut Connection) -> Result<()> {
        let legacy = {
            let mut stmt = db.prepare(
                "SELECT f.id,COALESCE(g.generation,0),f.model_id,k.content_key
                 FROM files f JOIN file_content_keys k ON k.file_id=f.id
                 LEFT JOIN file_generations g ON g.file_id=f.id
                 LEFT JOIN file_embedding_context e ON e.file_id=f.id
                 WHERE f.status='content' AND f.model_id IS NOT NULL AND e.file_id IS NULL",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if legacy.is_empty() {
            return Ok(());
        }
        let mut migrated = Vec::new();
        for (file_id, generation, model_id, key) in legacy {
            let marker = format!("|model={model_id}|context=");
            let Some(context) = key
                .split_once(&marker)
                .and_then(|(_, suffix)| suffix.split('|').next())
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            if !(512..=32768).contains(&context) {
                continue;
            }
            let vectors = {
                let mut stmt =
                    db.prepare("SELECT vector,model_id FROM chunks WHERE file_id=?1 ORDER BY id")?;
                stmt.query_map([file_id], |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if vectors.is_empty() {
                continue;
            }
            let mut dimension = None;
            let valid = vectors.iter().all(|(raw, chunk_model)| {
                if chunk_model.as_deref() != Some(model_id.as_str()) {
                    return false;
                }
                let Some(vector) = raw.as_deref().and_then(decode_vector) else {
                    return false;
                };
                let norm_squared: f32 = vector.iter().map(|value| value * value).sum();
                if vector.is_empty()
                    || vector.iter().any(|value| !value.is_finite())
                    || !norm_squared.is_finite()
                    || norm_squared <= f32::EPSILON
                    || dimension.is_some_and(|expected| expected != vector.len())
                {
                    return false;
                }
                dimension = Some(vector.len());
                true
            });
            if valid {
                migrated.push((file_id, generation, model_id, context as i64));
            }
        }
        if migrated.is_empty() {
            return Ok(());
        }
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (file_id, generation, model_id, context) in migrated {
            let complete: bool = tx.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM chunks WHERE file_id=?1
                     AND (vector IS NULL OR model_id IS NOT ?2))",
                params![file_id, model_id],
                |row| row.get(0),
            )?;
            if !complete {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO file_embedding_context(file_id,generation,model_id,context_size)
                 VALUES (?1,?2,?3,?4)",
                params![file_id, generation, model_id, context],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO chunk_embedding_context(chunk_id,context_size)
                 SELECT id,?2 FROM chunks WHERE file_id=?1 AND vector IS NOT NULL
                   AND model_id=?3",
                params![file_id, context, model_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn connect(&self) -> Result<Connection> {
        let db = Connection::open(&self.path)
            .with_context(|| format!("색인 DB를 열 수 없습니다: {}", self.path.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        Ok(db)
    }

    fn prepare_search_metadata(db: &mut Connection) -> Result<()> {
        let has_paths_fts = db
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE name='files_fts'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS chunks_model ON chunks(model_id,file_id) WHERE vector IS NOT NULL;
             CREATE INDEX IF NOT EXISTS chunks_file_text ON chunks(file_id,text,location);
             CREATE INDEX IF NOT EXISTS files_modified ON files(modified DESC);
             CREATE INDEX IF NOT EXISTS files_extension ON files(extension,modified DESC);
             CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
                 path, content='files', content_rowid='id', tokenize='trigram');
             CREATE TABLE IF NOT EXISTS index_meta (
                 id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL,
                 files INTEGER NOT NULL, chunks INTEGER NOT NULL, vectors INTEGER NOT NULL);
             INSERT INTO index_meta SELECT 1,0,
                 (SELECT count(*) FROM files), (SELECT count(*) FROM chunks),
                 (SELECT count(*) FROM chunks WHERE vector IS NOT NULL)
                 WHERE NOT EXISTS (SELECT 1 FROM index_meta WHERE id=1);
             CREATE TRIGGER IF NOT EXISTS files_search_ai AFTER INSERT ON files BEGIN
                 INSERT INTO files_fts(rowid,path) VALUES(new.id,new.path);
                 UPDATE index_meta SET revision=revision+1,files=files+1 WHERE id=1;
             END;
             CREATE TRIGGER IF NOT EXISTS files_search_ad AFTER DELETE ON files BEGIN
                 INSERT INTO files_fts(files_fts,rowid,path) VALUES('delete',old.id,old.path);
                 UPDATE index_meta SET revision=revision+1,files=files-1 WHERE id=1;
             END;
             CREATE TRIGGER IF NOT EXISTS files_search_au AFTER UPDATE ON files BEGIN
                 UPDATE index_meta SET revision=revision+1 WHERE id=1;
             END;
             CREATE TRIGGER IF NOT EXISTS files_path_au AFTER UPDATE OF path ON files
             WHEN old.path != new.path BEGIN
                 INSERT INTO files_fts(files_fts,rowid,path) VALUES('delete',old.id,old.path);
                 INSERT INTO files_fts(rowid,path) VALUES(new.id,new.path);
             END;
             CREATE TRIGGER IF NOT EXISTS chunks_meta_ai AFTER INSERT ON chunks BEGIN
                 UPDATE index_meta SET revision=revision+1,chunks=chunks+1,
                     vectors=vectors+(new.vector IS NOT NULL) WHERE id=1;
             END;
             CREATE TRIGGER IF NOT EXISTS chunks_meta_ad AFTER DELETE ON chunks BEGIN
                 UPDATE index_meta SET revision=revision+1,chunks=chunks-1,
                     vectors=vectors-(old.vector IS NOT NULL) WHERE id=1;
             END;
             CREATE TRIGGER IF NOT EXISTS chunks_meta_au AFTER UPDATE ON chunks BEGIN
                 UPDATE index_meta SET revision=revision+1,
                     vectors=vectors+(new.vector IS NOT NULL)-(old.vector IS NOT NULL) WHERE id=1;
             END;",
        )?;
        if !has_paths_fts {
            tx.execute("INSERT INTO files_fts(files_fts) VALUES('rebuild')", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn settings(&self) -> Result<Value> {
        let db = self.connect()?;
        let mut settings = crate::config::defaults();
        let mut stmt = db.prepare("SELECT key,value FROM settings")?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (key, raw) = row?;
            if let Ok(value) = serde_json::from_str(&raw) {
                settings.as_object_mut().unwrap().insert(key, value);
            }
        }
        Ok(settings)
    }

    pub fn update_settings(&self, settings: &Value) -> Result<Value> {
        let input = settings
            .as_object()
            .ok_or_else(|| anyhow!("설정은 JSON 객체여야 합니다."))?;
        let mut normalized = input.clone();
        let known: HashSet<String> = crate::config::defaults()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        if let Some(key) = input.keys().find(|key| !known.contains(key.as_str())) {
            bail!("알 수 없는 설정입니다: {key}");
        }
        if let Some(value) = input.get("chat_enabled") {
            value
                .as_bool()
                .ok_or_else(|| anyhow!("chat_enabled: true 또는 false가 필요합니다."))?;
        }
        if let Some(value) = input.get("gpu_mode") {
            let mode = value
                .as_str()
                .ok_or_else(|| anyhow!("gpu_mode: auto 또는 cpu 문자열이 필요합니다."))?;
            if !matches!(mode, "auto" | "cpu") {
                bail!("gpu_mode: auto 또는 cpu를 선택하세요.");
            }
        }
        if let Some(value) = input.get("filename_only_dirs") {
            let raw = value.as_str().ok_or_else(|| {
                anyhow!("filename_only_dirs: 폴더 이름 목록 문자열이 필요합니다.")
            })?;
            let mut seen = HashSet::new();
            let mut names = Vec::new();
            for item in raw
                .split(|ch| matches!(ch, ',' | ';' | '\n'))
                .map(str::trim)
                .filter(|item| !item.is_empty())
            {
                if item == "."
                    || item == ".."
                    || item.ends_with('.')
                    || item.chars().any(|ch| {
                        ch.is_control()
                            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                    })
                {
                    bail!("filename_only_dirs에는 경로가 아닌 폴더 이름만 입력하세요: {item}");
                }
                let folded = item.to_lowercase();
                if seen.insert(folded.clone()) {
                    names.push(folded);
                }
            }
            normalized.insert(
                "filename_only_dirs".to_string(),
                Value::String(names.join(",")),
            );
        }
        for (key, kind) in [
            ("embedding_model", crate::models::ModelKind::Embedding),
            ("chat_model", crate::models::ModelKind::Chat),
        ] {
            if let Some(value) = input.get(key) {
                let raw = value
                    .as_str()
                    .ok_or_else(|| anyhow!("{key}: 문자열 경로가 필요합니다."))?;
                if !raw.is_empty() {
                    let path = Path::new(raw);
                    let metadata = std::fs::symlink_metadata(path).with_context(|| {
                        format!("Google 모델 파일을 찾을 수 없습니다: {}", path.display())
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        bail!("일반 Google 모델 파일만 사용할 수 있습니다.");
                    }
                    let canonical_path = canonical(path)?;
                    if canonical_path.extension().and_then(|s| s.to_str()) != Some("gguf")
                        || canonical_path.file_name()
                            != Some(std::ffi::OsStr::new(kind.spec().filename))
                    {
                        bail!("{key}: 검증된 Google 모델 파일을 지정하세요.");
                    }
                    normalized.insert(
                        key.to_string(),
                        Value::String(canonical_path.to_string_lossy().to_string()),
                    );
                }
            }
        }
        for (key, bounds) in [
            ("n_gpu_layers", (-1, 200)),
            ("context_size", (512, 32768)),
            ("scan_interval", (0, 86400)),
            ("max_file_mb", (1, 512)),
        ] {
            if let Some(value) = input.get(key) {
                let n = value
                    .as_i64()
                    .ok_or_else(|| anyhow!("{key}: 정수가 필요합니다."))?;
                if !(bounds.0..=bounds.1).contains(&n) {
                    bail!("{key}: {}~{} 범위의 정수가 필요합니다.", bounds.0, bounds.1);
                }
            }
        }
        let mut db = self.connect()?;
        let tx = db.transaction()?;
        for (key, value) in &normalized {
            let encoded = serde_json::to_string(value)?;
            tx.execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES (?1,?2)",
                params![key, encoded],
            )?;
        }
        tx.commit()?;
        self.settings()
    }

    pub fn roots(&self) -> Result<Vec<Value>> {
        let db = self.connect()?;
        let mut stmt = db.prepare("SELECT id,path FROM roots ORDER BY id")?;
        Ok(stmt
            .query_map([], |row| {
                Ok(json!({"id": row.get::<_, i64>(0)?, "path": row.get::<_, String>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether the index contains at least one file under the selected folder.
    pub fn has_files_in(&self, folder: &Path) -> Result<bool> {
        let pattern = crate::search::scope_like(folder);
        let db = self.connect()?;
        let mut stmt =
            db.prepare("SELECT path FROM files WHERE path LIKE ? ESCAPE '\\' COLLATE NOCASE")?;
        let mut rows = stmt.query([pattern])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            if crate::search::folder_contains_path(folder, Path::new(&path)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn add_root(&self, raw_path: impl AsRef<Path>) -> Result<Value> {
        let path = canonical(raw_path.as_ref())?;
        if !path.is_dir() {
            bail!("접근 가능한 폴더 경로를 입력하세요.");
        }
        if excluded(&path) || within(&path, &self.data_dir) {
            bail!("기본 제외 경로 또는 앱 데이터 폴더는 등록할 수 없습니다.");
        }
        let db = self.connect()?;
        let mut stmt = db.prepare("SELECT path FROM roots")?;
        for row in stmt.query_map([], |row| row.get::<_, String>(0))? {
            let other = PathBuf::from(row?);
            if within(&path, &other) || within(&other, &path) {
                bail!("이미 등록된 폴더와 범위가 겹칩니다.");
            }
        }
        drop(stmt);
        db.execute(
            "INSERT INTO roots(path) VALUES (?1)",
            [path.to_string_lossy().as_ref()],
        )?;
        Ok(json!({"id": db.last_insert_rowid(), "path": path.to_string_lossy()}))
    }

    pub fn remove_root(&self, root_id: i64) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM roots WHERE id=?1", [root_id])?;
        Ok(())
    }

    pub fn index_state(&self) -> Result<Value> {
        let db = self.connect()?;
        Self::index_state_on(&db)
    }

    pub fn index_state_on(db: &Connection) -> Result<Value> {
        let (revision, files, chunks, vectors) = db.query_row(
            "SELECT revision,files,chunks,vectors FROM index_meta WHERE id=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
        Ok(json!({"revision":revision,"files":files,"chunks":chunks,"vectors":vectors}))
    }

    pub fn counts(&self) -> Result<Value> {
        let state = self.index_state()?;
        Ok(json!({"files":state["files"],"chunks":state["chunks"],"vectors":state["vectors"]}))
    }

    pub(crate) fn content_key_on(db: &Connection, file_id: i64) -> Result<Option<String>> {
        Ok(db
            .query_row(
                "SELECT content_key FROM file_content_keys WHERE file_id=?1",
                [file_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn set_content_key_on(
        db: &Connection,
        file_id: i64,
        key: &str,
        expected: &Value,
    ) -> Result<()> {
        let Some(model_value) = expected.get("model_id") else {
            return Ok(());
        };
        if !model_value.is_null() && !model_value.is_string() {
            return Ok(());
        }
        let model_id = model_value.as_str();
        let (Some(size), Some(modified), Some(reader_id)) = (
            expected.get("size").and_then(Value::as_i64),
            expected.get("modified").and_then(Value::as_i64),
            expected.get("reader_id").and_then(Value::as_str),
        ) else {
            return Ok(());
        };
        db.execute(
            "INSERT INTO file_content_keys(file_id,content_key)
             SELECT id,?2 FROM files WHERE id=?1 AND size=?3 AND modified=?4
                 AND reader_id=?5 AND model_id IS ?6
                 AND status IN ('content','metadata') AND error IS NULL
             ON CONFLICT(file_id) DO UPDATE SET content_key=excluded.content_key",
            params![file_id, key, size, modified, reader_id, model_id],
        )?;
        Ok(())
    }

    pub(crate) fn reusable_content_on(db: &Connection, key: &str) -> Result<Option<ReusedContent>> {
        let mut stmt = db.prepare(
            "WITH candidate AS (
                 SELECT f.id,f.model_id FROM file_content_keys k
                 JOIN files f ON f.id=k.file_id
                 WHERE k.content_key=?1 AND f.status IN ('content','metadata') AND f.error IS NULL
                 ORDER BY f.id LIMIT 1
             )
             SELECT candidate.id,candidate.model_id,c.id,c.text,c.location,c.vector,c.model_id
             FROM candidate LEFT JOIN chunks c ON c.file_id=candidate.id ORDER BY c.id",
        )?;
        let rows = stmt
            .query_map([key], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some((_, model_id, _, _, _, _, _)) = rows.first() else {
            return Ok(None);
        };
        let model_id = (*model_id).clone();
        let mut passages = Vec::with_capacity(rows.len());
        let mut vectors = Vec::with_capacity(rows.len());
        let mut vector_count = 0usize;
        for (_, _, chunk_id, text, location, raw, chunk_model_id) in rows {
            let Some(_chunk_id) = chunk_id else {
                continue;
            };
            let (Some(text), Some(location)) = (text, location) else {
                return Ok(None);
            };
            let passage = Passage { text, location };
            passages.push(passage);
            if let Some(raw) = raw {
                if model_id.is_none() || chunk_model_id.as_deref() != model_id.as_deref() {
                    return Ok(None);
                }
                let Some(vector) = decode_vector(&raw) else {
                    return Ok(None);
                };
                vector_count += 1;
                vectors.push(vector);
            }
        }
        let vectors = if vector_count == 0 {
            None
        } else {
            if vector_count != passages.len() || vectors.len() != passages.len() {
                return Ok(None);
            }
            let dimension = vectors.first().map(Vec::len).unwrap_or(0);
            if dimension == 0
                || vectors.iter().any(|v| {
                    let norm_squared = v.iter().map(|value| value * value).sum::<f32>();
                    v.len() != dimension
                        || v.iter().any(|value| !value.is_finite())
                        || !norm_squared.is_finite()
                        || norm_squared <= f32::EPSILON
                })
            {
                return Ok(None);
            }
            Some(vectors)
        };
        if !passages.is_empty() && model_id.is_some() && vectors.is_none() {
            return Ok(None);
        }
        Ok(Some(ReusedContent {
            passages,
            vectors,
            model_id,
        }))
    }

    pub(crate) fn file_content_on(db: &Connection, file_id: i64) -> Result<Option<ReusedContent>> {
        let mut stmt = db.prepare(
            "SELECT f.model_id,c.id,c.text,c.location,c.vector,c.model_id
             FROM files f LEFT JOIN chunks c ON c.file_id=f.id
             WHERE f.id=?1 AND f.status='content' AND f.error IS NULL ORDER BY c.id",
        )?;
        let rows = stmt
            .query_map([file_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some((_, _, _, _, _, _)) = rows.first() else {
            return Ok(None);
        };
        let model_id = rows[0].0.clone();
        let mut passages = Vec::new();
        let mut vectors = Vec::new();
        let mut vector_count = 0usize;
        let mut vectors_valid = true;
        for (_, chunk_id, text, location, raw, chunk_model_id) in rows {
            let Some(_chunk_id) = chunk_id else {
                continue;
            };
            let (Some(text), Some(location)) = (text, location) else {
                return Ok(None);
            };
            passages.push(Passage { text, location });
            if let Some(raw) = raw {
                let valid_model = model_id.is_some() && chunk_model_id == model_id;
                if !valid_model {
                    vectors_valid = false;
                    continue;
                }
                if let Some(vector) = decode_vector(&raw) {
                    if !vector.is_empty()
                        && vector.iter().all(|value| value.is_finite())
                        && vector.iter().map(|value| value * value).sum::<f32>() > f32::EPSILON
                    {
                        vector_count += 1;
                        vectors.push(vector);
                    } else {
                        vectors_valid = false;
                    }
                } else {
                    vectors_valid = false;
                }
            } else {
                vectors_valid = false;
            }
        }
        let vectors = if passages.is_empty() || !vectors_valid || vector_count != passages.len() {
            None
        } else {
            let dimension = vectors.first().map(Vec::len).unwrap_or(0);
            if dimension == 0 || vectors.iter().any(|vector| vector.len() != dimension) {
                None
            } else {
                Some(vectors)
            }
        };
        Ok(Some(ReusedContent {
            passages,
            vectors,
            model_id,
        }))
    }

    pub fn file_by_path(&self, path: &Path) -> Result<Option<Value>> {
        Self::file_by_path_on(&self.connect()?, path)
    }

    pub fn file_by_path_on(db: &Connection, path: &Path) -> Result<Option<Value>> {
        let path = path.to_string_lossy();
        db.query_row(
            "SELECT * FROM files WHERE path=?1",
            [path.as_ref()],
            row_json,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Publish the path and current filesystem version before any content work.
    /// Unchanged rows keep their body and vectors; changed rows invalidate both
    /// atomically so stale text cannot appear under a new filename/version.
    pub(crate) fn inventory_files_on(
        db: &mut Connection,
        records: &[InventoryRecord],
    ) -> Result<(usize, usize)> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut changed_count, mut unchanged_count) = (0usize, 0usize);
        for record in records {
            let path = record.path.to_string_lossy().to_string();
            let name = record
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let extension = record
                .path
                .extension()
                .map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
                .unwrap_or_default();
            let old = tx
                .query_row(
                    "SELECT f.id,f.size,f.modified,f.status,f.reader_id,
                            COALESCE(g.generation,0)
                     FROM files f LEFT JOIN file_generations g ON g.file_id=f.id
                     WHERE f.path=?1",
                    [&path],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .optional()?;
            let unchanged = old
                .as_ref()
                .is_some_and(|(_, size, modified, status, reader, _)| {
                    *size == record.size
                        && *modified == record.modified
                        && reader == &record.reader_id
                        && status != "error"
                });
            if let Some((file_id, _, _, _, _, generation)) = old {
                if unchanged {
                    unchanged_count += 1;
                    tx.execute(
                        "UPDATE files SET root_id=?2 WHERE id=?1 AND root_id<>?2",
                        params![file_id, record.root_id],
                    )?;
                    tx.execute(
                        "INSERT OR IGNORE INTO file_generations(file_id,generation) VALUES (?1,?2)",
                        params![file_id, generation],
                    )?;
                    continue;
                }
                changed_count += 1;
                let generation = generation.saturating_add(1).max(1);
                tx.execute(
                    "UPDATE files SET root_id=?2,name=?3,extension=?4,size=?5,modified=?6,
                         status='metadata',error=NULL,model_id=NULL,reader_id=?7 WHERE id=?1",
                    params![
                        file_id,
                        record.root_id,
                        name,
                        extension,
                        record.size,
                        record.modified,
                        record.reader_id,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO file_generations(file_id,generation) VALUES (?1,?2)
                     ON CONFLICT(file_id) DO UPDATE SET generation=excluded.generation",
                    params![file_id, generation],
                )?;
                tx.execute("DELETE FROM chunks WHERE file_id=?1", [file_id])?;
                tx.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
                tx.execute(
                    "DELETE FROM file_embedding_context WHERE file_id=?1",
                    [file_id],
                )?;
            } else {
                changed_count += 1;
                tx.execute(
                    "INSERT INTO files(root_id,path,name,extension,size,modified,status,error,model_id,reader_id)
                     VALUES (?1,?2,?3,?4,?5,?6,'metadata',NULL,NULL,?7)",
                    params![
                        record.root_id,
                        path,
                        name,
                        extension,
                        record.size,
                        record.modified,
                        record.reader_id,
                    ],
                )?;
                let file_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO file_generations(file_id,generation) VALUES (?1,1)",
                    [file_id],
                )?;
            }
        }
        tx.commit()?;
        Ok((changed_count, unchanged_count))
    }

    pub(crate) fn files_by_root_on(db: &Connection, root_id: i64) -> Result<Vec<IndexedFile>> {
        let mut stmt = db.prepare(
            "SELECT f.id,f.root_id,f.path,f.size,f.modified,f.status,f.model_id,f.reader_id,
                    COALESCE(g.generation,0),e.context_size
             FROM files f LEFT JOIN file_generations g ON g.file_id=f.id
             LEFT JOIN file_embedding_context e ON e.file_id=f.id
             WHERE f.root_id=?1 ORDER BY f.path",
        )?;
        let files = stmt
            .query_map([root_id], |row| {
                Ok(IndexedFile {
                    id: row.get(0)?,
                    root_id: row.get(1)?,
                    path: PathBuf::from(row.get::<_, String>(2)?),
                    size: row.get(3)?,
                    modified: row.get(4)?,
                    status: row.get(5)?,
                    model_id: row.get(6)?,
                    reader_id: row.get(7)?,
                    generation: row.get(8)?,
                    embedding_context: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(files)
    }

    pub(crate) fn delete_file_generation_on(
        db: &mut Connection,
        file_id: i64,
        generation: i64,
    ) -> Result<bool> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM files WHERE id=?1 AND EXISTS(
                 SELECT 1 FROM file_generations g WHERE g.file_id=files.id AND g.generation=?2
             )",
            params![file_id, generation],
        )?;
        tx.commit()?;
        Ok(deleted > 0)
    }

    /// Store extracted text (and optional, already validated cached vectors).
    /// This is a short version-checked transaction so FTS becomes searchable
    /// before the later embedding queue starts.
    pub(crate) fn write_content_on(
        db: &mut Connection,
        file_id: i64,
        generation: i64,
        passages: &[Passage],
        vectors: Option<&[Vec<f32>]>,
        model_id: Option<&str>,
        context_size: u64,
        error: Option<&str>,
        content_key: Option<&str>,
    ) -> Result<bool> {
        if let Some(vectors) = vectors {
            if vectors.len() != passages.len() {
                bail!("임베딩 벡터 수가 문서 구간 수와 일치하지 않습니다.");
            }
        }
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status = if error.is_some() { "error" } else { "content" };
        let updated = tx.execute(
            "UPDATE files SET status=?3,error=?4,model_id=?5
             WHERE id=?1 AND status='metadata'
               AND EXISTS(SELECT 1 FROM file_generations g
                          WHERE g.file_id=files.id AND g.generation=?2)",
            params![file_id, generation, status, error, model_id],
        )?;
        if updated == 0 {
            tx.rollback()?;
            return Ok(false);
        }
        tx.execute("DELETE FROM chunks WHERE file_id=?1", [file_id])?;
        for (index, passage) in passages.iter().enumerate() {
            let vector = vectors.map(|values| encode_vector(&values[index]));
            let chunk_model = vector.as_ref().map(|_| model_id);
            let has_vector = vector.is_some();
            tx.execute(
                "INSERT INTO chunks(file_id,text,location,vector,model_id) VALUES (?1,?2,?3,?4,?5)",
                params![file_id, passage.text, passage.location, vector, chunk_model],
            )?;
            if has_vector {
                tx.execute(
                    "INSERT INTO chunk_embedding_context(chunk_id,context_size) VALUES (?1,?2)",
                    params![tx.last_insert_rowid(), context_size as i64],
                )?;
            }
        }
        if let (Some(model_id), Some(_)) = (model_id, vectors) {
            tx.execute(
                "INSERT INTO file_embedding_context(file_id,generation,model_id,context_size)
                 VALUES (?1,?2,?3,?4)
                 ON CONFLICT(file_id) DO UPDATE SET generation=excluded.generation,
                     model_id=excluded.model_id,context_size=excluded.context_size",
                params![file_id, generation, model_id, context_size as i64],
            )?;
        } else {
            tx.execute(
                "DELETE FROM file_embedding_context WHERE file_id=?1",
                [file_id],
            )?;
        }
        if error.is_none() {
            if let Some(key) = content_key {
                tx.execute(
                    "INSERT INTO file_content_keys(file_id,content_key) VALUES (?1,?2)
                     ON CONFLICT(file_id) DO UPDATE SET content_key=excluded.content_key",
                    params![file_id, key],
                )?;
            } else {
                tx.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
            }
        } else {
            tx.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn pending_embedding_chunks_on(
        db: &Connection,
        model_id: &str,
        context_size: u64,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<EmbeddingChunk>> {
        let mut stmt = db.prepare(
            "SELECT c.id,c.file_id,COALESCE(g.generation,0),c.text
             FROM chunks c JOIN files f ON f.id=c.file_id
             LEFT JOIN file_generations g ON g.file_id=f.id
             LEFT JOIN chunk_embedding_context ce ON ce.chunk_id=c.id
             WHERE f.status='content'
               AND c.id>?2 AND (c.vector IS NULL OR c.model_id IS NOT ?1
                   OR ce.context_size IS NOT ?3)
             ORDER BY c.id LIMIT ?4",
        )?;
        let chunks = stmt
            .query_map(
                params![model_id, after_id, context_size as i64, limit as i64],
                |row| {
                    Ok(EmbeddingChunk {
                        id: row.get(0)?,
                        file_id: row.get(1)?,
                        generation: row.get(2)?,
                        text: row.get(3)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(chunks)
    }

    pub(crate) fn embedding_counts_on(
        db: &Connection,
        model_id: &str,
        context_size: u64,
    ) -> Result<(u64, u64)> {
        let (total, done) = db.query_row(
            "SELECT count(*),sum(c.vector IS NOT NULL AND c.model_id=?1
                  AND ce.context_size=?2)
             FROM chunks c JOIN files f ON f.id=c.file_id
             LEFT JOIN chunk_embedding_context ce ON ce.chunk_id=c.id
             WHERE f.status='content'",
            params![model_id, context_size as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                ))
            },
        )?;
        Ok((total.max(0) as u64, done.max(0) as u64))
    }

    /// Clear malformed vectors before queueing work. This also recovers rows
    /// whose blob exists but cannot safely participate in semantic search.
    pub(crate) fn reset_invalid_vectors_on(db: &mut Connection, model_id: &str) -> Result<u64> {
        let invalid: Vec<i64> = {
            let mut stmt = db
                .prepare("SELECT id,vector FROM chunks WHERE vector IS NOT NULL AND model_id=?1")?;
            stmt.query_map([model_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .filter_map(|item| match item {
                Ok((id, raw)) => {
                    let valid = decode_vector(&raw).is_some_and(|vector| {
                        !vector.is_empty()
                            && vector.iter().all(|value| value.is_finite())
                            && vector
                                .iter()
                                .map(|value| value * value)
                                .sum::<f32>()
                                .is_finite()
                            && vector.iter().map(|value| value * value).sum::<f32>() > f32::EPSILON
                    });
                    (!valid).then_some(Ok(id))
                }
                Err(error) => Some(Err(error)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if invalid.is_empty() {
            return Ok(0);
        }
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in &invalid {
            tx.execute(
                "UPDATE chunks SET vector=NULL,model_id=NULL WHERE id=?1 AND model_id=?2",
                params![id, model_id],
            )?;
            tx.execute(
                "DELETE FROM chunk_embedding_context WHERE chunk_id=?1",
                [id],
            )?;
            tx.execute(
                "UPDATE files SET model_id=NULL WHERE id=(SELECT file_id FROM chunks WHERE id=?1)",
                [id],
            )?;
            tx.execute(
                "DELETE FROM file_content_keys WHERE file_id=(SELECT file_id FROM chunks WHERE id=?1)",
                [id],
            )?;
            tx.execute(
                "DELETE FROM file_embedding_context WHERE file_id=(SELECT file_id FROM chunks WHERE id=?1)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(invalid.len() as u64)
    }

    pub(crate) fn update_embedding_vectors_on(
        db: &mut Connection,
        updates: &[EmbeddingUpdate],
        model_id: &str,
        context_size: u64,
    ) -> Result<(usize, HashSet<(i64, i64)>)> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut affected = HashSet::new();
        let mut updated_count = 0usize;
        for update in updates {
            if update.vector.is_empty() || update.vector.iter().any(|value| !value.is_finite()) {
                bail!("임베딩 결과가 비어 있거나 유한하지 않습니다.");
            }
            let raw = encode_vector(&update.vector);
            let count = tx.execute(
                "UPDATE chunks SET vector=?4,model_id=?5 WHERE id=?1 AND file_id=?2
                 AND EXISTS(SELECT 1 FROM files f JOIN file_generations g ON g.file_id=f.id
                            WHERE f.id=?2 AND f.status='content' AND g.generation=?3)",
                params![update.id, update.file_id, update.generation, raw, model_id],
            )?;
            if count > 0 {
                updated_count += count;
                affected.insert((update.file_id, update.generation));
                tx.execute(
                    "INSERT INTO chunk_embedding_context(chunk_id,context_size) VALUES (?1,?2)
                     ON CONFLICT(chunk_id) DO UPDATE SET context_size=excluded.context_size
                     WHERE context_size<>excluded.context_size",
                    params![update.id, context_size as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok((updated_count, affected))
    }

    pub(crate) fn set_content_key_for_generation_on(
        db: &Connection,
        file_id: i64,
        generation: i64,
        model_id: Option<&str>,
        key: &str,
    ) -> Result<()> {
        db.execute(
            "INSERT INTO file_content_keys(file_id,content_key)
             SELECT f.id,?4 FROM files f JOIN file_generations g ON g.file_id=f.id
             WHERE f.id=?1 AND f.status='content' AND f.model_id IS ?3
               AND g.generation=?2
             ON CONFLICT(file_id) DO UPDATE SET content_key=excluded.content_key",
            params![file_id, generation, model_id, key],
        )?;
        Ok(())
    }

    pub(crate) fn complete_embedding_file_on(
        db: &mut Connection,
        file_id: i64,
        generation: i64,
        model_id: &str,
        context_size: u64,
        content_key: Option<&str>,
    ) -> Result<bool> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let complete = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE id=?1 AND status='content'
               AND EXISTS(SELECT 1 FROM file_generations g
                          WHERE g.file_id=files.id AND g.generation=?2)
               AND NOT EXISTS(SELECT 1 FROM chunks c LEFT JOIN chunk_embedding_context ce
                              ON ce.chunk_id=c.id WHERE c.file_id=files.id
                              AND (c.vector IS NULL OR c.model_id IS NOT ?3
                                   OR ce.context_size IS NOT ?4)))",
            params![file_id, generation, model_id, context_size as i64],
            |row| row.get::<_, bool>(0),
        )?;
        if !complete {
            tx.rollback()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE files SET model_id=?3 WHERE id=?1 AND status='content'
               AND model_id IS NOT ?3
               AND EXISTS(SELECT 1 FROM file_generations g
                          WHERE g.file_id=files.id AND g.generation=?2)",
            params![file_id, generation, model_id],
        )?;
        tx.execute(
            "INSERT INTO file_embedding_context(file_id,generation,model_id,context_size)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(file_id) DO UPDATE SET generation=excluded.generation,
                 model_id=excluded.model_id,context_size=excluded.context_size
             WHERE generation<>excluded.generation OR model_id<>excluded.model_id
                OR context_size<>excluded.context_size",
            params![file_id, generation, model_id, context_size as i64],
        )?;
        if let Some(key) = content_key {
            tx.execute(
                "INSERT INTO file_content_keys(file_id,content_key) VALUES (?1,?2)
                 ON CONFLICT(file_id) DO UPDATE SET content_key=excluded.content_key
                 WHERE content_key<>excluded.content_key",
                params![file_id, key],
            )?;
        } else {
            tx.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn file(&self, file_id: i64) -> Result<Option<Value>> {
        let db = self.connect()?;
        let Some(mut file) = db
            .query_row("SELECT * FROM files WHERE id=?1", [file_id], row_json)
            .optional()?
        else {
            return Ok(None);
        };
        let mut stmt =
            db.prepare("SELECT text,location FROM chunks WHERE file_id=?1 ORDER BY id LIMIT 100")?;
        let passages = stmt
            .query_map([file_id], |row| {
                Ok(json!({"text": row.get::<_, String>(0)?, "location": row.get::<_, String>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        file["passages"] = json!(passages);
        Ok(Some(file))
    }

    pub fn replace_files(&self, records: &[FileRecord]) -> Result<()> {
        let mut db = self.connect()?;
        self.replace_files_on(&mut db, records)
    }

    pub fn replace_files_on(&self, db: &mut Connection, records: &[FileRecord]) -> Result<()> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for record in records {
            replace_one(&tx, record)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune(&self, root_id: i64, seen: &HashSet<PathBuf>) -> Result<()> {
        let mut db = self.connect()?;
        self.prune_on(&mut db, root_id, seen)
    }

    pub fn prune_on(
        &self,
        db: &mut Connection,
        root_id: i64,
        seen: &HashSet<PathBuf>,
    ) -> Result<()> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stmt = tx.prepare("SELECT id,path FROM files WHERE root_id=?1")?;
        let stale = stmt
            .query_map([root_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|row| match row {
                Ok((id, path)) if !seen.contains(Path::new(&path)) => Some(Ok(id)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for file_id in stale {
            tx.execute("DELETE FROM files WHERE id=?1", [file_id])?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn replace_one(db: &Connection, record: &FileRecord) -> Result<()> {
    if let Some(vectors) = &record.vectors {
        if vectors.len() != record.passages.len() {
            bail!("임베딩 벡터 수가 문서 구간 수와 일치하지 않습니다.");
        }
    }
    let path = record.path.to_string_lossy().to_string();
    let name = record
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let extension = record
        .path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let status = if record.error.is_some() {
        "error"
    } else if record.passages.is_empty() {
        "metadata"
    } else {
        "content"
    };
    db.execute(
        "INSERT INTO files(root_id,path,name,extension,size,modified,status,error,model_id,reader_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(path) DO UPDATE SET root_id=excluded.root_id,size=excluded.size,
         modified=excluded.modified,status=excluded.status,error=excluded.error,
         model_id=excluded.model_id,reader_id=excluded.reader_id",
        params![record.root_id,path,name,extension,record.size,record.modified,status,record.error,record.model_id,record.reader_id],
    )?;
    let file_id: i64 = db.query_row("SELECT id FROM files WHERE path=?1", [&path], |row| {
        row.get(0)
    })?;
    db.execute("DELETE FROM chunks WHERE file_id=?1", [file_id])?;
    for (index, passage) in record.passages.iter().enumerate() {
        let vector = record
            .vectors
            .as_ref()
            .map(|vectors| encode_vector(&vectors[index]));
        let model_id = if vector.is_some() {
            record.model_id.as_deref()
        } else {
            None
        };
        db.execute(
            "INSERT INTO chunks(file_id,text,location,vector,model_id) VALUES (?1,?2,?3,?4,?5)",
            params![file_id, passage.text, passage.location, vector, model_id],
        )?;
    }
    if record.error.is_none() {
        if let Some(key) = &record.content_key {
            db.execute(
                "INSERT INTO file_content_keys(file_id,content_key) VALUES (?1,?2)
                 ON CONFLICT(file_id) DO UPDATE SET content_key=excluded.content_key",
                params![file_id, key],
            )?;
        } else {
            db.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
        }
    } else {
        db.execute("DELETE FROM file_content_keys WHERE file_id=?1", [file_id])?;
    }
    Ok(())
}

pub(crate) fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub(crate) fn decode_vector(raw: &[u8]) -> Option<Vec<f32>> {
    if raw.len() % 4 != 0 {
        return None;
    }
    Some(
        raw.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
    )
}

pub(crate) fn row_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut output = Map::new();
    for (index, name) in [
        "id",
        "root_id",
        "path",
        "name",
        "extension",
        "size",
        "modified",
        "status",
        "error",
        "model_id",
        "reader_id",
    ]
    .iter()
    .enumerate()
    {
        let value = match *name {
            "id" | "root_id" | "size" | "modified" => row.get::<_, i64>(index).map(Value::from)?,
            "error" | "model_id" => row
                .get::<_, Option<String>>(index)?
                .map(Value::from)
                .unwrap_or(Value::Null),
            _ => Value::from(row.get::<_, String>(index)?),
        };
        output.insert((*name).to_string(), value);
    }
    Ok(Value::Object(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tempdir(prefix: &str) -> tempfile::TempDir {
        let base = Path::new(".moasearch-tests");
        std::fs::create_dir_all(base).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .unwrap()
    }

    #[test]
    fn schema_migrates_and_revision_counters_follow_atomic_writes_and_cascades() {
        let temp = test_tempdir("store-");
        let base = temp.path();
        let library = base.join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(base.join("data")).unwrap();
        let root = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let path = library.join("note.txt");
        let record = FileRecord {
            root_id: root,
            path: path.clone(),
            size: 6,
            modified: 123,
            passages: vec![Passage {
                text: "임대차 메모".into(),
                location: "본문".into(),
            }],
            vectors: Some(vec![vec![1.0, 0.0]]),
            model_id: Some("google-test".into()),
            error: None,
            reader_id: "reader-v2".into(),
            content_key: None,
        };
        store.replace_files(&[record]).unwrap();
        assert_eq!(
            store.counts().unwrap(),
            json!({"files":1,"chunks":1,"vectors":1})
        );
        let before = store.index_state().unwrap()["revision"].as_i64().unwrap();
        store.remove_root(root).unwrap();
        assert_eq!(
            store.counts().unwrap(),
            json!({"files":0,"chunks":0,"vectors":0})
        );
        assert!(store.index_state().unwrap()["revision"].as_i64().unwrap() > before);
    }

    #[test]
    fn old_python_database_gets_reader_column_counters_and_rebuilt_path_fts_once() {
        let temp = test_tempdir("store-migration-");
        let library = temp.path().join("library");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let database = data.join("index.sqlite3");
        let db = Connection::open(&database).unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE roots(id INTEGER PRIMARY KEY,path TEXT UNIQUE NOT NULL);
             CREATE TABLE files(id INTEGER PRIMARY KEY,root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
                 path TEXT UNIQUE NOT NULL,name TEXT NOT NULL,extension TEXT NOT NULL,size INTEGER NOT NULL,
                 modified INTEGER NOT NULL,status TEXT NOT NULL,error TEXT,model_id TEXT);
             CREATE TABLE chunks(id INTEGER PRIMARY KEY,file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                 text TEXT NOT NULL,location TEXT NOT NULL,vector BLOB,model_id TEXT);
             CREATE VIRTUAL TABLE chunks_fts USING fts5(text,content='chunks',content_rowid='id',tokenize='trigram');
             INSERT INTO roots VALUES(1,'library');
             INSERT INTO files VALUES(2,1,'library/계약 보증금.txt','계약 보증금.txt','.txt',10,11,'content',NULL,'legacy-google');
             INSERT INTO chunks VALUES(3,2,'보증금 반환 내용','본문',x'0000803f','legacy-google');
             INSERT INTO chunks_fts(rowid,text) VALUES(3,'보증금 반환 내용');",
        ).unwrap();
        drop(db);

        let store = Store::open(&data).unwrap();
        assert_eq!(
            store.counts().unwrap(),
            json!({"files":1,"chunks":1,"vectors":1})
        );
        assert_eq!(store.file(2).unwrap().unwrap()["reader_id"], "legacy-v1");
        let state = store.index_state().unwrap();
        let db = store.connect().unwrap();
        let matches: i64 = db
            .query_row(
                "SELECT count(*) FROM files_fts WHERE files_fts MATCH '보증금'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matches, 1);
        drop(db);
        assert_eq!(Store::open(&data).unwrap().index_state().unwrap(), state);
    }

    #[test]
    fn failed_file_batch_rolls_back_rows_search_indexes_and_counters() {
        let temp = test_tempdir("store-batch-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let record = FileRecord {
            root_id,
            path: library.join("atomic.txt"),
            size: 1,
            modified: 1,
            passages: vec![Passage {
                text: "atomic batch text".into(),
                location: "본문".into(),
            }],
            vectors: Some(Vec::new()),
            model_id: Some("google-fixture".into()),
            error: None,
            reader_id: "test".into(),
            content_key: None,
        };
        let before = store.index_state().unwrap();
        assert!(store.replace_files(&[record]).is_err());
        assert_eq!(store.index_state().unwrap(), before);
        let db = store.connect().unwrap();
        let files: i64 = db
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap();
        let chunks: i64 = db
            .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!((files, chunks), (0, 0));
        assert!(
            db.query_row(
                "SELECT rowid FROM files_fts WHERE files_fts MATCH 'atomic'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn reusable_content_returns_all_passages_and_validated_vectors() {
        let temp = test_tempdir("store-reuse-payload-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let passages: Vec<_> = (0..105)
            .map(|i| Passage {
                text: format!("본문 조각 {i}"),
                location: format!("페이지 {i}"),
            })
            .collect();
        let vectors = vec![vec![0.25, 0.75]; passages.len()];
        store
            .replace_files(&[FileRecord {
                root_id,
                path: library.join("original.txt"),
                size: 10,
                modified: 1,
                passages: passages.clone(),
                vectors: Some(vectors.clone()),
                model_id: Some("embedding-fixture".into()),
                error: None,
                reader_id: "reader-v2".into(),
                content_key: Some("opaque-key".into()),
            }])
            .unwrap();

        let db = store.connect().unwrap();
        let file_id: i64 = db
            .query_row(
                "SELECT id FROM files WHERE path=?1",
                [library.join("original.txt").to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            Store::content_key_on(&db, file_id).unwrap().as_deref(),
            Some("opaque-key")
        );
        let cached = Store::reusable_content_on(&db, "opaque-key")
            .unwrap()
            .unwrap();
        assert_eq!(cached.passages.len(), 105);
        assert_eq!(cached.passages, passages);
        assert_eq!(cached.vectors.unwrap(), vectors);
        assert_eq!(cached.model_id.as_deref(), Some("embedding-fixture"));
        assert!(
            Store::reusable_content_on(&db, "missing-key")
                .unwrap()
                .is_none()
        );
        db.execute(
            "UPDATE chunks SET vector=?1 WHERE file_id=?2 AND id=(
                SELECT min(id) FROM chunks WHERE file_id=?2)",
            params![encode_vector(&[f32::NAN, 1.0]), file_id],
        )
        .unwrap();
        assert!(
            Store::reusable_content_on(&db, "opaque-key")
                .unwrap()
                .is_none()
        );
        db.execute(
            "UPDATE chunks SET vector=?1,model_id='different-model'
             WHERE file_id=?2 AND id=(SELECT min(id) FROM chunks WHERE file_id=?2)",
            params![encode_vector(&[1.0, 0.0]), file_id],
        )
        .unwrap();
        assert!(
            Store::reusable_content_on(&db, "opaque-key")
                .unwrap()
                .is_none()
        );
        db.execute(
            "UPDATE chunks SET vector=?1,model_id='embedding-fixture'
             WHERE file_id=?2 AND id=(SELECT min(id) FROM chunks WHERE file_id=?2)",
            params![encode_vector(&[0.0, 0.0]), file_id],
        )
        .unwrap();
        assert!(
            Store::reusable_content_on(&db, "opaque-key")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_missing_vectors_when_file_declares_embedding_model() {
        let temp = test_tempdir("store-reuse-missing-vector-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        store
            .replace_files(&[FileRecord {
                root_id,
                path: library.join("embedded.txt"),
                size: 4,
                modified: 1,
                passages: vec![Passage {
                    text: "body".into(),
                    location: "본문".into(),
                }],
                vectors: Some(vec![vec![0.5, 0.5]]),
                model_id: Some("embedding-fixture".into()),
                error: None,
                reader_id: "reader-v2".into(),
                content_key: Some("embedded-key".into()),
            }])
            .unwrap();
        let db = store.connect().unwrap();
        db.execute("UPDATE chunks SET vector=NULL", []).unwrap();
        assert!(
            Store::reusable_content_on(&db, "embedded-key")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_expected_metadata_cannot_attach_a_legacy_content_key() {
        let temp = test_tempdir("store-reuse-stale-backfill-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let path = library.join("backfill.txt");
        store
            .replace_files(&[FileRecord {
                root_id,
                path: path.clone(),
                size: 4,
                modified: 10,
                passages: vec![Passage {
                    text: "body".into(),
                    location: "본문".into(),
                }],
                vectors: None,
                model_id: None,
                error: None,
                reader_id: "reader-v2".into(),
                content_key: None,
            }])
            .unwrap();
        let db = store.connect().unwrap();
        let expected = Store::file_by_path_on(&db, &path).unwrap().unwrap();
        let file_id = expected["id"].as_i64().unwrap();
        db.execute(
            "UPDATE files SET modified=modified+1 WHERE id=?1",
            [file_id],
        )
        .unwrap();

        Store::set_content_key_on(&db, file_id, "stale-key", &expected).unwrap();
        assert_eq!(Store::content_key_on(&db, file_id).unwrap(), None);
        let fresh = Store::file_by_path_on(&db, &path).unwrap().unwrap();
        Store::set_content_key_on(&db, file_id, "fresh-key", &fresh).unwrap();
        assert_eq!(
            Store::content_key_on(&db, file_id).unwrap().as_deref(),
            Some("fresh-key")
        );
    }

    #[test]
    fn replacement_without_valid_key_and_error_replacement_invalidate_cache() {
        let temp = test_tempdir("store-reuse-invalidate-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let make_record = |key: Option<&str>, error: Option<&str>| FileRecord {
            root_id,
            path: library.join("copy.txt"),
            size: 4,
            modified: 1,
            passages: vec![Passage {
                text: "same body".into(),
                location: "body".into(),
            }],
            vectors: None,
            model_id: None,
            error: error.map(str::to_owned),
            reader_id: "reader-v2".into(),
            content_key: key.map(str::to_owned),
        };
        store
            .replace_files(&[make_record(Some("cached"), None)])
            .unwrap();
        let db = store.connect().unwrap();
        let file_id: i64 = db
            .query_row("SELECT id FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            Store::content_key_on(&db, file_id).unwrap().as_deref(),
            Some("cached")
        );
        let keyword_only = Store::reusable_content_on(&db, "cached").unwrap().unwrap();
        assert!(keyword_only.vectors.is_none());
        assert_eq!(keyword_only.passages.len(), 1);
        drop(db);

        store.replace_files(&[make_record(None, None)]).unwrap();
        let db = store.connect().unwrap();
        assert_eq!(Store::content_key_on(&db, file_id).unwrap(), None);
        drop(db);
        store
            .replace_files(&[make_record(Some("must-not-cache"), Some("reader failed"))])
            .unwrap();
        let db = store.connect().unwrap();
        assert_eq!(Store::content_key_on(&db, file_id).unwrap(), None);
        assert!(
            Store::reusable_content_on(&db, "must-not-cache")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cache_updates_rollback_with_file_batch_and_prune_cascades_key() {
        let temp = test_tempdir("store-reuse-atomic-");
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Store::open(temp.path().join("data")).unwrap();
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        let valid = FileRecord {
            root_id,
            path: library.join("atomic.txt"),
            size: 1,
            modified: 1,
            passages: vec![Passage {
                text: "original".into(),
                location: "body".into(),
            }],
            vectors: None,
            model_id: None,
            error: None,
            reader_id: "reader-v2".into(),
            content_key: Some("old-key".into()),
        };
        store.replace_files(&[valid.clone()]).unwrap();
        let mut bad = valid.clone();
        bad.path = library.join("bad.txt");
        bad.vectors = Some(Vec::new());
        bad.content_key = Some("bad-key".into());
        let mut replacement = valid.clone();
        replacement.passages[0].text = "uncommitted replacement".into();
        replacement.content_key = Some("new-key".into());
        assert!(store.replace_files(&[replacement, bad]).is_err());
        let db = store.connect().unwrap();
        let file_id: i64 = db
            .query_row(
                "SELECT id FROM files WHERE path=?1",
                [valid.path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            Store::content_key_on(&db, file_id).unwrap().as_deref(),
            Some("old-key")
        );
        assert_eq!(
            Store::reusable_content_on(&db, "old-key")
                .unwrap()
                .unwrap()
                .passages[0]
                .text,
            "original"
        );
        drop(db);

        store.prune(root_id, &HashSet::new()).unwrap();
        let db = store.connect().unwrap();
        assert_eq!(Store::content_key_on(&db, file_id).unwrap(), None);
        assert!(
            Store::reusable_content_on(&db, "old-key")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn filename_only_directory_settings_normalize_deduplicate_and_allow_empty() {
        let temp = test_tempdir("store-filename-only-settings-");
        let store = Store::open(temp.path().join("data")).unwrap();
        let saved = store
            .update_settings(&json!({"filename_only_dirs":" _SITE, Temp;\r\n vendor, TEMP "}))
            .unwrap();
        assert_eq!(saved["filename_only_dirs"], "_site,temp,vendor");
        let saved = store
            .update_settings(&json!({"filename_only_dirs":""}))
            .unwrap();
        assert_eq!(saved["filename_only_dirs"], "");
    }

    #[test]
    fn invalid_filename_only_directory_paths_leave_settings_unchanged() {
        let temp = test_tempdir("store-filename-only-invalid-");
        let store = Store::open(temp.path().join("data")).unwrap();
        store
            .update_settings(&json!({"filename_only_dirs":"_site,temp"}))
            .unwrap();
        for invalid in ["../private", "folder\\child", "C:\\private", "*cache"] {
            assert!(
                store
                    .update_settings(&json!({"filename_only_dirs":invalid}))
                    .is_err()
            );
            assert_eq!(
                store.settings().unwrap()["filename_only_dirs"],
                "_site,temp"
            );
        }
    }

    #[test]
    fn chat_and_gpu_settings_require_supported_types_and_values() {
        let temp = test_tempdir("store-feature-settings-");
        let store = Store::open(temp.path().join("data")).unwrap();
        assert!(
            store
                .update_settings(&json!({"chat_enabled":"false"}))
                .is_err()
        );
        assert!(store.update_settings(&json!({"gpu_mode":true})).is_err());
        assert!(store.update_settings(&json!({"gpu_mode":"cuda"})).is_err());
        let saved = store
            .update_settings(&json!({"chat_enabled":true,"gpu_mode":"cpu"}))
            .unwrap();
        assert_eq!(saved["chat_enabled"], true);
        assert_eq!(saved["gpu_mode"], "cpu");
    }
}
