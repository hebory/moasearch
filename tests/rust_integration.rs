use anyhow::Result;
use moasearch::{
    commands::{Command, ListFilesInput, SearchInput},
    config,
    engine::{EmbeddedEngine, Embedder},
    indexer::Indexer,
    search::Search,
    service::AppServices,
    store::Store,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct MockEngine {
    calls: AtomicUsize,
    delay: bool,
}

fn scratch() -> Result<tempfile::TempDir> {
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".moasearch-tests");
    std::fs::create_dir_all(&parent)?;
    Ok(tempfile::tempdir_in(parent)?)
}

#[test]
fn metadata_file_listing_paginates_and_resets_only_for_list_changes() -> Result<()> {
    let defaults: ListFilesInput = serde_json::from_value(json!({}))?;
    assert_eq!(defaults.limit, 200);
    defaults.validate()?;
    let mut invalid = defaults.clone();
    invalid.limit = 501;
    assert!(invalid.validate().is_err());
    invalid.limit = 20;
    invalid.extension = Some(".".into());
    assert!(invalid.validate().is_err());
    let dir = scratch()?;
    let root = dir.path().join("목록");
    for relative in [
        "docs/sub",
        "docs-extra",
        "Docs",
        "docs%_special",
        "docs%_special-other",
    ] {
        std::fs::create_dir_all(root.join(relative))?;
    }
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let root_id = store.add_root(&root)?["id"].as_i64().unwrap();
    let files = [
        ("docs/a.txt", "a.txt", ".txt"),
        ("docs/sub/b.txt", "b.txt", ".txt"),
        ("docs-extra/c.pdf", "c.pdf", ".pdf"),
        ("Docs/f.txt", "f.txt", ".txt"),
        ("docs%_special/d.txt", "d.txt", ".txt"),
        ("docs%_special-other/e.txt", "e.txt", ".txt"),
    ];
    let db = store.connect()?;
    for (index, (relative, name, extension)) in files.iter().enumerate() {
        let path = root.join(relative);
        std::fs::write(&path, b"x")?;
        let path = config::canonical(&path)?;
        db.execute(
            "INSERT INTO files(root_id,path,name,extension,size,modified,status,error,model_id,reader_id)
             VALUES(?1,?2,?3,?4,1,?5,'complete',NULL,NULL,'test-v1')",
            rusqlite::params![
                root_id,
                path.to_string_lossy().as_ref(),
                name,
                extension,
                index as i64,
            ],
        )?;
        if index == 0 {
            let file_id = db.last_insert_rowid();
            db.execute(
                "INSERT INTO chunks(file_id,text,location,vector,model_id)
                 VALUES(?1,'본문을 목록에 싣지 않습니다.','',NULL,NULL)",
                [file_id],
            )?;
        }
    }
    drop(db);

    let services = AppServices::new(store.clone(), Arc::new(EmbeddedEngine::new(store.clone())));
    let first = services.execute(Command::ListFiles(ListFilesInput {
        limit: 2,
        extension: None,
        folder: None,
        cursor: None,
    }))?;
    assert_eq!(first["total"], 6);
    assert_eq!(first["results"].as_array().unwrap().len(), 2);
    assert_eq!(first["reset_required"], false);
    assert!(first["results"][0].get("text").is_none());
    let cursor: moasearch::commands::ListCursor =
        serde_json::from_value(first["next_cursor"].clone())?;

    let stable_revision = first["revision"].as_i64().unwrap();
    assert_eq!(
        services.execute(Command::Status {})?["list_revision"],
        stable_revision
    );
    let db = store.connect()?;
    db.execute("UPDATE files SET status='pending' WHERE name='a.txt'", [])?;
    db.execute(
        "UPDATE chunks SET vector=?1,model_id='fixture-google' WHERE file_id=(SELECT id FROM files WHERE name='a.txt')",
        rusqlite::params![vec![0_u8; 12]],
    )?;
    drop(db);
    assert_eq!(store.list_revision()?, stable_revision);
    let second = services.execute(Command::ListFiles(ListFilesInput {
        limit: 2,
        extension: None,
        folder: None,
        cursor: Some(cursor.clone()),
    }))?;
    assert_eq!(second["reset_required"], false);
    assert_eq!(second["results"].as_array().unwrap().len(), 2);
    let third_cursor: moasearch::commands::ListCursor =
        serde_json::from_value(second["next_cursor"].clone())?;
    let third = services.execute(Command::ListFiles(ListFilesInput {
        limit: 2,
        extension: None,
        folder: None,
        cursor: Some(third_cursor),
    }))?;
    assert_eq!(third["results"].as_array().unwrap().len(), 2);
    assert!(third["next_cursor"].is_null());
    let first_ids = first["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["id"].as_i64().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert!(
        second["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| { !first_ids.contains(&file["id"].as_i64().unwrap()) })
    );
    assert!(third["results"].as_array().unwrap().iter().all(|file| {
        !first_ids.contains(&file["id"].as_i64().unwrap())
            && !second["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|prior| prior["id"] == file["id"])
    }));

    let scoped = services.execute(Command::ListFiles(ListFilesInput {
        limit: 20,
        extension: Some("TXT".into()),
        folder: Some(root.join("docs%_special")),
        cursor: None,
    }))?;
    assert_eq!(scoped["total"], 1);
    assert_eq!(scoped["results"][0]["name"], "d.txt");
    let case_scoped = services.execute(Command::ListFiles(ListFilesInput {
        limit: 20,
        extension: None,
        folder: Some(root.join("docs")),
        cursor: None,
    }))?;
    #[cfg(windows)]
    assert_eq!(case_scoped["total"], 3);
    #[cfg(not(windows))]
    {
        assert_eq!(case_scoped["total"], 2);
        assert!(
            case_scoped["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file["name"] != "f.txt")
        );
    }

    let db = store.connect()?;
    db.execute("UPDATE files SET name='renamed.txt' WHERE name='e.txt'", [])?;
    drop(db);
    let stale = services.execute(Command::ListFiles(ListFilesInput {
        limit: 2,
        extension: None,
        folder: None,
        cursor: Some(cursor),
    }))?;
    assert_eq!(stale["reset_required"], true);
    assert_eq!(stale["results"].as_array().unwrap().len(), 0);
    assert!(stale["next_cursor"].is_null());
    assert!(stale["revision"].as_i64().unwrap() > stable_revision);
    Ok(())
}
impl Embedder for MockEngine {
    fn model_id(&self) -> Option<String> {
        Some("fixture-google".into())
    }
    fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.delay {
            std::thread::sleep(Duration::from_millis(150));
        }
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
    fn answer(&self, _: &str, _: &str) -> Result<String> {
        Ok("예제 답변 [1]".into())
    }
}

struct FailOnceEngine {
    failed: AtomicBool,
}

impl Embedder for FailOnceEngine {
    fn model_id(&self) -> Option<String> {
        Some("fixture-google".into())
    }
    fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            anyhow::bail!("fixture embedding failure");
        }
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
    fn answer(&self, _: &str, _: &str) -> Result<String> {
        Ok("예제 답변 [1]".into())
    }
}

struct NamedEngine {
    id: String,
    calls: AtomicUsize,
    encoded_texts: AtomicUsize,
}

struct StaleGenerationEngine {
    store: Arc<Store>,
    mutated: AtomicBool,
}

impl Embedder for StaleGenerationEngine {
    fn model_id(&self) -> Option<String> {
        Some("fixture-google".into())
    }
    fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        if !self.mutated.swap(true, Ordering::SeqCst) {
            let db = self.store.connect()?;
            db.execute("UPDATE file_generations SET generation=generation+1", [])?;
        }
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
    fn answer(&self, _: &str, _: &str) -> Result<String> {
        Ok("예제 답변 [1]".into())
    }
}

impl Embedder for NamedEngine {
    fn model_id(&self) -> Option<String> {
        Some(self.id.clone())
    }
    fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.encoded_texts.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }
    fn answer(&self, _: &str, _: &str) -> Result<String> {
        Ok("예제 답변 [1]".into())
    }
}
fn wait(index: &Indexer) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    while index.status()["running"] == true {
        assert!(Instant::now() < until, "index timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    index.status()
}

#[test]
fn incremental_index_keeps_text_and_reuses_vectors() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    for i in 0..96 {
        std::fs::write(
            docs.join(format!("계약_{i}.txt")),
            format!("계약번호 {i} 보증금은 14일 이내 반환합니다."),
        )?;
    }
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let engine = Arc::new(MockEngine {
        calls: AtomicUsize::new(0),
        delay: false,
    });
    let index = Indexer::new(store.clone(), engine.clone());
    assert!(index.start());
    let first = wait(&index);
    assert_eq!(first["errors"], 0);
    assert_eq!(first["phase"], "complete");
    assert_eq!(first["indexed"], 96);
    assert_eq!(store.counts()?["vectors"], 96);
    let calls = engine.calls.load(Ordering::SeqCst);
    let stable_revision = store.index_state()?["revision"].as_i64().unwrap();
    index.start();
    let second = wait(&index);
    assert_eq!(second["skipped"], 96);
    assert_eq!(second["phase"], "complete");
    assert_eq!(engine.calls.load(Ordering::SeqCst), calls);
    assert_eq!(store.index_state()?["revision"], stable_revision);
    std::fs::write(
        docs.join("계약_0.txt"),
        "수정 계약번호 0 보증금은 30일 이내 반환합니다.",
    )?;
    std::fs::remove_file(docs.join("계약_1.txt"))?;
    index.start();
    let third = wait(&index);
    assert_eq!(third["errors"], 0);
    assert_eq!(third["phase"], "complete");
    assert_eq!(third["indexed"], 1);
    assert_eq!(store.counts()?["files"], 95);
    let search = Search::new(store, engine);
    let result = search.search("30일", 5, None, "keyword")?;
    assert_eq!(result["results"][0]["name"], "계약_0.txt");
    Ok(())
}

#[test]
fn partial_index_preserves_text_and_retries_after_embedding_and_hwp_failures() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    std::fs::write(
        docs.join("valid.txt"),
        "검증본문보존 다음 실행에서 벡터를 다시 생성합니다.",
    )?;
    std::fs::write(docs.join("broken.hwp"), "깨진 한글 문서".as_bytes())?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let engine = Arc::new(FailOnceEngine {
        failed: AtomicBool::new(false),
    });
    let index = Indexer::new(store.clone(), engine);

    assert!(index.start());
    let first = wait(&index);
    assert_eq!(first["phase"], "partial");
    assert_eq!(first["errors"], 2);
    assert_eq!(first["current_chunk"], 0);
    assert_eq!(first["total_chunks"], 0);
    assert_eq!(store.counts()?["chunks"], 1);
    assert_eq!(store.counts()?["vectors"], 0);
    let db = store.connect()?;
    let content_keys: i64 = db.query_row("SELECT count(*) FROM file_content_keys", [], |row| {
        row.get(0)
    })?;
    assert_eq!(content_keys, 0);
    drop(db);
    let preserved = Search::new(
        store.clone(),
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: false,
        }),
    )
    .search("검증본문보존", 5, None, "keyword")?;
    assert_eq!(preserved["results"][0]["name"], "valid.txt");

    assert!(index.start());
    let retry = wait(&index);
    assert_eq!(retry["phase"], "partial");
    assert_eq!(retry["errors"], 1);
    assert_eq!(retry["current_chunk"], 0);
    assert_eq!(retry["total_chunks"], 0);
    assert_eq!(store.counts()?["chunks"], 1);
    assert_eq!(store.counts()?["vectors"], 1);
    let db = store.connect()?;
    let content_keys: i64 = db.query_row("SELECT count(*) FROM file_content_keys", [], |row| {
        row.get(0)
    })?;
    assert_eq!(content_keys, 1);
    Ok(())
}

