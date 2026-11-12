//! Native Windows windows that render the local MoaSearch UI in WebView2.

use anyhow::Result;
use std::{path::PathBuf, sync::Arc};

use moasearch::{engine::EmbeddedEngine, store::Store};

#[cfg(windows)]
mod windows_desktop {
    use super::{EmbeddedEngine, PathBuf, Result, Store};
    use crate::launcher::folder_url;
    use anyhow::{Context, anyhow};
    use moasearch::{
        app::{self, AppState},
        search::{canonical_folder, folder_contains_path},
    };
    use std::sync::Arc;
    use std::{
        collections::HashMap,
        net::Ipv4Addr,
        path::Path,
        thread::{self, JoinHandle},
        time::Duration,
    };
    use tao::{
        dpi::LogicalSize,
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
        platform::run_return::EventLoopExtRunReturn,
        window::{Icon, Window, WindowBuilder, WindowId},
    };
    use tokio::{net::TcpListener, runtime::Runtime};
    use wry::{
        NewWindowResponse, PermissionResponse, WebContext, WebView, WebViewBuilder,
        WebViewBuilderExtWindows,
    };

    enum UserEvent {
        Open(Option<PathBuf>),
        ServerStopped,
    }

    struct DesktopWindow {
        window: Window,
        _webview: WebView,
        folder: Option<PathBuf>,
    }

