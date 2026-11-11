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
        Self::prepare_search_metadata(&mut db)?;
        Ok(store)
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
}
