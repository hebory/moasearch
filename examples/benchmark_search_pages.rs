//! Validate and time paged search against an existing, explicitly copied DB.
//! The indexed source files are read only; Store may initialize its copied DB.

use anyhow::{Context, Result, ensure};
use clap::Parser;
use moasearch::{
    commands::{Command, SearchPageCursor, SearchPageInput},
    engine::EmbeddedEngine,
    search::{canonical_folder, folder_contains_path},
    service::AppServices,
    store::Store,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Parser)]
#[command(about = "기존 색인 DB 복사본에서 검색 결과 페이지를 검증하고 측정합니다")]
struct Args {
    /// Must point to a disposable copy of the MoaSearch data directory.
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    report: PathBuf,
    /// Repeat to validate several independent one-token queries.
    #[arg(long = "query", required = true, num_args = 1..)]
    queries: Vec<String>,
    #[arg(long)]
    folder: Option<PathBuf>,
    #[arg(long, default_value_t = 200)]
    page_size: usize,
    /// Also run hybrid search; all literal keyword matches must remain present.
    #[arg(long)]
    hybrid: bool,
}

struct TimedPage {
    value: Value,
    elapsed_ms: f64,
    response_bytes: usize,
}

fn escaped_like_literal(value: &str) -> String {
    format!(
        "%{}%",
        value
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

fn expected_ids(store: &Store, query: &str, folder: Option<&Path>) -> Result<HashSet<i64>> {
    let pattern = escaped_like_literal(query);
    let db = store.connect()?;
    let mut statement = db.prepare(
        "SELECT f.id,f.path FROM files f
         WHERE f.path LIKE ?1 ESCAPE '\\' COLLATE NOCASE
            OR EXISTS (SELECT 1 FROM chunks c WHERE c.file_id=f.id
                       AND c.text LIKE ?1 ESCAPE '\\' COLLATE NOCASE)",
    )?;
    let rows = statement.query_map(params![pattern], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut ids = HashSet::new();
    for row in rows {
        let (id, path) = row?;
        if folder.is_none_or(|folder| folder_contains_path(folder, Path::new(&path))) {
            ids.insert(id);
        }
    }
    Ok(ids)
}

fn run_page(
    services: &Arc<AppServices>,
    query: &str,
    limit: usize,
    folder: Option<&Path>,
    mode: &str,
    cursor: Option<SearchPageCursor>,
) -> Result<TimedPage> {
    let started = Instant::now();
    let value = services.execute(Command::SearchPage(SearchPageInput {
        query: query.to_string(),
        limit,
        extension: None,
        folder: folder.map(Path::to_path_buf),
        mode: mode.to_string(),
        cursor,
    }))?;
    let response_bytes = serde_json::to_vec(&value)?.len();
    Ok(TimedPage {
        value,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        response_bytes,
    })
}

fn page_ids(page: &Value) -> Result<Vec<i64>> {
    page["results"]
        .as_array()
        .context("검색 응답에 results 배열이 없습니다")?
        .iter()
        .map(|item| item["id"].as_i64().context("검색 결과 파일 ID가 없습니다"))
        .collect()
}

fn run_query(
    services: &Arc<AppServices>,
    store: &Store,
    query: &str,
    limit: usize,
    folder: Option<&Path>,
    mode: &str,
) -> Result<Value> {
    let expected = expected_ids(store, query, folder)?;
    let mut seen = HashSet::new();
    let mut cursor: Option<SearchPageCursor> = None;
    let mut revision = None;
    let mut total = None;
    let mut pages = 0usize;
    let mut first_page_ms = None;
    let mut next_page_ms = None;
    let mut repeated_page_ms = None;
    let mut max_response_bytes = 0usize;
    let mut repeated_page_match = true;
    let mut repeated_page_ids: Option<Vec<i64>> = None;

    loop {
        let page = run_page(services, query, limit, folder, mode, cursor.clone())?;
        max_response_bytes = max_response_bytes.max(page.response_bytes);
        if pages == 0 {
            first_page_ms = Some(page.elapsed_ms);
        } else if pages == 1 {
            next_page_ms = Some(page.elapsed_ms);
            let replay = run_page(services, query, limit, folder, mode, cursor.clone())?;
            repeated_page_ms = Some(replay.elapsed_ms);
            max_response_bytes = max_response_bytes.max(replay.response_bytes);
            repeated_page_match = page_ids(&page.value)? == page_ids(&replay.value)?;
            ensure!(
                repeated_page_match,
                "동일 cursor 재요청 결과가 달라졌습니다"
            );
            repeated_page_ids = Some(page_ids(&replay.value)?);
        }

        ensure!(
            !page.value["reset_required"].as_bool().unwrap_or(true),
            "검색 snapshot이 재설정되었습니다"
        );
        let page_total = page.value["total"]
            .as_u64()
            .context("검색 응답 total이 없습니다")? as usize;
        let page_revision = page.value["revision"]
            .as_i64()
            .context("검색 응답 revision이 없습니다")?;
        if let Some(expected_total) = total {
            ensure!(
                page_total == expected_total,
                "페이지 간 total이 바뀌었습니다"
            );
        } else {
            total = Some(page_total);
        }
        if let Some(expected_revision) = revision {
            ensure!(
                page_revision == expected_revision,
                "페이지 간 revision이 바뀌었습니다"
            );
        } else {
            revision = Some(page_revision);
        }
        let ids = page_ids(&page.value)?;
        let current_offset = cursor.as_ref().map_or(0, |value| value.offset);
        ensure!(
            ids.len() == page_total.saturating_sub(current_offset).min(limit),
            "페이지 결과 개수가 total/offset과 맞지 않습니다"
        );
        for id in ids {
            ensure!(seen.insert(id), "페이지 사이에 중복된 파일 결과가 있습니다");
        }
        pages += 1;
        let next = page.value["next_cursor"]
            .as_object()
            .map(|value| SearchPageCursor {
                snapshot_id: value["snapshot_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                offset: value["offset"].as_u64().unwrap_or_default() as usize,
                revision: value["revision"].as_i64().unwrap_or_default(),
            });
        if let Some(next_cursor) = next {
            ensure!(
                next_cursor.offset == seen.len(),
                "cursor offset가 누적 페이지 결과 수와 다릅니다"
            );
            ensure!(
                next_cursor.revision == page_revision,
                "cursor revision이 다릅니다"
            );
            cursor = Some(next_cursor);
        } else {
            break;
        }
        if pages == 2 {
            ensure!(
                repeated_page_ids.as_ref() == Some(&page_ids(&page.value)?),
                "반복 페이지 ID가 달라졌습니다"
            );
        }
    }

    let keyword_expected_missing = expected.difference(&seen).count();
    let keyword_expected_ids_present = keyword_expected_missing == 0;
    if mode == "keyword" {
        ensure!(
            expected == seen,
            "keyword 페이지 전체 결과가 독립 SQL 예상 집합과 다릅니다"
        );
    } else {
        ensure!(
            keyword_expected_ids_present,
            "hybrid 페이지에서 keyword 예상 결과가 누락되었습니다"
        );
    }
    ensure!(
        seen.len() == total.unwrap_or_default(),
        "페이지 합계가 응답 total과 다릅니다"
    );

    Ok(json!({
        "mode":mode,
        "eligible_expected_files":expected.len(),
        "returned_total":seen.len(),
        "missing_expected_files":keyword_expected_missing,
        "extra_files":seen.difference(&expected).count(),
        "pages":pages,
        "page_size":limit,
        "revision":revision,
        "first_page_ms":first_page_ms,
        "next_page_ms":next_page_ms,
        "repeated_page_ms":repeated_page_ms,
        "repeated_page_identical":repeated_page_match,
        "keyword_expected_ids_present":keyword_expected_ids_present,
        "max_response_bytes":max_response_bytes,
    }))
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        (1..=500).contains(&args.page_size),
        "page size must be 1..=500"
    );
    ensure!(
        args.data.join("index.sqlite3").is_file(),
        "--data must be an existing copied data directory containing index.sqlite3"
    );
    let report_parent = args
        .report
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(report_parent)?;

    let store = Arc::new(Store::open(&args.data)?);
    let folder = args.folder.as_deref().map(canonical_folder).transpose()?;
    let services = AppServices::new(store.clone(), Arc::new(EmbeddedEngine::new(store.clone())));
    let initial_status = services.execute(Command::Status {})?;
    let gpu_backend = initial_status["engine"]["gpu_backend"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    ensure!(
        gpu_backend == "unavailable",
        "keyword benchmark unexpectedly loaded a model/backend: {gpu_backend}"
    );

    let mut query_reports = Vec::new();
    for query in &args.queries {
        let query = query.trim();
        ensure!(
            !query.is_empty() && !query.chars().any(char::is_whitespace),
            "each --query must be one non-empty token"
        );
        let keyword = run_query(
            &services,
            &store,
            query,
            args.page_size,
            folder.as_deref(),
            "keyword",
        )?;
        let keyword_backend_after = services.execute(Command::Status {})?["engine"]["gpu_backend"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        ensure!(
            keyword_backend_after == "unavailable",
            "keyword search unexpectedly loaded a model/backend: {keyword_backend_after}"
        );
        query_reports.push(json!({
            "keyword":keyword,
            "gpu_backend_after_keyword":keyword_backend_after,
            "hybrid":null,
        }));
    }
    // Finish all model-free checks before a hybrid query loads the backend.
    if args.hybrid {
        for (query, report) in args.queries.iter().zip(query_reports.iter_mut()) {
            report["hybrid"] = run_query(
                &services,
                &store,
                query.trim(),
                args.page_size,
                folder.as_deref(),
                "hybrid",
            )?;
        }
    }
    let report = json!({
        "benchmark":"search-pages",
        "scope":"existing copied index DB; one-token literal LIKE validation; source filesystem is not modified",
        "gpu_backend_before_queries":gpu_backend,
        "gpu_backend_after_queries":services.execute(Command::Status {})?["engine"]["gpu_backend"],
        "page_size":args.page_size,
        "folder_scoped":folder.is_some(),
        "queries":query_reports,
    });
    std::fs::write(&args.report, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
