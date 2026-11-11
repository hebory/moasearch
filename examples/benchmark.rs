//! Synthetic indexing/search benchmark. It reads only files generated here.
//! Model inference and OCR are intentionally excluded.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use moasearch::{engine::Embedder, indexer::Indexer, search::Search, store::Store};
use serde_json::{Value, json};

struct FixtureEmbedder;

impl Embedder for FixtureEmbedder {
    fn model_id(&self) -> Option<String> {
        Some("benchmark-fixture-v1".into())
    }

    fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0; 384];
                if query || text.contains("EXPECTED_FILE_SENTINEL") {
                    vector[0] = 1.0;
                } else {
                    vector[1] = 1.0;
                }
                vector
            })
            .collect())
    }

    fn answer(&self, _question: &str, _context: &str) -> Result<String> {
        Ok("benchmark fixture".into())
    }
}

struct Options {
    files: usize,
    repeats: usize,
    root: PathBuf,
}

fn options() -> Result<Options> {
    let mut files = 5_000usize;
    let mut repeats = 5usize;
    let mut root = PathBuf::from(format!(".moasearch-perf/run-rust-{}", std::process::id()));
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        if index + 1 == args.len() {
            anyhow::bail!("값이 필요한 옵션입니다: {}", args[index]);
        }
        let value = &args[index + 1];
        match args[index].as_str() {
            "--files" => files = value.parse()?,
            "--repeats" => repeats = value.parse()?,
            "--root" => root = PathBuf::from(value),
            other => anyhow::bail!("알 수 없는 옵션입니다: {other}"),
        }
        index += 2;
    }
    ensure!(
        (1..=100_000).contains(&files),
        "--files는 1~100000이어야 합니다."
    );
    ensure!(
        (2..=30).contains(&repeats),
        "--repeats는 2~30이어야 합니다."
    );
    Ok(Options {
        files,
        repeats,
        root,
    })
}

fn index(indexer: &Arc<Indexer>) -> Result<(Value, Duration)> {
    ensure!(indexer.start(), "색인 작업을 시작하지 못했습니다.");
    let started = Instant::now();
    while indexer.status()["running"] == true {
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();
    let status = indexer.status();
    ensure!(
        status["errors"].as_u64().unwrap_or(0) == 0,
        "합성 색인 오류: {}",
        status["last_error"]
    );
    Ok((status, elapsed))
}

fn main() -> Result<()> {
    let options = options()?;
    ensure!(
        !options.root.exists(),
        "기존 경로를 보호하기 위해 아직 없는 새 경로를 지정하세요: {}",
        options.root.display()
    );
    std::fs::create_dir_all(&options.root)?;
    let library = options.root.join("files");
    let data = options.root.join("data");
    std::fs::create_dir_all(&library)?;
    let corpus = "synthetic benchmark text contains ordinary words and sample facts. ".repeat(58);

    for index in 0..options.files {
        let mut text = format!("synthetic document {index:06}\n");
        text.push_str(&corpus);
        if index == 0 {
            text.push_str(" EXPECTED_FILE_SENTINEL");
        }
        std::fs::write(library.join(format!("document-{index:06}.txt")), text)?;
    }

    let store = Arc::new(Store::open(&data)?);
    store.add_root(
        library
            .to_str()
            .context("벤치마크 폴더 경로가 올바르지 않습니다")?,
    )?;
    let engine: Arc<dyn Embedder> = Arc::new(FixtureEmbedder);
    let indexer = Indexer::new(store.clone(), engine.clone());

    let (initial_status, initial_time) = index(&indexer)?;
    let (unchanged_status, unchanged_time) = index(&indexer)?;

    let modified_count = (options.files / 100).max(1);
    for index in 0..modified_count {
        let path = library.join(format!("document-{index:06}.txt"));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        writeln!(file, "\nchanged synthetic content")?;
    }
    let (modified_status, modified_time) = index(&indexer)?;

    let search = Search::new(store.clone(), engine);
    let mut search_ms = Vec::new();
    for index in 0..options.repeats {
        // Avoid corpus/path terms and document-number digits: this measurement
        // isolates vector retrieval without keyword/filename rank collisions.
        let query = format!(
            "벡터검증전용질의{}",
            char::from_u32(0xAC00 + index as u32).unwrap()
        );
        let started = Instant::now();
        let result = search.search(&query, 10, None, "hybrid")?;
        search_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        ensure!(
            result["mode"] == "hybrid",
            "검색이 의미 모드로 동작하지 않았습니다: {result}"
        );
        ensure!(
            result["results"]
                .as_array()
                .and_then(|rows| rows.first())
                .and_then(|r| r["name"].as_str())
                == Some("document-000000.txt"),
            "정해 둔 검색 결과를 찾지 못했습니다: {result}"
        );
    }
    let mut warm = search_ms.iter().copied().skip(1).collect::<Vec<_>>();
    warm.sort_by(f64::total_cmp);
    let middle = warm.len() / 2;
    let warm_median_ms = if warm.len() % 2 == 0 {
        (warm[middle - 1] + warm[middle]) / 2.0
    } else {
        warm[middle]
    };

    let report = json!({
        "files": options.files,
        "chunks": store.counts()?["chunks"],
        "queries": options.repeats,
        "counts": store.counts()?,
        "index_seconds": {
            "initial": initial_time.as_secs_f64(),
            "unchanged": unchanged_time.as_secs_f64(),
            "modified_count": modified_count,
            "modified": modified_time.as_secs_f64(),
        },
        "indexed_files": {
            "initial": initial_status["indexed"],
            "unchanged_skipped": unchanged_status["skipped"],
            "modified": modified_status["indexed"],
        },
        "search_ms": search_ms,
        "search_cold_ms": search_ms[0],
        "search_warm_median_ms": warm_median_ms,
        "scope": "합성 텍스트 파일과 결정적 가짜 벡터. 모델 추론, 모델 로드, OCR 제외.",
    });
    let report_path = options.root.join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("벤치마크 보고서: {}", report_path.display());
    Ok(())
}