#[test]
fn index_status_reports_embedding_chunk_progress_and_clears_it_on_completion() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    std::fs::write(docs.join("long.txt"), "progress token. ".repeat(600))?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let index = Indexer::new(
        store,
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: true,
        }),
    );

    assert!(index.start());
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let status = index.status();
        if status["phase"] == "embedding" && status["current_chunk"].as_u64().unwrap_or(0) > 0 {
            assert!(status["total_chunks"].as_u64().unwrap_or(0) > 4);
            assert!(
                status["current_chunk"].as_u64().unwrap()
                    <= status["total_chunks"].as_u64().unwrap()
            );
            break;
        }
        assert!(
            Instant::now() < until,
            "embedding progress was not published: {status}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let finished = wait(&index);
    assert_eq!(finished["phase"], "complete");
    assert_eq!(finished["errors"], 0);
    assert_eq!(finished["current_chunk"], 0);
    assert_eq!(finished["total_chunks"], 0);
    Ok(())
}

#[test]
fn filenames_and_extracted_text_are_searchable_before_embedding_finishes() -> Result<()> {
    struct PausedEngine {
        started: AtomicBool,
        released: Arc<AtomicBool>,
    }
    impl Embedder for PausedEngine {
        fn model_id(&self) -> Option<String> {
            Some("fixture-google".into())
        }
        fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
            self.started.store(true, Ordering::Release);
            while !self.released.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
        }
        fn answer(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
    }
    struct ReleaseOnDrop(Arc<AtomicBool>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    std::fs::write(
        docs.join("급여명세서_2026.txt"),
        "이번 달 급여명세서 총액은 350만원입니다.",
    )?;
    std::fs::write(
        docs.join("회의기록_2026.txt"),
        "프로젝트 회의기록에서 출시 일정을 확인합니다.",
    )?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let released = Arc::new(AtomicBool::new(false));
    let _release = ReleaseOnDrop(released.clone());
    let engine = Arc::new(PausedEngine {
        started: AtomicBool::new(false),
        released: released.clone(),
    });
    let index = Indexer::new(store.clone(), engine.clone());
    assert!(index.start());

    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let status = index.status();
        if engine.started.load(Ordering::Acquire) {
            assert_eq!(status["running"], true);
            assert_eq!(status["metadata_done"], 2);
            assert_eq!(status["metadata_total"], 2);
            assert_eq!(status["content_done"], 2);
            break;
        }
        assert!(
            Instant::now() < until,
            "embedding phase was not reached: {status}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let search = Search::new(
        store,
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: false,
        }),
    );
    let filename = search.search("급여명세서", 10, None, "keyword")?;
    assert!(
        filename["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| { file["name"] == "급여명세서_2026.txt" })
    );
    let body = search.search("350만원", 10, None, "keyword")?;
    assert!(
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| { file["name"] == "급여명세서_2026.txt" })
    );

    released.store(true, Ordering::Release);
    let finished = wait(&index);
    assert_eq!(finished["phase"], "complete");
    Ok(())
}

