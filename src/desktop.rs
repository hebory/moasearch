//! Windows host: bundled screen, in-process commands, and native window lifetime.
mod assets;
mod bridge;
mod worker;

use anyhow::{Context, Result, anyhow, ensure};
use moasearch::{
    commands::Command,
    engine::EmbeddedEngine,
    instance::{Instance, InstanceCommand, InstanceHandler},
    search::{canonical_folder, folder_contains_path},
    service::AppServices,
    store::Store,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    platform::run_return::EventLoopExtRunReturn,
    window::{Icon, Window, WindowBuilder, WindowId},
};
use wry::{
    NewWindowResponse, PermissionResponse, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows,
};

enum DesktopEvent {
    Open(Option<PathBuf>),
    Invoke(WindowId, bridge::Request),
    Response(WindowId, bridge::Response, bool),
    Quit,
}
struct AppWindow {
    // Drop the WebView before its native parent window.
    webview: WebView,
    window: Window,
    folder: Option<PathBuf>,
}

pub fn run(store: Arc<Store>, engine: Arc<EmbeddedEngine>, folder: Option<PathBuf>) -> Result<()> {
    let folder = folder.map(|path| canonical_folder(&path)).transpose()?;
    let mut event_loop = EventLoopBuilder::<DesktopEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let closing = Arc::new(AtomicBool::new(false));
    let gate = closing.clone();
    let handoff_proxy = proxy.clone();
    let handler: InstanceHandler = Arc::new(move |command| {
        ensure!(
            !gate.load(Ordering::Acquire),
            "모아검색이 종료 중입니다. 잠시 후 다시 실행하세요."
        );
        handoff_proxy
            .send_event(match command {
                InstanceCommand::Open(folder) => DesktopEvent::Open(folder),
                InstanceCommand::Quit => DesktopEvent::Quit,
            })
            .map_err(|_| anyhow!("모아검색 창이 이미 종료되었습니다."))
    });
    // Claim the data directory before opening its WebView profile or starting work.
    let instance = Instance::bind(&store.data_dir, handler)?;
    let services = AppServices::new(store.clone(), engine);
    let workers = worker::Workers::new(4, 64);
    let mut context = WebContext::new(Some(store.data_dir.join("webview2")));
    let first = create_window(&event_loop, &mut context, folder, proxy.clone())?;
    let mut windows = HashMap::from([(first.window.id(), first)]);
    services.indexer.start_scheduler();
    let mut failure = None;

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
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(DesktopEvent::Quit) => *control_flow = ControlFlow::Exit,
            Event::UserEvent(DesktopEvent::Open(folder)) => {
                let result = (|| -> Result<()> {
                    let folder = folder.map(|path| canonical_folder(&path)).transpose()?;
                    let found = windows
                        .values_mut()
                        .find(|entry| same_folder(entry.folder.as_deref(), folder.as_deref()));
                    if let Some(entry) = found {
                        // Restore the selected folder if the user previously cleared its scope.
                        if folder.is_some() {
                            entry
                                .webview
                                .load_url(&assets::window_url(folder.as_deref())?)?;
                        }
                        focus(&entry.window);
                    } else if folder.is_none() && !windows.is_empty() {
                        focus(&windows.values().next().unwrap().window);
                    } else {
                        let entry = create_window(target, &mut context, folder, proxy.clone())?;
                        windows.insert(entry.window.id(), entry);
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failure = Some(error);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(DesktopEvent::Invoke(id, request)) => {
                let Some(window) = windows.get(&id) else {
                    return;
                };
                let command = match request.command() {
                    Ok(command) => command,
                    Err(error) => {
                        send_response(&window.webview, request.response(Err(error)));
                        return;
                    }
                };
                if matches!(command, Command::Quit {}) {
                    send_response(
                        &window.webview,
                        request.response(Ok(serde_json::json!({"ok":true}))),
                    );
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                let boot = matches!(command, Command::Status {});
                let request = Arc::new(request);
                let background_request = request.clone();
                let services = services.clone();
                let responder = proxy.clone();
                let submitted = workers.submit(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        services.execute(command)
                    }))
                    .unwrap_or_else(|_| Err(anyhow!("앱 작업 처리 중 오류가 발생했습니다.")));
                    let ready = boot && result.is_ok();
                    let _ = responder.send_event(DesktopEvent::Response(
                        id,
                        background_request.response(result),
                        ready,
                    ));
                });
                if let Err(error) = submitted {
                    send_response(&window.webview, request.response(Err(error)));
                }
            }
            Event::UserEvent(DesktopEvent::Response(id, response, ready)) => {
                if let Some(window) = windows.get(&id) {
                    if let Ok(script) = response.script() {
                        if window.webview.evaluate_script(&script).is_ok() && ready {
                            // The bundled UI has invoked Rust and received its first status.
                            instance.mark_ready();
                        }
                    }
                }
            }
            _ => {}
        }
    });

    closing.store(true, Ordering::Release);
    windows.clear();
    services.begin_shutdown();
    drop(workers);
    services.wait_shutdown()?;
    drop(instance);
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

fn send_response(webview: &WebView, response: bridge::Response) {
    if let Ok(script) = response.script() {
        let _ = webview.evaluate_script(&script);
    }
}
fn focus(window: &Window) {
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
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
    target: &EventLoopWindowTarget<DesktopEvent>,
    context: &mut WebContext,
    folder: Option<PathBuf>,
    proxy: EventLoopProxy<DesktopEvent>,
) -> Result<AppWindow> {
    let image = image::load_from_memory(include_bytes!("../icon.png"))?.into_rgba8();
    let (width, height) = image.dimensions();
    let icon = Icon::from_rgba(image.into_raw(), width, height)?;
    let window = WindowBuilder::new()
        .with_title("모아검색")
        .with_inner_size(LogicalSize::new(1180.0, 800.0))
        .with_min_inner_size(LogicalSize::new(900.0, 650.0))
        .with_window_icon(Some(icon))
        .build(target)
        .context("모아검색 창을 만들지 못했습니다.")?;
    let id = window.id();
    let webview = WebViewBuilder::new_with_web_context(context)
        .with_https_scheme(true)
        .with_custom_protocol(assets::SCHEME.into(), |_, request| {
            let asset = if request.method() == wry::http::Method::GET && assets::is_app_url(&request.uri().to_string()) {
                assets::resource(request.uri().path())
            } else { None };
            let (status, mime, body) = match asset {
                Some((mime, body)) => (200, mime, body),
                None => (404, "text/plain; charset=utf-8", b"Not found".as_slice()),
            };
            wry::http::Response::builder().status(status)
                .header("Content-Type", mime).header("Content-Security-Policy", assets::CSP)
                .header("X-Content-Type-Options", "nosniff").header("Cache-Control", "no-store")
                .body(Cow::Borrowed(body)).unwrap()
        })
        .with_url(assets::window_url(folder.as_deref())?)
        .with_ipc_handler(move |request| {
            if !assets::is_document(&request.uri().to_string()) { return; }
            if let Ok(request) = bridge::Request::parse(request.body()) {
                let _ = proxy.send_event(DesktopEvent::Invoke(id, request));
            }
        })
        .with_navigation_handler(|url| assets::is_document(&url))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false)
        .with_permission_handler(|_| PermissionResponse::Deny)
        .with_default_context_menus(false).with_devtools(false)
        .build(&window).context("WebView2 화면을 시작하지 못했습니다. Microsoft WebView2 Runtime을 설치하거나 복구하세요.")?;
    Ok(AppWindow {
        webview,
        window,
        folder,
    })
}
