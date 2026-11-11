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

#[derive(Parser)]
#[command(version, about = "모아검색 — Rust와 Google 내장 AI로 내 파일 검색")]
struct Cli {
    #[arg(long, env="MOASEARCH_DATA_DIR", default_value_os_t=config::data_directory())]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// 로컬 검색 화면 실행
    Serve {
        #[arg(long,default_value_t=8765,value_parser=clap::value_parser!(u16).range(1024..))]
        port: u16,
        #[arg(long)]
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
    },
    /// 검색된 문서를 근거로 Google Gemma 답변 생성
    Answer { query: String },
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
    match args.command.unwrap_or(Command::Serve {
        port: 8765,
        no_browser: false,
    }) {
        Command::Doctor => emit(
            json!({"runtime":"Rust / llama.cpp (in-process)","data_dir":store.data_dir,"counts":store.counts()?,"settings":store.settings()?,"engine":engine.status(),"ocr":ocr::status(&store.data_dir.join("models"))}),
        ),
        Command::Search { query, keyword } => emit(Search::new(store, engine).search(
            &query,
            30,
            None,
            if keyword { "keyword" } else { "hybrid" },
        )?),
        Command::Answer { query } => emit(Search::new(store, engine).answer(&query, 5)?),
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
        Command::Serve { port, no_browser } => {
            let state = AppState::new(store, engine);
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                eprintln!("모아검색 Rust: http://127.0.0.1:{port}");
                state.indexer.start_scheduler();
                if !no_browser {
                    let url = format!("http://127.0.0.1:{port}");
                    std::thread::spawn(move || {
                        let _ = app::shell_open(&url);
                    });
                }
                let shutdown = state.indexer.clone();
                axum::serve(listener, app::router(state))
                    .with_graceful_shutdown(async move {
                        let _ = tokio::signal::ctrl_c().await;
                        shutdown.close();
                        while shutdown.status()["running"] == true {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    })
                    .await?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Command::Models { .. } => unreachable!(),
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("오류: {error:#}");
        std::process::exit(1);
    }
}