#[test]
fn generation_conflict_leaves_index_partial_instead_of_reporting_complete() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    std::fs::write(docs.join("generation.txt"), "generation conflict body")?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let index = Indexer::new(
        store.clone(),
        Arc::new(StaleGenerationEngine {
            store: store.clone(),
            mutated: AtomicBool::new(false),
        }),
    );
    assert!(index.start());
    let status = wait(&index);
    assert_eq!(status["errors"], 0);
    assert_eq!(status["embedding_state"], "pending");
    assert_eq!(status["phase"], "partial");
    assert_eq!(status["embedding_done_chunks"], 0);
    assert!(status["embedding_total_chunks"].as_u64().unwrap() > 0);
    Ok(())
}

#[test]
fn identical_files_reuse_vectors_within_a_run_and_after_indexer_restart() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let shared = "동일한 파일 본문은 한 번만 읽고 임베딩합니다.";
    std::fs::write(docs.join("copy-a.txt"), shared)?;
    std::fs::write(docs.join("copy-b.txt"), shared)?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let engine = Arc::new(NamedEngine {
        id: "fixture-google".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });
    let first_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(first_indexer.start());
    let first = wait(&first_indexer);
    assert_eq!(first["phase"], "complete");
    assert_eq!(first["indexed"], 2);
    assert_eq!(first["reused"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.counts()?["vectors"], 2);

    std::fs::write(docs.join("copy-c.txt"), shared)?;
    let restarted_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(restarted_indexer.start());
    let restarted = wait(&restarted_indexer);
    assert_eq!(restarted["phase"], "complete");
    assert_eq!(restarted["indexed"], 1);
    assert_eq!(restarted["reused"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.counts()?["files"], 3);
    assert_eq!(store.counts()?["vectors"], 3);
    Ok(())
}

#[test]
fn invalid_cached_vector_is_reembedded_instead_of_skipped() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let path = docs.join("recover.txt");
    std::fs::write(&path, "손상된 벡터는 다음 색인에서 다시 생성해야 합니다.")?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let engine = Arc::new(NamedEngine {
        id: "fixture-google".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });
    let first_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(first_indexer.start());
    let first = wait(&first_indexer);
    assert_eq!(first["phase"], "complete");
    assert_eq!(first["indexed"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.counts()?["vectors"], 1);

    let stored_path = config::canonical(&path)?.to_string_lossy().into_owned();
    let db = store.connect()?;
    db.execute(
        "UPDATE chunks SET vector=zeroblob(length(vector)) WHERE file_id=(SELECT id FROM files WHERE path=?1)",
        [&stored_path],
    )?;
    drop(db);

    let recovery_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(recovery_indexer.start());
    let recovered = wait(&recovery_indexer);
    assert_eq!(recovered["phase"], "complete");
    assert_eq!(recovered["indexed"], 0);
    assert_eq!(recovered["reused"], 0);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.counts()?["vectors"], 1);

    let db = store.connect()?;
    let vector: Vec<u8> = db.query_row(
        "SELECT c.vector FROM chunks c JOIN files f ON f.id=c.file_id WHERE f.path=?1 LIMIT 1",
        [&stored_path],
        |row| row.get(0),
    )?;
    let decoded: Vec<f32> = vector
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four-byte vector element")))
        .collect();
    assert!(!decoded.is_empty());
    assert!(decoded.iter().all(|value| value.is_finite()));
    assert!(decoded.iter().map(|value| value * value).sum::<f32>() > f32::EPSILON);
    Ok(())
}

