#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::Result;
use clap::{Parser, Subcommand};
use moasearch::{
    commands::{Command as AppCommand, SearchInput},
    config,
    engine::EmbeddedEngine,
    models, ocr,
    service::AppServices,
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
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long, hide = true)]
        background: bool,
    },
    /// 탐색기에서 선택한 폴더와 하위 폴더 검색 화면 열기
    OpenFolder {
        #[arg(long)]
        path: PathBuf,
    },
    /// 실행 중인 데스크톱 앱 확인
    #[command(hide = true)]
    Instance,
    /// 실행 중인 앱과 색인 작업 종료
    Quit,
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
    if let Some(Command::OpenFolder { path }) = &args.command {
        emit(launcher::open_window(&store.data_dir, Some(path))?);
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
        folder: None,
        background: false,
    }) {
        Command::Desktop { folder, background } => {
            let folder = folder
                .map(|path| moasearch::search::canonical_folder(&path))
                .transpose()?;
            if background {
                #[cfg(windows)]
                desktop::run(store, engine, folder)?;
                #[cfg(not(windows))]
                anyhow::bail!("모아검색 데스크톱 앱은 Windows에서 실행하세요.");
            } else {
                emit(launcher::open_window(&store.data_dir, folder.as_deref())?);
            }
        }
        Command::Instance => emit(
            launcher::running_instance(&store.data_dir)?
                .unwrap_or_else(|| json!({"running":false})),
        ),
        Command::Quit => emit(launcher::quit(&store.data_dir)?),
        Command::Doctor => emit(
            json!({"runtime":"Rust / llama.cpp (in-process)","data_dir":store.data_dir,"counts":store.counts()?,"settings":store.settings()?,"engine":engine.status(),"ocr":ocr::status(&store.data_dir.join("models"))}),
        ),
        Command::Search {
            query,
            keyword,
            folder,
        } => emit(
            AppServices::new(store, engine).execute(AppCommand::Search(SearchInput {
                query,
                limit: 30,
                extension: None,
                mode: if keyword { "keyword" } else { "hybrid" }.into(),
                folder,
            }))?,
        ),
        Command::Answer { query, folder } => emit(AppServices::new(store, engine).execute(
            AppCommand::Answer(SearchInput {
                query,
                limit: 5,
                extension: None,
                mode: "hybrid".into(),
                folder,
            }),
        )?),
        Command::Index { paths } => {
            let services = AppServices::new(store.clone(), engine);
            for path in paths {
                let resolved = config::canonical(&path)?;
                if !store
                    .roots()?
                    .iter()
                    .any(|r| r["path"].as_str() == resolved.to_str())
                {
                    services.execute(AppCommand::AddRoot { path: resolved })?;
                }
            }
            let indexer = services.indexer.clone();
            let cancel = indexer.clone();
            ctrlc::set_handler(move || cancel.cancel())?;
            services.execute(AppCommand::StartIndex {})?;
            while indexer.status()["running"] == true {
                std::thread::sleep(Duration::from_millis(100));
            }
            let status = indexer.status();
            services.wait_shutdown()?;
            emit(status.clone());
            anyhow::ensure!(
                status["errors"].as_u64().unwrap_or(0) == 0,
                "일부 파일을 색인하지 못했습니다. 위 오류를 확인하세요."
            );
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
                && !arguments
                    .iter()
                    .any(|arg| arg == "--background" || arg == "--no-dialog")
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
