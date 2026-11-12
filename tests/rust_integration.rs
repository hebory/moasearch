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
    assert_eq!(app.clone().oneshot(foreign).await?.status(), 403);
    let quit = |authorized| {
        let mut request = Request::builder()
            .uri("/api/quit")
            .method("POST")
            .header("host", "127.0.0.1:8765");
        if authorized {
            request = request.header("x-moasearch-token", &state.token);
        }
        request.body(Body::empty()).unwrap()
    };
    assert_eq!(app.clone().oneshot(quit(false)).await?.status(), 403);
    assert_eq!(app.oneshot(quit(true)).await?.status(), 200);
    tokio::time::timeout(Duration::from_secs(1), state.shutdown.notified()).await?;
    Ok(())
}

#[tokio::test]
async fn desktop_window_api_requires_auth_and_forwards_validated_folder() -> Result<()> {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    let dir = scratch()?;
    let folder = dir.path().join("선택 %_ # & 폴더");
    std::fs::create_dir_all(&folder)?;
    let canonical_folder = moasearch::config::canonical(&folder)?;
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    let state = AppState::new(store.clone(), Arc::new(EmbeddedEngine::new(store)));
    let app = app::router(state.clone());
    let get_instance = || {
        Request::builder()
            .uri("/api/instance")
            .header("host", "127.0.0.1:8765")
            .header("x-moasearch-token", &state.token)
            .body(Body::empty())
            .unwrap()
    };
    let post_window = |body: Value, authorized: bool| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/window")
            .header("host", "127.0.0.1:8765")
            .header("content-type", "application/json");
        if authorized {
            request = request.header("x-moasearch-token", &state.token);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };

    let instance = app
        .clone()
        .oneshot(get_instance())
        .await?
        .into_body()
        .collect()
        .await?
        .to_bytes();
    let instance: Value = serde_json::from_slice(&instance)?;
    assert_eq!(instance["desktop"], false);

    let response = app
        .clone()
        .oneshot(post_window(json!({"folder":null}), false))
        .await?;
    assert_eq!(response.status(), 403);
    let response = app
        .clone()
        .oneshot(post_window(json!({"folder":canonical_folder}), true))
        .await?;
    assert_eq!(
        response.status(),
        400,
        "a headless server has no window callback"
    );

    let calls = Arc::new(std::sync::Mutex::new(Vec::<Option<PathBuf>>::new()));
    let recorded = calls.clone();
    state.set_desktop_handler(Arc::new(move |folder| {
        recorded.lock().unwrap().push(folder);
        Ok(())
    }));

    let instance = app
        .clone()
        .oneshot(get_instance())
        .await?
        .into_body()
        .collect()
        .await?
        .to_bytes();
    let instance: Value = serde_json::from_slice(&instance)?;
    assert_eq!(instance["desktop"], true);

    let response = app
        .clone()
        .oneshot(post_window(json!({"folder":null}), true))
        .await?;
    assert_eq!(response.status(), 200);
    let response = app
        .clone()
        .oneshot(post_window(
            json!({"folder":canonical_folder.to_string_lossy()}),
            true,
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let invalid = folder.join("does-not-exist");
    let response = app
        .clone()
        .oneshot(post_window(
            json!({"folder":invalid.to_string_lossy()}),
            true,
        ))
        .await?;
    assert_eq!(response.status(), 400);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![None, Some(canonical_folder)],
        "invalid paths must be rejected before invoking the native window callback"
    );
    Ok(())
}

#[tokio::test]
async fn explorer_scope_api_limits_search_and_answer_without_mutating_roots() -> Result<()> {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let dir = scratch()?;
    let library = dir.path().join("문서");
    let scope = library.join("선택 %_ # & 폴더");
    let nested = scope.join("하위");
    let sibling = library.join("선택 %_ # & 폴더2");
    let empty = library.join("빈 폴더");
    for folder in [&nested, &sibling, &empty] {
        std::fs::create_dir_all(folder)?;
    }
    for path in [
        scope.join("현재.txt"),
        nested.join("안쪽.txt"),
        sibling.join("바깥.txt"),
    ] {
        std::fs::write(path, "계약검토 보증금 반환 내역")?;
    }
    let store = Arc::new(Store::open(dir.path().join("data"))?);
    store.add_root(&library)?;
    let index = Indexer::new(
        store.clone(),
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: false,
        }),
    );
    index.start();
    assert_eq!(wait(&index)["errors"], 0);
    let mut state = AppState::new(store.clone(), Arc::new(EmbeddedEngine::new(store.clone())));
    Arc::get_mut(&mut state).unwrap().search = Arc::new(Search::new(
        store.clone(),
        Arc::new(MockEngine {
            calls: AtomicUsize::new(0),
            delay: false,
        }),
    ));
    let app = app::router(state.clone());
    let post = |path: &str, body: Value, authorized: bool| {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", "127.0.0.1:8765")
            .header("content-type", "application/json");
        if authorized {
            request = request.header("x-moasearch-token", &state.token);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(post("/api/scope", json!({"path":scope}), false))
            .await?
            .status(),
        403
    );
    for (path, indexed, registered) in [
        (&scope, true, false),
        (&empty, false, false),
        (&library, true, true),
    ] {
        let response = app
            .clone()
            .oneshot(post("/api/scope", json!({"path":path}), true))
            .await?;
        assert_eq!(response.status(), 200);
        let data: Value =
            serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
        assert_eq!(data["indexed"], indexed);
        assert_eq!(data["registered"], registered);
        assert_eq!(data["covered"], true);
    }
    for endpoint in ["/api/search", "/api/answer"] {
        let response = app
            .clone()
            .oneshot(post(
                endpoint,
                json!({"query":"계약검토", "mode":"keyword", "folder":scope}),
                true,
            ))
            .await?;
        assert_eq!(response.status(), 200);
        let data: Value =
            serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
        let field = if endpoint.ends_with("answer") {
            "sources"
        } else {
            "results"
        };
        let results = data[field].as_array().unwrap();
        assert_eq!(results.len(), 2, "{data}");
        assert!(results.iter().all(|result| result["name"] != "바깥.txt"));
        for invalid in [
            String::new(),
            "   ".to_string(),
            scope.join("현재.txt").to_string_lossy().into_owned(),
            library.join("없는 폴더").to_string_lossy().into_owned(),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(post(
                        endpoint,
                        json!({"query":"계약검토", "folder":invalid}),
                        true
                    ))
                    .await?
                    .status(),
                400
            );
        }
    }
    let response = app
        .clone()
        .oneshot(post(
            "/api/search",
            json!({"query":"계약검토", "mode":"keyword"}),
            true,
        ))
        .await?;
    let data: Value = serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
    assert_eq!(data["results"].as_array().unwrap().len(), 3);
    assert_eq!(store.roots()?.len(), 1);
    let instance = Request::builder()
        .uri("/api/instance")
        .header("host", "127.0.0.1:8765")
        .header("x-moasearch-token", &state.token)
        .body(Body::empty())?;
    let response = app.oneshot(instance).await?;
    let data: Value = serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
    assert_eq!(data["folder_protocol"], 1);
    assert_eq!(data["data_dir"], serde_json::to_value(&store.data_dir)?);
    Ok(())
}