#[test]
fn legacy_content_is_backfilled_then_reused_from_store() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let text = "기존 색인 본문을 읽지 않고 재사용합니다.";
    let original = docs.join("legacy.txt");
    std::fs::write(&original, text)?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let root = store.add_root(&docs)?;
    let metadata = std::fs::metadata(&original)?;
    store.replace_files(&[moasearch::store::FileRecord {
        root_id: root["id"].as_i64().unwrap(),
        path: config::canonical(&original)?,
        size: metadata.len() as i64,
        modified: config::modified_ns(&metadata),
        passages: vec![moasearch::extract::Passage {
            text: text.into(),
            location: "본문".into(),
        }],
        vectors: Some(vec![vec![1.0, 0.0, 0.0]]),
        model_id: Some("fixture-google".into()),
        error: None,
        reader_id: "rust-content-v1".into(),
        content_key: None,
    }])?;
    let engine = Arc::new(NamedEngine {
        id: "fixture-google".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });

    let backfill_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(backfill_indexer.start());
    let backfilled = wait(&backfill_indexer);
    assert_eq!(backfilled["phase"], "complete");
    assert_eq!(backfilled["skipped"], 1);
    // This legacy row has vectors but no cache key carrying its context size;
    // re-embed it once before reusing its now-verified cache for copies.
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);

    std::fs::write(docs.join("legacy-copy.txt"), text)?;
    let restarted_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(restarted_indexer.start());
    let reused = wait(&restarted_indexer);
    assert_eq!(reused["phase"], "complete");
    assert_eq!(reused["indexed"], 1);
    assert_eq!(reused["reused"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.counts()?["vectors"], 2);
    Ok(())
}

