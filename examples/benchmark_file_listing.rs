//! Verify every metadata page against an existing index without loading an AI model.
use anyhow::{Result, ensure};
use clap::Parser;
use moasearch::{
    commands::{Command, ListCursor, ListFilesInput, SearchInput},
    engine::EmbeddedEngine,
    search::{canonical_folder, folder_contains_path},
    service::AppServices,
    store::Store,
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    report: PathBuf,
    #[arg(long)]
    folder: Option<PathBuf>,
    #[arg(long)]
    query: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.data.join("index.sqlite3").is_file(),
        "기존 검증 DB가 필요합니다."
    );
    let store = Arc::new(Store::open(&args.data)?);
    let folder = args
        .folder
        .map(|path| canonical_folder(&path))
        .transpose()?;
    let expected: HashMap<i64, String> = {
        let db = store.connect()?;
        let mut statement = db.prepare("SELECT id,path FROM files")?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, path)| {
                folder
                    .as_ref()
                    .is_none_or(|root| folder_contains_path(root, std::path::Path::new(path)))
            })
            .collect()
    };
    let engine = Arc::new(EmbeddedEngine::new(store.clone()));
    let services = AppServices::new(store, engine.clone());
    let mut cursor = None;
    let mut seen = HashMap::new();
    let mut page_ms = Vec::new();
    let mut revision = None;
    let mut response_bytes = Vec::new();
    loop {
        let start = Instant::now();
        let data = services.execute(Command::ListFiles(ListFilesInput {
            limit: 200,
            extension: None,
            folder: folder.clone(),
            cursor,
        }))?;
        page_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        response_bytes.push(serde_json::to_vec(&data)?.len());
        ensure!(
            data["reset_required"] != true,
            "검증 중 목록이 변경됐습니다."
        );
        ensure!(
            data["total"].as_u64() == Some(expected.len() as u64),
            "총 개수 불일치"
        );
        let page_revision = data["revision"].as_u64().unwrap();
        if let Some(revision) = revision {
            ensure!(revision == page_revision, "페이지 revision 불일치");
        }
        revision = Some(page_revision);
        let rows = data["results"].as_array().unwrap();
        ensure!(rows.len() <= 200, "페이지 상한 초과");
        for row in rows {
            let id = row["id"].as_i64().unwrap();
            let path = row["path"].as_str().unwrap().to_string();
            ensure!(expected.get(&id) == Some(&path), "범위 밖 또는 잘못된 파일");
            ensure!(seen.insert(id, path).is_none(), "중복 페이지 파일");
            for field in ["text", "snippet", "passages", "vector", "content_key"] {
                ensure!(row.get(field).is_none(), "목록이 본문 데이터를 노출합니다.");
            }
        }
        cursor = serde_json::from_value::<Option<ListCursor>>(data["next_cursor"].clone())?;
        if cursor.is_none() {
            break;
        }
        ensure!(!rows.is_empty(), "진행하지 않는 cursor");
    }
    ensure!(seen.len() == expected.len(), "전체 목록 누락");
    let mut repeated = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        services.execute(Command::ListFiles(ListFilesInput {
            limit: 200,
            extension: None,
            folder: folder.clone(),
            cursor: None,
        }))?;
        repeated.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    repeated.sort_by(f64::total_cmp);
    let mut instant_queries = Vec::new();
    for query in args.query {
        let start = Instant::now();
        let data = services.execute(Command::Search(SearchInput {
            query: query.clone(),
            limit: 30,
            extension: None,
            mode: "instant".into(),
            folder: folder.clone(),
        }))?;
        instant_queries.push(json!({"query":query,"milliseconds":start.elapsed().as_secs_f64()*1000.0,"results":data["results"].as_array().unwrap().len()}));
    }
    let status = engine.status();
    ensure!(
        status["gpu_backend"] == "unavailable",
        "파일 목록이 추론 backend를 로드했습니다."
    );
    let result: Value = json!({
        "files_verified":seen.len(),"pages":page_ms.len(),"page_limit":200,
        "first_page_ms":page_ms[0],"all_pages_ms":page_ms.iter().sum::<f64>(),
        "maximum_page_ms":page_ms.iter().copied().fold(0.0,f64::max),
        "maximum_response_bytes":response_bytes.iter().max(),
        "repeated_first_page_median_ms":repeated[50],"repeated_first_page_p95_ms":repeated[94],
        "model_loaded":false,"missing":0,"duplicates":0,
        "instant_queries":instant_queries,
    });
    std::fs::write(&args.report, serde_json::to_vec_pretty(&result)?)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    services.begin_shutdown();
    services.wait_shutdown()?;
    Ok(())
}
