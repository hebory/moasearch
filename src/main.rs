#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::Result;
use clap::{Parser, Subcommand};
use moasearch::{
    app::{self, AppState},
    config,
    engine::EmbeddedEngine,
    indexer::Indexer,
    models, ocr,
    search::Search,
    store::Store,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
#[cfg(windows)]
mod desktop;
mod launcher;

#[derive(Parser)]
#[command(version, about = "모아검색 — Rust와 Google 내장 AI로 내 파일 검색")]
struct Cli {
    #[arg(long, global = true, hide = true)]
    no_dialog: bool,
    #[arg(long, env="MOASEARCH_DATA_DIR", default_value_os_t=config::data_directory())]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// 모아검색 Windows 앱 실행
    Desktop {
        #[arg(long,default_value_t=8765,value_parser=clap::value_parser!(u16).range(1024..))]
        port: u16,
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long, hide = true)]
        background: bool,
    },
    /// 탐색기에서 선택한 폴더와 하위 폴더 검색 화면 열기
    OpenFolder {
        #[arg(long)]
        path: PathBuf,
        #[arg(long,default_value_t=8765,value_parser=clap::value_parser!(u16).range(1024..))]
        port: u16,
        /// 창 없는 개발 서버 사용 (자동화 검증용)
        #[arg(long, hide = true)]
        no_browser: bool,
    },
    /// 창 없는 개발 서버 실행
    #[command(hide = true)]
    Serve {
        #[arg(long,default_value_t=8765,value_parser=clap::value_parser!(u16).range(1024..))]
        port: u16,
        #[arg(long, hide = true)]
        no_browser: bool,
    },
    /// 엔진·Google 모델·Rust OCR 상태 확인
    Doctor,
    /// 지정한 폴더 추가 후 증분 색인
    Index { paths: Vec<PathBuf> },
    /// 자연어 또는 키워드 검색
    Search {
        query: String,
        #[arg(long)]
        keyword: bool,
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// 검색된 문서를 근거로 Google Gemma 답변 생성
    Answer {
        query: String,
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// Google 검색·답변 모델과 OCR 모델 관리
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
}
#[derive(Subcommand)]
enum ModelCommand {
    List,
    Download {
        #[arg(value_parser=["embedding","chat","ocr"])]
        kind: String,
    },
}
fn emit(value: Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
fn run() -> Result<()> {
    let args = Cli::parse();
    let store = Arc::new(Store::open(&args.data_dir)?);
    if let Some(Command::OpenFolder {
        path,
        port,
        no_browser,
    }) = &args.command
    {
        emit(launcher::open_folder(
            &store.data_dir,
            path,
            *port,
            *no_browser,
        )?);
        return Ok(());
    }
    if let Some(Command::Models { command }) = &args.command {
        match command {
            ModelCommand::List => emit(models::catalog()),
            ModelCommand::Download { kind } => {
                let last = std::sync::Mutex::new(0u64);
                let path = models::download_model(
                    kind,
                    &store.data_dir.join("models"),
                    &|received, total| {
                        let mut previous = last.lock().unwrap();
                        if received.saturating_sub(*previous) >= 64 * 1024 * 1024
                            || received == total
                        {
                            eprintln!(
                                "다운로드 {} / {} MB",
                                received / 1024 / 1024,
                                total / 1024 / 1024
                            );
                            *previous = received;
                        }
                    },
                )?;
                if kind != "ocr" {
                    store.update_settings(&json!({format!("{kind}_model"):path}))?;
                }
                emit(json!({"model":path,"verified":true}));
            }
        }
        return Ok(());
    }
    let engine = Arc::new(EmbeddedEngine::new(store.clone()));
    match args.command.unwrap_or(Command::Desktop {
        port: 8765,
        folder: None,
        background: false,
    }) {
        Command::Desktop { port, folder, .. } => {
            let folder = folder
                .map(|path| moasearch::search::canonical_folder(&path))
                .transpose()?;
            if !launcher::focus_existing(&store.data_dir, folder.as_deref(), port)? {
                #[cfg(windows)]
                desktop::run(store, engine, folder, port)?;
                #[cfg(not(windows))]
                anyhow::bail!("모아검색 데스크톱 앱은 Windows에서 실행하세요.");
            }
        }
        Command::Doctor => emit(
            json!({"runtime":"Rust / llama.cpp (in-process)","data_dir":store.data_dir,"counts":store.counts()?,"settings":store.settings()?,"engine":engine.status(),"ocr":ocr::status(&store.data_dir.join("models"))}),
        ),
        Command::Search {
            query,
            keyword,
            folder,
        } => emit(Search::new(store, engine).search_in(
            &query,
            30,
            None,
            if keyword { "keyword" } else { "hybrid" },
            folder.as_deref(),
        )?),
        Command::Answer { query, folder } => {
            emit(Search::new(store, engine).answer_in(&query, 5, folder.as_deref())?)
        }
        Command::Index { paths } => {
            for path in paths {
                let resolved = config::canonical(&path)?;
                if !store
                    .roots()?
                    .iter()
                    .any(|r| r["path"].as_str() == resolved.to_str())
                {
                    store.add_root(&resolved)?;
                }
            }
            let indexer = Indexer::new(store, engine);
            let cancel = indexer.clone();
            ctrlc::set_handler(move || cancel.cancel())?;
            indexer.start();
            while indexer.status()["running"] == true {
                std::thread::sleep(Duration::from_millis(100));
            }
            let status = indexer.status();
            emit(status.clone());
            anyhow::ensure!(
                status["errors"].as_u64().unwrap_or(0) == 0,
                "일부 파일을 색인하지 못했습니다. 위 오류를 확인하세요."
            );
        }
        Command::Serve { port, .. } => {
            let state = AppState::new(store, engine);
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                eprintln!("모아검색 Rust: http://127.0.0.1:{port}");
                state.indexer.start_scheduler();
                let shutdown = state.indexer.clone();
                let requested_shutdown = state.clone();
                axum::serve(listener, app::router(state))
                    .with_graceful_shutdown(async move {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {},
                            _ = requested_shutdown.shutdown.notified() => {},
                        }
                        shutdown.close();
                        while shutdown.status()["running"] == true {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                    .await?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Command::Models { .. } | Command::OpenFolder { .. } => unreachable!(),
    }
    Ok(())
}
fn main() {
    #[cfg(windows)]
    {
        let arguments: Vec<_> = std::env::args_os().skip(1).collect();
        if !arguments.is_empty()
            && !arguments
                .iter()
                .any(|arg| arg == "desktop" || arg == "open-folder")
        {
            use windows::Win32::System::Console::{
                ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
            };
            // Preserve redirected CLI pipes; attach only when no output handle exists.
            if unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_or(true, |handle| handle.is_invalid())
            {
                let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
            }
        }
    }
    if let Err(error) = run() {
        eprintln!("오류: {error:#}");
        #[cfg(windows)]
        {
            let arguments: Vec<_> = std::env::args_os().skip(1).collect();
            let interactive = arguments.is_empty()
                || arguments
                    .iter()
                    .any(|arg| arg == "desktop" || arg == "open-folder");
            if interactive
                && !arguments.iter().any(|arg| {
                    arg == "--no-browser" || arg == "--background" || arg == "--no-dialog"
                })
            {
                use windows::{
                    Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
                    core::PCWSTR,
                };
                let message: Vec<_> = format!("모아검색을 실행하지 못했습니다.\n\n{error:#}")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let title: Vec<_> = "모아검색".encode_utf16().chain(Some(0)).collect();
                unsafe {
                    MessageBoxW(
                        None,
                        PCWSTR(message.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
        }
        std::process::exit(1);
    }
}
