//! Loopback UI/API. Heavy disk/model operations never run on async executor threads.
use crate::{
    config, engine::EmbeddedEngine, indexer::Indexer, models, ocr, search::Search, store::Store,
};
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type DesktopHandler = Arc<dyn Fn(Option<PathBuf>) -> Result<()> + Send + Sync>;

pub struct AppState {
    pub store: Arc<Store>,
    pub engine: Arc<EmbeddedEngine>,
    pub indexer: Arc<Indexer>,
    pub search: Arc<Search>,
    pub token: String,
    pub shutdown: tokio::sync::Notify,
    download: Mutex<Value>,
    desktop: Mutex<Option<DesktopHandler>>,
}
type ApiResult = std::result::Result<Json<Value>, ApiError>;
pub struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":self.0.to_string()})),
        )
            .into_response()
    }
}
impl AppState {
    pub fn new(store: Arc<Store>, engine: Arc<EmbeddedEngine>) -> Arc<Self> {
        let indexer = Indexer::new(store.clone(), engine.clone());
        Arc::new(Self {
            search: Arc::new(Search::new(store.clone(), engine.clone())),
            store,
            engine,
            indexer,
            token: (0..32)
                .map(|_| format!("{:02x}", rand::random::<u8>()))
                .collect(),
            shutdown: tokio::sync::Notify::new(),
            desktop: Mutex::new(None),
            download: Mutex::new(
                json!({"running":false,"kind":"","received":0,"total":0,"error":"","message":""}),
            ),
        })
    }
    pub fn set_desktop_handler(&self, handler: DesktopHandler) {
        *self.desktop.lock().unwrap() = Some(handler);
    }
}
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/static/app.js", get(js))
        .route("/static/style.css", get(css))
        .route("/static/icon.png", get(icon))
        .route("/api/status", get(status))
        .route("/api/instance", get(instance))
        .route("/api/window", post(open_window))
        .route("/api/scope", post(scope))
        .route("/api/quit", post(quit))
        .route("/api/roots", post(add_root))
        .route("/api/roots/{id}", delete(remove_root))
        .route("/api/index", post(start_index))
        .route("/api/index/cancel", post(cancel_index))
        .route("/api/settings", patch(settings))
        .route("/api/search", post(search))
        .route("/api/answer", post(answer))
        .route("/api/models/download", post(download))
        .route("/api/files/{id}", get(file))
        .route("/api/files/{id}/open", post(open_file))
        .route("/api/files/{id}/reveal", post(reveal))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), local_only))
        .with_state(state)
}
async fn blocking<F>(job: F) -> ApiResult
where
    F: FnOnce() -> Result<Value> + Send + 'static,
{
    let value = tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| ApiError(anyhow::anyhow!(e)))??;
    Ok(Json(value))
}
async fn local_only(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let hostname = host.split(':').next().unwrap_or("");
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok());
    let bad = !["localhost", "127.0.0.1"].contains(&hostname)
        || origin.is_some_and(|o| o != format!("http://{host}"))
        || (request.uri().path().starts_with("/api/")
            && request
                .headers()
                .get("x-moasearch-token")
                .and_then(|h| h.to_str().ok())
                != Some(&state.token));
    if bad {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"앱 페이지를 새로 고친 뒤 로컬 주소에서 다시 시도하세요."})),
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_static(value),
        );
    }
    response
}
async fn home(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(include_str!("static/index.html").replace("__TOKEN__", &state.token))
}
async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/app.js"),
    )
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("static/style.css"),
    )
}
async fn icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../icon.png").as_slice(),
    )
}
async fn status(State(s): State<Arc<AppState>>) -> ApiResult {
    blocking(move||Ok(json!({"counts":s.store.counts()?,"roots":s.store.roots()?,"settings":s.store.settings()?,
        "index":s.indexer.status(),"engine":s.engine.status(),"ocr":ocr::status(&s.store.data_dir.join("models")),"suggestions":suggestions(),"model_download":s.download.lock().unwrap().clone()}))).await
}
async fn instance(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"app":"moasearch", "version":env!("CARGO_PKG_VERSION"),
        "folder_protocol":1, "desktop":s.desktop.lock().unwrap().is_some(),
        "data_dir":s.store.data_dir, "pid":std::process::id()}),
    )
}
#[derive(Deserialize)]
struct WindowInput {
    folder: Option<String>,
}
async fn open_window(State(s): State<Arc<AppState>>, Json(body): Json<WindowInput>) -> ApiResult {
    blocking(move || {
        let folder = body
            .folder
            .map(|path| crate::search::canonical_folder(Path::new(&path)))
            .transpose()?;
        let handler = s.desktop.lock().unwrap().clone().ok_or_else(|| {
            anyhow::anyhow!(
                "개발 서버에는 앱 창이 없습니다. 서버를 종료한 뒤 모아검색을 실행하세요."
            )
        })?;
        handler(folder)?;
        Ok(json!({"ok":true}))
    })
    .await
}
async fn quit(State(s): State<Arc<AppState>>) -> Json<Value> {
    s.shutdown.notify_one();
    Json(json!({"ok":true}))
}