    pub(super) fn run(
        store: Arc<Store>,
        engine: Arc<EmbeddedEngine>,
        folder: Option<PathBuf>,
        port: u16,
    ) -> Result<()> {
        anyhow::ensure!(port >= 1024, "포트는 1024 이상이어야 합니다.");
        let folder = folder.map(|path| canonical_folder(&path)).transpose()?;

        let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        let state = AppState::new(store.clone(), engine);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("로컬 앱 서버를 초기화하지 못했습니다.")?;
        let listener = runtime
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, port)))
            .with_context(|| format!("로컬 앱 서버 포트 {port}를 열지 못했습니다."))?;

        state.indexer.start_scheduler();
        let server = start_server(runtime, listener, state.clone(), proxy.clone());

        let mut web_context = WebContext::new(Some(store.data_dir.join("webview2")));
        let mut windows = HashMap::<WindowId, DesktopWindow>::new();
        match create_window(&event_loop, &mut web_context, port, folder.clone()) {
            Ok(window) => {
                windows.insert(window.window.id(), window);
            }
            Err(error) => {
                stop_server(&state, server)?;
                return Err(error);
            }
        }

        let event_proxy = proxy.clone();
        state.set_desktop_handler(Arc::new(move |folder| {
            event_proxy
                .send_event(UserEvent::Open(folder))
                .map_err(|_| anyhow!("모아검색 데스크톱 창이 이미 종료되었습니다."))
        }));

        let mut loop_error = None;
        event_loop.run_return(|event, target, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    window_id,
                    ..
                } => {
                    windows.remove(&window_id);
                    if windows.is_empty() {
                        state.indexer.close();
                        state.shutdown.notify_one();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                Event::UserEvent(UserEvent::Open(folder)) => {
                    match folder.map(|path| canonical_folder(&path)).transpose() {
                        Err(error) => {
                            eprintln!("요청한 폴더를 열 수 없습니다: {error:#}");
                            focus_window(&windows, None);
                        }
                        Ok(folder) => {
                            let focused = if let Some(folder) = folder.as_deref() {
                                focus_matching_folder(&mut windows, folder, port).unwrap_or_else(
                                    |error| {
                                        loop_error = Some(error);
                                        false
                                    },
                                )
                            } else {
                                focus_window(&windows, None)
                            };
                            if loop_error.is_some() {
                                state.indexer.close();
                                state.shutdown.notify_one();
                                for (_, window) in windows.drain() {
                                    window.window.set_visible(false);
                                }
                                *control_flow = ControlFlow::Exit;
                            } else if !focused {
                                match create_window(target, &mut web_context, port, folder) {
                                    Ok(window) => {
                                        windows.insert(window.window.id(), window);
                                    }
                                    Err(error) => {
                                        loop_error = Some(error);
                                        state.indexer.close();
                                        state.shutdown.notify_one();
                                        for (_, window) in windows.drain() {
                                            window.window.set_visible(false);
                                        }
                                        *control_flow = ControlFlow::Exit;
                                    }
                                }
                            }
                        }
                    }
                }
                Event::UserEvent(UserEvent::ServerStopped) => {
                    for (_, window) in windows.drain() {
                        window.window.set_visible(false);
                    }
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });

        windows.clear();
        state.indexer.close();
        state.shutdown.notify_one();
        let server_result = server
            .join()
            .map_err(|_| anyhow!("로컬 앱 서버 thread가 비정상 종료되었습니다."))?;
        server_result?;
        if let Some(error) = loop_error {
            return Err(error);
        }
        Ok(())
    }

    fn start_server(
        runtime: Runtime,
        listener: TcpListener,
        state: Arc<AppState>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> JoinHandle<Result<()>> {
        thread::spawn(move || {
            let shutdown_state = state.clone();
            let result = runtime.block_on(async move {
                axum::serve(listener, app::router(state))
                    .with_graceful_shutdown(async move {
                        shutdown_state.shutdown.notified().await;
                        shutdown_state.indexer.close();
                        while shutdown_state.indexer.status()["running"] == true {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                    .await
                    .context("로컬 앱 서버가 종료되었습니다.")
            });
            let _ = proxy.send_event(UserEvent::ServerStopped);
            result
        })
    }

    fn stop_server(state: &Arc<AppState>, server: JoinHandle<Result<()>>) -> Result<()> {
        state.indexer.close();
        state.shutdown.notify_one();
        server
            .join()
            .map_err(|_| anyhow!("로컬 앱 서버 thread가 비정상 종료되었습니다."))??;
        Ok(())
    }

    fn focus_window(windows: &HashMap<WindowId, DesktopWindow>, folder: Option<&Path>) -> bool {
        let found = windows
            .values()
            .find(|entry| same_folder(entry.folder.as_deref(), folder))
            .or_else(|| {
                folder.is_none().then(|| {
                    windows
                        .values()
                        .find(|entry| entry.folder.is_none())
                        .or_else(|| windows.values().next())
                })?
            });
        let Some(entry) = found else { return false };
        entry.window.set_visible(true);
        entry.window.set_minimized(false);
        entry.window.set_focus();
        true
    }

    fn focus_matching_folder(
        windows: &mut HashMap<WindowId, DesktopWindow>,
        folder: &Path,
        port: u16,
    ) -> Result<bool> {
        let Some(entry) = windows
            .values_mut()
            .find(|entry| same_folder(entry.folder.as_deref(), Some(folder)))
        else {
            return Ok(false);
        };
        entry._webview.load_url(&folder_url(port, folder)?)?;
        entry.window.set_visible(true);
        entry.window.set_minimized(false);
        entry.window.set_focus();
        Ok(true)
    }

    fn same_folder(left: Option<&Path>, right: Option<&Path>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                folder_contains_path(left, right) && folder_contains_path(right, left)
            }
            _ => false,
        }
    }

    fn create_window(
        target: &EventLoopWindowTarget<UserEvent>,
        context: &mut WebContext,
        port: u16,
        folder: Option<PathBuf>,
    ) -> Result<DesktopWindow> {
        let base_url = format!("http://127.0.0.1:{port}");
        let url = match folder.as_deref() {
            Some(folder) => folder_url(port, folder)?,
            None => base_url.clone(),
        };
        let icon = app_icon()?;
        let window = WindowBuilder::new()
            .with_title("모아검색")
            .with_inner_size(LogicalSize::new(1180.0, 800.0))
            .with_min_inner_size(LogicalSize::new(900.0, 650.0))
            .with_window_icon(Some(icon))
            .build(target)
            .context("모아검색 데스크톱 창을 만들지 못했습니다.")?;

        let expected_origin = reqwest::Url::parse(&base_url)?;
        let builder = WebViewBuilder::new_with_web_context(context)
            .with_url(&url)
            .with_devtools(false)
            .with_navigation_handler(move |requested| is_app_origin(&expected_origin, &requested))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .with_permission_handler(|_| PermissionResponse::Deny)
            .with_default_context_menus(false);
        let webview = builder.build(&window).map_err(|error| {
            anyhow!("WebView2 Runtime을 시작하지 못했습니다. Microsoft Edge WebView2 Runtime을 설치하거나 복구한 뒤 다시 실행하세요: {error}")
        })?;

        Ok(DesktopWindow {
            window,
            _webview: webview,
            folder,
        })
    }

    fn is_app_origin(expected: &reqwest::Url, requested: &str) -> bool {
        let Ok(requested) = reqwest::Url::parse(requested) else {
            return false;
        };
        requested.scheme() == expected.scheme()
            && requested.host_str() == expected.host_str()
            && requested.port_or_known_default() == expected.port_or_known_default()
            && requested.username().is_empty()
            && requested.password().is_none()
    }

    fn app_icon() -> Result<Icon> {
        let image = image::load_from_memory(include_bytes!("../icon.png"))?.into_rgba8();
        let (width, height) = image.dimensions();
        Icon::from_rgba(image.into_raw(), width, height)
            .map_err(|error| anyhow!("모아검색 창 아이콘을 읽지 못했습니다: {error}"))
    }
}

#[cfg(windows)]
pub fn run(
    store: Arc<Store>,
    engine: Arc<EmbeddedEngine>,
    folder: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    windows_desktop::run(store, engine, folder, port)
}
