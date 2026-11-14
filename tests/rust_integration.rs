use anyhow::Result;
use moasearch::{
    commands::SearchInput,
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
}

impl Embedder for NamedEngine {
    fn model_id(&self) -> Option<String> {
        Some(self.id.clone())
    }
    fn embed(&self, texts: &[String], _: bool) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
    index.start();
    let second = wait(&index);
    assert_eq!(second["skipped"], 96);
    assert_eq!(second["phase"], "complete");
    assert_eq!(engine.calls.load(Ordering::SeqCst), calls);
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
    assert_eq!(recovered["indexed"], 1);
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
    });

    let backfill_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(backfill_indexer.start());
    let backfilled = wait(&backfill_indexer);
    assert_eq!(backfilled["phase"], "complete");
    assert_eq!(backfilled["skipped"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);

    std::fs::write(docs.join("legacy-copy.txt"), text)?;
    let restarted_indexer = Indexer::new(store.clone(), engine.clone());
    assert!(restarted_indexer.start());
    let reused = wait(&restarted_indexer);
    assert_eq!(reused["phase"], "complete");
    assert_eq!(reused["indexed"], 1);
    assert_eq!(reused["reused"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
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
    });
    let indexer = Indexer::new(store.clone(), engine.clone());

    assert!(indexer.start());
    let status = wait(&indexer);
    assert_eq!(status["phase"], "complete");
    assert_eq!(status["reused_chunks"], 1);
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);
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
    assert_eq!(context_status["indexed"], 2);
    assert_eq!(context_status["reused"], 0);
    assert_eq!(context_status["reused_chunks"], 1);
    assert_eq!(first_engine.calls.load(Ordering::SeqCst), 3);

    let second_engine = Arc::new(NamedEngine {
        id: "fixture-google-v2".into(),
        calls: AtomicUsize::new(0),
    });
    let model_indexer = Indexer::new(store, second_engine.clone());
    assert!(model_indexer.start());
    let model_status = wait(&model_indexer);
    assert_eq!(model_status["indexed"], 2);
    assert_eq!(model_status["reused"], 0);
    assert_eq!(model_status["reused_chunks"], 1);
    assert_eq!(second_engine.calls.load(Ordering::SeqCst), 1);
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
    assert_eq!(status["indexed"], 1);
    assert_eq!(store.counts()?["files"], 8);
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
    let answer = services.execute(Command::Answer(SearchInput {
        query: "계약검토".into(),
        limit: 30,
        extension: None,
        mode: "keyword".into(),
        folder: Some(documents.join("선택")),
    }))?;
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