async fn scope(State(s): State<Arc<AppState>>, Json(body): Json<RootInput>) -> ApiResult {
    blocking(move || {
        let folder = crate::search::canonical_folder(Path::new(&body.path))?;
        let roots = s.store.roots()?;
        let covering = roots
            .iter()
            .filter_map(|root| root["path"].as_str())
            .find(|root| crate::search::folder_contains_path(Path::new(root), &folder));
        let registered = roots
            .iter()
            .filter_map(|root| root["path"].as_str())
            .any(|root| {
                config::within(&folder, Path::new(root)) && config::within(Path::new(root), &folder)
            });
        Ok(
            json!({"path":folder,"indexed":s.store.has_files_in(&folder)?,
            "registered":registered,"covered":covering.is_some(),"root_path":covering}),
        )
    })
    .await
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
#[derive(Deserialize)]
struct RootInput {
    path: String,
}
async fn add_root(State(s): State<Arc<AppState>>, Json(body): Json<RootInput>) -> ApiResult {
    blocking(move || {
        let _guard = s.indexer.mutation.lock().unwrap();
        s.indexer.ensure_idle()?;
        s.store.add_root(Path::new(&body.path))
    })
    .await
}
async fn remove_root(State(s): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult {
    blocking(move || {
        let _guard = s.indexer.mutation.lock().unwrap();
        s.indexer.ensure_idle()?;
        s.store.remove_root(id)?;
        Ok(json!({"ok":true}))
    })
    .await
}
async fn start_index(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"started":s.indexer.start()}))
}
async fn cancel_index(State(s): State<Arc<AppState>>) -> Json<Value> {
    s.indexer.cancel();
    Json(json!({"ok":true}))
}
async fn settings(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    blocking(move || {
        let _guard = s.indexer.mutation.lock().unwrap();
        s.indexer.ensure_idle()?;
        anyhow::ensure!(
            !s.download.lock().unwrap()["running"]
                .as_bool()
                .unwrap_or(false),
            "모델 다운로드가 끝난 뒤 설정을 저장하세요."
        );
        s.store.update_settings(&body)?;
        s.engine.reset();
        s.store.settings()
    })
    .await
}
#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    extension: Option<String>,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    folder: Option<String>,
}
fn default_limit() -> usize {
    30
}
fn default_mode() -> String {
    "hybrid".into()
}
impl SearchInput {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.query.trim().is_empty() && self.query.chars().count() <= 2000,
            "검색어는 1~2000자여야 합니다."
        );
        anyhow::ensure!(
            (1..=100).contains(&self.limit),
            "검색 결과 수는 1~100개여야 합니다."
        );
        anyhow::ensure!(
            ["hybrid", "keyword"].contains(&self.mode.as_str()),
            "알 수 없는 검색 방식입니다."
        );
        anyhow::ensure!(
            self.extension.as_ref().is_none_or(|s| s.len() <= 30),
            "확장자가 너무 깁니다."
        );
        Ok(())
    }
}
async fn search(State(s): State<Arc<AppState>>, Json(body): Json<SearchInput>) -> ApiResult {
    blocking(move || {
        body.validate()?;
        s.search.search_in(
            &body.query,
            body.limit,
            body.extension.as_deref(),
            &body.mode,
            body.folder.as_deref().map(Path::new),
        )
    })
    .await
}
async fn answer(State(s): State<Arc<AppState>>, Json(body): Json<SearchInput>) -> ApiResult {
    blocking(move || {
        body.validate()?;
        s.search.answer_in(
            &body.query,
            body.limit.min(8),
            body.folder.as_deref().map(Path::new),
        )
    })
    .await
}
async fn file(State(s): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult {
    blocking(move || {
        s.store
            .file(id)?
            .ok_or_else(|| anyhow::anyhow!("색인된 파일을 찾을 수 없습니다."))
    })
    .await
}
#[derive(Deserialize)]
struct DownloadInput {
    kind: String,
}
async fn download(State(s): State<Arc<AppState>>, Json(body): Json<DownloadInput>) -> ApiResult {
    if !["embedding", "chat", "ocr"].contains(&body.kind.as_str()) {
        return Err(anyhow::anyhow!("알 수 없는 모델입니다.").into());
    }
    {
        let mut download = s.download.lock().unwrap();
        if download["running"] == true {
            return Err(anyhow::anyhow!("모델을 이미 다운로드 중입니다.").into());
        }
        *download = json!({"running":true,"kind":body.kind,"received":0,"total":0,"error":"","message":"로컬 모델 준비 중"});
    }
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let model = models::download_model(
                &body.kind,
                &s.store.data_dir.join("models"),
                &|received, total| {
                    let mut state = s.download.lock().unwrap();
                    state["received"] = json!(received);
                    state["total"] = json!(total);
                },
            )?;
            loop {
                {
                    let _guard = s.indexer.mutation.lock().unwrap();
                    if s.indexer.ensure_idle().is_ok() {
                        if body.kind != "ocr" {
                            s.store
                                .update_settings(&json!({format!("{}_model",body.kind):model}))?;
                            s.engine.reset();
                        }
                        break;
                    }
                }
                s.download.lock().unwrap()["message"] =
                    json!("다운로드 완료. 현재 색인 종료 후 적용합니다.");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Ok(())
        })();
        let mut download = s.download.lock().unwrap();
        download["running"] = json!(false);
        match result {
            Ok(()) => {
                download["message"] = json!(if body.kind == "ocr" {
                    "OCR 모델 준비 완료. 다시 색인하면 이미지와 스캔 PDF 본문을 읽습니다."
                } else {
                    "모델 준비 완료. 다시 색인하면 Google 모델로 검색합니다."
                })
            }
            Err(e) => download["error"] = json!(e.to_string()),
        };
    });
    Ok(Json(json!({"started":true})))
}
fn action_path(s: &AppState, id: i64) -> Result<PathBuf> {
    let file = s
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
        s.store
            .roots()?
            .iter()
            .any(|r| config::within(&resolved, Path::new(r["path"].as_str().unwrap_or("")))),
        "파일이 검색 범위 밖입니다."
    );
    Ok(resolved)
}
async fn reveal(State(s): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult {
    blocking(move || {
        let path = action_path(&s, id)?;
        std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(path)
            .spawn()?;
        Ok(json!({"ok":true}))
    })
    .await
}
async fn open_file(State(s): State<Arc<AppState>>, RoutePath(id): RoutePath<i64>) -> ApiResult {
    blocking(move || {
        let path = action_path(&s, id)?;
        let extension = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        anyhow::ensure!(
            [
                "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "hwp", "hwpx", "txt", "md",
                "csv", "png", "jpg", "jpeg", "gif", "webp", "mp3", "wav", "mp4", "mkv", "bmp",
                "tif", "tiff"
            ]
            .contains(&extension.as_str()),
            "이 형식은 폴더에서 확인하세요. 실행 파일과 스크립트는 실행하지 않습니다."
        );
        shell_open(&path.to_string_lossy())?;
        Ok(json!({"ok":true}))
    })
    .await
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
