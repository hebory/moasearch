use anyhow::Result;
use moasearch::{
    app::{self, AppState},
    engine::{EmbeddedEngine, Embedder},
    indexer::Indexer,
    search::Search,
    store::Store,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
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
    assert_eq!(first["indexed"], 96);
    assert_eq!(store.counts()?["vectors"], 96);
    let calls = engine.calls.load(Ordering::SeqCst);
    index.start();
    let second = wait(&index);
    assert_eq!(second["skipped"], 96);
    assert_eq!(engine.calls.load(Ordering::SeqCst), calls);
    std::fs::write(
        docs.join("계약_0.txt"),
        "수정 계약번호 0 보증금은 30일 이내 반환합니다.",
    )?;
    std::fs::remove_file(docs.join("계약_1.txt"))?;
    index.start();
    let third = wait(&index);
    assert_eq!(third["errors"], 0);
    assert_eq!(third["indexed"], 1);
    assert_eq!(store.counts()?["files"], 95);
    let search = Search::new(store, engine);
    let result = search.search("30일", 5, None, "keyword")?;
    assert_eq!(result["results"][0]["name"], "계약_0.txt");
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
            "새로운 프로젝트의 수정 일정과 변경 사항",
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

#[tokio::test]
async fn local_api_authentication_and_validation() -> Result<()> {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let dir = scratch()?;
    let store = Arc::new(Store::open(dir.path())?);
    let state = AppState::new(store.clone(), Arc::new(EmbeddedEngine::new(store)));
    let app = app::router(state.clone());
    let get = |host: &str, path: &str, token: Option<&str>| {
        let mut b = Request::builder().uri(path).header("host", host);
        if let Some(t) = token {
            b = b.header("x-moasearch-token", t);
        }
        b.body(Body::empty()).unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(get("evil.example", "/", None))
            .await?
            .status(),
        403
    );
    assert_eq!(
        app.clone()
            .oneshot(get("127.0.0.1:8765", "/api/status", None))
            .await?
            .status(),
        403
    );
    let page = app
        .clone()
        .oneshot(get("127.0.0.1:8765", "/", None))
        .await?;
    assert_eq!(page.status(), 200);
    assert!(page.headers().contains_key("content-security-policy"));
    let content = page.into_body().collect().await?.to_bytes();
    assert!(std::str::from_utf8(&content)?.contains(&state.token));
    let body = json!({"query":"","limit":500,"mode":"hybrid"}).to_string();
    let bad = Request::builder()
        .uri("/api/search")
        .method("POST")
        .header("host", "127.0.0.1:8765")
        .header("x-moasearch-token", &state.token)
        .header("content-type", "application/json")
        .body(Body::from(body))?;
    assert_eq!(app.clone().oneshot(bad).await?.status(), 400);
    let foreign = Request::builder()
        .uri("/api/status")
        .header("host", "127.0.0.1:8765")
        .header("x-moasearch-token", &state.token)
        .header("origin", "https://evil.example")
        .body(Body::empty())?;
    assert_eq!(app.oneshot(foreign).await?.status(), 403);
    Ok(())
}
