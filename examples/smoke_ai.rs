//! Explicit real Google-model validation against fictional repository examples.
use anyhow::Result;
use moasearch::{
    config, engine::EmbeddedEngine, indexer::Indexer, models, search::Search, store::Store,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
fn main() -> Result<()> {
    let data = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".moasearch-rust-smoke"));
    let models_dir = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".moasearch-rust-dev/models"));
    let store = Arc::new(Store::open(&data)?);
    store.update_settings(&json!({"embedding_model":config::canonical(&models_dir.join(models::EMBEDDING.filename))?,
        "chat_model":config::canonical(&models_dir.join(models::CHAT.filename))?,"scan_interval":0}))?;
    let fixtures =
        config::canonical(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/documents"))?;
    if !store
        .roots()?
        .iter()
        .any(|r| r["path"].as_str() == fixtures.to_str())
    {
        store.add_root(&fixtures)?;
    }
    let engine = Arc::new(EmbeddedEngine::new(store.clone()));
    let index = Indexer::new(store.clone(), engine.clone());
    let started = Instant::now();
    index.start();
    while index.status()["running"] == true {
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(index.status()["errors"] == 0, "{}", index.status());
    println!(
        "Google 모델 색인 완료: {} ({:.1}초)",
        store.counts()?,
        started.elapsed().as_secs_f64()
    );
    let search = Search::new(store.clone(), engine);
    let mut queries = Vec::new();
    for (query, expected) in [
        ("이사 나가면 맡긴 돈을 언제 돌려받나요", "임대차_메모.txt"),
        ("가족과 바닷가에서 쉬려고 해", "여행_계획.txt"),
        (
            "서비스 업데이트가 실패했을 때 복구하는 절차",
            "개발_회의.md",
        ),
    ] {
        let result = search.search(query, 3, None, "hybrid")?;
        anyhow::ensure!(
            result["mode"] == "hybrid" && result["results"][0]["name"] == expected,
            "{query}: {result}"
        );
        println!("의미 검색 통과: {query} -> {expected}");
        queries.push(json!({"query":query,"expected":expected,"passed":true}));
    }
    let answer = search.answer("보증금은 언제 반환되나요?", 2)?;
    let content = answer["answer"].as_str().unwrap_or_default();
    anyhow::ensure!(
        content.contains("14") && content.contains("[1]"),
        "문서 근거 답변: {answer}"
    );
    println!("Google Gemma 실제 답변: {content}");
    let report = json!({"runtime":"Rust","models":models::catalog(),"counts":store.counts()?,"queries":queries,"answer":answer});
    std::fs::write(
        data.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}
