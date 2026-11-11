//! Full OCR -> chunk -> Google embedding -> search regression on fictional files.
use anyhow::{Result, ensure};
use moasearch::{
    config, engine::EmbeddedEngine, indexer::Indexer, models, search::Search, store::Store,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
fn main() -> Result<()> {
    let data = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".moasearch-rust-format-smoke"));
    let model_dir = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".moasearch-rust-dev/models"));
    let store = Arc::new(Store::open(&data)?);
    let (detector, recognizer) = models::verify_ocr_models(&model_dir)?;
    std::fs::create_dir_all(store.data_dir.join("models"))?;
    for source in [detector, recognizer] {
        let target = store
            .data_dir
            .join("models")
            .join(source.file_name().unwrap());
        if !target.exists() {
            std::fs::copy(source, target)?;
        }
    }
    store.update_settings(&json!({"embedding_model":config::canonical(&model_dir.join(models::EMBEDDING.filename))?,"scan_interval":0}))?;
    // Index only the five documents. The fixture README describes their answers
    // and would otherwise contaminate the retrieval regression corpus.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let inputs = data.with_extension("fixtures");
    std::fs::create_dir_all(&inputs)?;
    for name in [
        "hwp-aurora.hwp",
        "hwpx-cyan.hwpx",
        "receipt.png",
        "meeting.pdf",
        "rental.pdf",
    ] {
        let target = inputs.join(name);
        if target.exists() {
            ensure!(
                std::fs::read(&target)? == std::fs::read(source.join(name))?,
                "기존 검증 파일이 다릅니다. 새 데이터 폴더로 실행하세요: {}",
                target.display()
            );
        } else {
            std::fs::copy(source.join(name), target)?;
        }
    }
    let fixture = config::canonical(&inputs)?;
    ensure!(
        store
            .roots()?
            .iter()
            .all(|r| r["path"].as_str() == fixture.to_str()),
        "별도 새 검증 데이터 폴더로 실행하세요."
    );
    if !store
        .roots()?
        .iter()
        .any(|r| r["path"].as_str() == fixture.to_str())
    {
        store.add_root(&fixture)?;
    }
    let engine = Arc::new(EmbeddedEngine::new(store.clone()));
    let index = Indexer::new(store.clone(), engine.clone());
    index.start();
    while index.status()["running"] == true {
        std::thread::sleep(Duration::from_millis(100));
    }
    ensure!(index.status()["errors"] == 0, "{}", index.status());
    let search = Search::new(store.clone(), engine);
    let mut results = Vec::new();
    for (query, expected, mode) in [
        ("오로라-3147", "hwp-aurora.hwp", "keyword"),
        ("나침반-6602", "hwpx-cyan.hwpx", "keyword"),
        (
            "이사 나간 뒤 맡긴 돈을 돌려받는 조건",
            "rental.pdf",
            "hybrid",
        ),
        (
            "서비스 배포가 실패하면 이전 버전으로 복구하는 방법",
            "meeting.pdf",
            "hybrid",
        ),
        ("출장 교통비를 청구할 지출 내역", "receipt.png", "hybrid"),
    ] {
        let result = search.search(query, 3, None, mode)?;
        ensure!(
            result["mode"] == mode && result["results"][0]["name"] == expected,
            "{query}: {result}"
        );
        println!("Rust 내용 검색 통과: {query} -> {expected}");
        results.push(json!({"query":query,"expected":expected,"mode":mode,"passed":true}));
    }
    index.start();
    while index.status()["running"] == true {
        std::thread::sleep(Duration::from_millis(100));
    }
    ensure!(
        index.status()["indexed"] == 0 && index.status()["errors"] == 0,
        "{}",
        index.status()
    );
    let report = json!({"runtime":"Rust","models":models::catalog(),"counts":store.counts()?,"searches":results,"unchanged_index":index.status()});
    std::fs::write(
        data.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