#[test]
fn identical_passages_in_different_files_reuse_only_the_shared_chunk_vector() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let shared_chunk = "중복".repeat(600);
    std::fs::write(docs.join("a.txt"), format!("{shared_chunk}A"))?;
    std::fs::write(docs.join("b.txt"), format!("{shared_chunk}B"))?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let engine = Arc::new(NamedEngine {
        id: "fixture-google".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });
    let indexer = Indexer::new(store.clone(), engine.clone());

    assert!(indexer.start());
    let status = wait(&indexer);
    assert_eq!(status["phase"], "complete");
    assert_eq!(status["reused_chunks"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(engine.encoded_texts.load(Ordering::SeqCst), 3);
    assert_eq!(store.counts()?["files"], 2);
    assert_eq!(store.counts()?["vectors"], 4);
    Ok(())
}

#[test]
fn file_content_cache_identity_invalidates_for_changed_content_extension_context_and_model()
-> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let original = docs.join("one.txt");
    std::fs::write(&original, "first version of cached text")?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let first_engine = Arc::new(NamedEngine {
        id: "fixture-google-v1".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });
    let first_indexer = Indexer::new(store.clone(), first_engine.clone());
    assert!(first_indexer.start());
    assert_eq!(wait(&first_indexer)["phase"], "complete");
    assert_eq!(first_engine.calls.load(Ordering::SeqCst), 1);

    let changed = "second version of cached text with new content";
    std::fs::write(&original, changed)?;
    let changed_indexer = Indexer::new(store.clone(), first_engine.clone());
    assert!(changed_indexer.start());
    let changed_status = wait(&changed_indexer);
    assert_eq!(changed_status["indexed"], 1);
    assert_eq!(changed_status["reused"], 0);
    assert_eq!(first_engine.calls.load(Ordering::SeqCst), 2);

    std::fs::write(docs.join("same-content.md"), changed)?;
    let extension_indexer = Indexer::new(store.clone(), first_engine.clone());
    assert!(extension_indexer.start());
    let extension_status = wait(&extension_indexer);
    assert_eq!(extension_status["indexed"], 1);
    assert_eq!(extension_status["reused"], 0);
    assert_eq!(extension_status["reused_chunks"], 1);
    assert_eq!(first_engine.calls.load(Ordering::SeqCst), 2);

    store.update_settings(&json!({"context_size":8192}))?;
    let context_indexer = Indexer::new(store.clone(), first_engine.clone());
    assert!(context_indexer.start());
    let context_status = wait(&context_indexer);
    assert_eq!(context_status["indexed"], 0);
    assert_eq!(context_status["reused"], 0);
    assert_eq!(context_status["reused_chunks"], 1);
    assert_eq!(first_engine.calls.load(Ordering::SeqCst), 3);

    let second_engine = Arc::new(NamedEngine {
        id: "fixture-google-v2".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });
    let model_indexer = Indexer::new(store, second_engine.clone());
    assert!(model_indexer.start());
    let model_status = wait(&model_indexer);
    assert_eq!(model_status["indexed"], 0);
    assert_eq!(model_status["reused"], 0);
    assert_eq!(model_status["reused_chunks"], 1);
    assert_eq!(second_engine.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn generated_directories_keep_names_hide_content_and_restore_when_policy_is_cleared() -> Result<()>
{
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    let normal = docs.join("normal");
    let site = docs.join("_site");
    let temp = docs.join("TEMP");
    let vendor = docs.join("vendor");
    for folder in [&normal, &site, &temp, &vendor] {
        std::fs::create_dir_all(folder)?;
    }

    let shared_body = "공통 배포 산출물 본문 원본 페이지 clone payload";
    std::fs::write(normal.join("source-page.txt"), shared_body)?;
    std::fs::write(site.join("site-clone.txt"), shared_body)?;
    std::fs::write(temp.join("temp-clone.txt"), shared_body)?;
    std::fs::write(vendor.join("vendor-clone.txt"), shared_body)?;
    std::fs::write(
        site.join("site-only-file.txt"),
        "SITE_ONLY_BODY_TOKEN 생성 본문",
    )?;
    std::fs::write(
        temp.join("temp-only-file.txt"),
        "TEMP_ONLY_BODY_TOKEN 생성 본문",
    )?;
    std::fs::write(
        vendor.join("vendor-only-file.txt"),
        "VENDOR_ONLY_BODY_TOKEN 생성 본문",
    )?;
    std::fs::write(
        docs.join("_site-marker.txt"),
        "root level named directory marker body",
    )?;

    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    store.update_settings(&json!({"filename_only_dirs":""}))?;
    let engine = Arc::new(NamedEngine {
        id: "fixture-google".into(),
        calls: AtomicUsize::new(0),
        encoded_texts: AtomicUsize::new(0),
    });

    let initial_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(initial_indexer.start());
    let initial = wait(&initial_indexer);
    assert_eq!(initial["phase"], "complete");
    assert_eq!(store.counts()?["files"], 8);
    assert_eq!(store.counts()?["chunks"], 8);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);
    assert_eq!(engine.encoded_texts.load(Ordering::SeqCst), 5);

    store.update_settings(&json!({"filename_only_dirs":"_site, temp, VENDOR"}))?;
    let policy_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(policy_indexer.start());
    let policy = wait(&policy_indexer);
    assert_eq!(policy["phase"], "complete");
    assert_eq!(policy["indexed"], 6);
    assert_eq!(policy["skipped"], 2);
    assert_eq!(store.counts()?["files"], 8);
    assert_eq!(store.counts()?["chunks"], 2);
    assert_eq!(store.counts()?["vectors"], 2);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);

    let db = store.connect()?;
    let metadata_rows: i64 = db.query_row(
        "SELECT count(*) FROM files WHERE reader_id LIKE 'filename-only-v1:%' AND status='metadata'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(metadata_rows, 6);
    let remaining_keys: i64 = db.query_row(
        "SELECT count(*) FROM file_content_keys k JOIN files f ON f.id=k.file_id WHERE f.reader_id LIKE 'filename-only-v1:%'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(remaining_keys, 0);
    drop(db);

    let stable_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(stable_indexer.start());
    let stable = wait(&stable_indexer);
    assert_eq!(stable["phase"], "complete");
    assert_eq!(stable["indexed"], 0);
    assert_eq!(stable["skipped"], 8);
    assert_eq!(store.counts()?["chunks"], 2);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);

    let search = Search::new(store.clone(), engine.clone());
    for body_token in [
        "SITE_ONLY_BODY_TOKEN",
        "TEMP_ONLY_BODY_TOKEN",
        "VENDOR_ONLY_BODY_TOKEN",
    ] {
        assert!(
            search.search(body_token, 10, None, "keyword")?["results"]
                .as_array()
                .unwrap()
                .is_empty(),
            "generated body became searchable: {body_token}"
        );
    }
    let filename = search.search("site-only-file.txt", 10, None, "keyword")?;
    assert!(
        filename["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| { row["name"] == "site-only-file.txt" && row["status"] == "metadata" })
    );
    let root_file = search.search("_site-marker.txt", 10, None, "keyword")?;
    assert!(
        root_file["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "_site-marker.txt" && row["status"] == "content")
    );

    store.update_settings(&json!({"filename_only_dirs":""}))?;
    let restored_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(restored_indexer.start());
    let restored = wait(&restored_indexer);
    assert_eq!(restored["phase"], "complete");
    assert_eq!(restored["indexed"], 6);
    assert_eq!(restored["reused"], 3);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 3);
    assert_eq!(engine.encoded_texts.load(Ordering::SeqCst), 8);
    assert_eq!(store.counts()?["files"], 8);
    assert_eq!(store.counts()?["chunks"], 8);
    assert_eq!(store.counts()?["vectors"], 8);
    let restored_search = Search::new(store, engine);
    assert_eq!(
        restored_search.search("SITE_ONLY_BODY_TOKEN", 10, None, "keyword")?["results"][0]["name"],
        "site-only-file.txt"
    );
    Ok(())
}

#[test]
fn cancellation_preserves_prior_records_and_publishes_completed_files() -> Result<()> {
    let dir = scratch()?;
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    for i in 0..8 {
        std::fs::write(docs.join(format!("file{i}.txt")), "기존 프로젝트 일정")?;
    }
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&docs)?;
    let fast = Arc::new(MockEngine {
        calls: AtomicUsize::new(0),
        delay: false,
    });
    let first = Indexer::new(store.clone(), fast);
    first.start();
    wait(&first);
    std::fs::remove_file(docs.join("file7.txt"))?;
    for i in 0..7 {
        std::fs::write(
            docs.join(format!("file{i}.txt")),
            format!("서로 다른 프로젝트 {i}의 수정 일정과 변경 사항"),
        )?;
    }
    let slow = Arc::new(MockEngine {
        calls: AtomicUsize::new(0),
        delay: true,
    });
    let second = Indexer::new(store.clone(), slow.clone());
    second.start();
    let until = Instant::now() + Duration::from_secs(10);
    while slow.calls.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    second.cancel();
    let status = wait(&second);
    assert_eq!(status["phase"], "cancelled");
    assert_eq!(status["indexed"], 7);
    assert_eq!(status["metadata_total"], 7);
    assert_eq!(status["metadata_done"], 7);
    assert_eq!(status["content_done"], 7);
    assert_eq!(store.counts()?["files"], 7);
    assert_eq!(store.counts()?["chunks"], 7);
    let search = Search::new(
        store.clone(),
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: false,
        }),
    );
    assert_eq!(
        search.search("서로 다른 프로젝트", 10, None, "keyword")?["results"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    assert!(
        search.search("기존", 10, None, "keyword")?["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[test]
fn legacy_qwen_settings_are_disabled_without_deleting_index() -> Result<()> {
    let dir = scratch()?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let docs = dir.path().join("documents");
    std::fs::create_dir(&docs)?;
    let path = docs.join("기존계약.txt");
    std::fs::write(&path, "보증금 반환은 14일 이내입니다.")?;
    let root = store.add_root(&docs)?;
    store.replace_files(&[moasearch::store::FileRecord {
        root_id: root["id"].as_i64().unwrap(),
        path,
        size: 42,
        modified: 1,
        passages: vec![moasearch::extract::Passage {
            text: "보증금 반환은 14일 이내입니다.".into(),
            location: "본문".into(),
        }],
        vectors: Some(vec![vec![1.0, 0.0, 0.0]]),
        model_id: Some("legacy-qwen-id".into()),
        error: None,
        reader_id: "legacy-v1".into(),
        content_key: None,
    }])?;
    let db = store.connect()?;
    db.execute(
        "INSERT INTO settings(key,value) VALUES ('embedding_model',?1)",
        [json!("C:\\old\\Qwen3-Embedding.gguf").to_string()],
    )?;
    db.execute(
        "INSERT INTO settings(key,value) VALUES ('chat_model',?1)",
        [json!("C:\\old\\Qwen3-chat.gguf").to_string()],
    )?;
    let engine = EmbeddedEngine::new(store.clone());
    assert_eq!(store.settings()?["embedding_model"], "");
    assert_eq!(store.settings()?["chat_model"], "");
    assert!(engine.model_id().is_none());
    assert_eq!(store.counts()?["files"], 1);
    assert_eq!(store.counts()?["vectors"], 1);
    let search = Search::new(store, Arc::new(engine));
    assert_eq!(
        search.search("보증금", 5, None, "keyword")?["results"][0]["name"],
        "기존계약.txt"
    );
    Ok(())
}

#[test]
fn app_services_execute_typed_commands_and_shutdown_cleanly() -> Result<()> {
    use moasearch::commands::Command;
    let dir = scratch()?;
    let documents = dir.path().join("문서");
    std::fs::create_dir_all(documents.join("선택"))?;
    std::fs::write(
        documents.join("선택").join("계약.txt"),
        "계약검토 보증금 반환은 14일 이내입니다.",
    )?;
    std::fs::write(documents.join("밖.txt"), "계약검토 형제 폴더 제외")?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let engine: Arc<dyn Embedder> = Arc::new(MockEngine {
        calls: AtomicUsize::new(0),
        delay: false,
    });
    let mut services =
        AppServices::new(store.clone(), Arc::new(EmbeddedEngine::new(store.clone())));
    Arc::get_mut(&mut services).unwrap().search = Arc::new(Search::new(store.clone(), engine));

    let root = services.execute(Command::AddRoot {
        path: documents.clone(),
    })?;
    assert_eq!(
        root["path"],
        serde_json::to_value(&moasearch::config::canonical(&documents)?)?
    );
    assert_eq!(services.execute(Command::StartIndex {})?["started"], true);
    assert_eq!(wait(&services.indexer)["errors"], 0);

    let status = services.execute(Command::Status {})?;
    assert_eq!(status["counts"]["files"], 2);
    assert!(status.get("model_download").is_some());
    assert!(
        services.execute(Command::Scope {
            path: documents.join("선택")
        })?["indexed"]
            == true
    );
    let search = services.execute(Command::Search(SearchInput {
        query: "계약검토".into(),
        limit: 30,
        extension: None,
        mode: "keyword".into(),
        folder: Some(documents.join("선택")),
    }))?;
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert_eq!(search["results"][0]["name"], "계약.txt");
    let question = SearchInput {
        query: "계약검토".into(),
        limit: 30,
        extension: None,
        mode: "keyword".into(),
        folder: Some(documents.join("선택")),
    };
    assert_eq!(store.settings()?["chat_enabled"], false);
    assert!(services.execute(Command::Answer(question.clone())).is_err());
    assert!(
        services
            .execute(Command::DownloadModel {
                kind: "chat".into()
            })
            .is_err()
    );
    store.update_settings(&json!({"chat_enabled":true}))?;
    let answer = services.execute(Command::Answer(question))?;
    assert_eq!(answer["sources"].as_array().unwrap().len(), 1);
    assert!(
        services
            .execute(Command::Search(SearchInput {
                query: " ".into(),
                limit: 30,
                extension: None,
                mode: "keyword".into(),
                folder: None,
            }))
            .is_err()
    );
    assert!(
        services
            .execute(Command::Scope {
                path: dir.path().join("missing")
            })
            .is_err()
    );
    assert!(
        services
            .execute(Command::UpdateSettings {
                settings: json!({"unknown":"value"})
            })
            .is_err()
    );

    assert_eq!(services.execute(Command::Quit {})?["ok"], true);
    assert!(
        !services.indexer.start(),
        "indexing cannot restart after shutdown begins"
    );
    assert!(services.execute(Command::Status {}).is_err());
    services.wait_shutdown()?;
    Ok(())
}

#[test]
fn app_services_preserve_root_mutation_and_model_download_guards() -> Result<()> {
    use moasearch::{commands::Command, models, service::AppServices};
    let dir = scratch()?;
    let documents = dir.path().join("documents");
    std::fs::create_dir_all(&documents)?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let services = AppServices::new(store.clone(), Arc::new(EmbeddedEngine::new(store)));
    let root = services.execute(Command::AddRoot {
        path: documents.clone(),
    })?;
    let root_id = root["id"].as_i64().unwrap();
    assert!(
        services
            .execute(Command::DownloadModel {
                kind: "unknown".into()
            })
            .is_err()
    );
    assert_eq!(
        services.execute(Command::RemoveRoot { id: root_id })?["ok"],
        true
    );
    assert!(services.execute(Command::Scope { path: documents }).is_ok());
    let cancel = || true;
    assert!(
        models::download_model_cancellable(
            "embedding",
            &dir.path().join("models"),
            &|_, _| {},
            &cancel
        )
        .is_err()
    );
    assert!(
        !dir.path().join("models").exists(),
        "cancel before transfer creates no partial model"
    );
    Ok(())
}
