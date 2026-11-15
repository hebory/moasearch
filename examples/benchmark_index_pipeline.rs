//! Measure a fresh real-folder index and concurrent searches in one app process.
//! All paths and query text stay in the explicitly selected local report.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use moasearch::{
    config,
    engine::EmbeddedEngine,
    models::{self, ModelKind},
    service::AppServices,
    store::Store,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    data: PathBuf,
    #[arg(long)]
    model: PathBuf,
    #[arg(long)]
    scenarios: PathBuf,
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value = "auto")]
    gpu_mode: String,
    #[arg(long)]
    expect_backend: Option<String>,
    /// Request cancellation at this deadline, then wait for in-flight native work.
    #[arg(long, default_value_t = 7200)]
    timeout_seconds: u64,
    /// Explicitly allow an existing DB for a separately labelled warm rescan.
    #[arg(long)]
    resume: bool,
}

#[derive(Deserialize)]
struct Scenarios {
    filename_query: String,
    filename_expected: Vec<String>,
    queries: Vec<Scenario>,
}
#[derive(Deserialize)]
struct Scenario {
    id: String,
    query: String,
    expected: Vec<String>,
}

fn rank(result: &Value, expected: &[String]) -> Option<usize> {
    result["results"]
        .as_array()?
        .iter()
        .position(|hit| {
            hit["path"]
                .as_str()
                .is_some_and(|path| expected.iter().any(|want| path.eq_ignore_ascii_case(want)))
        })
        .map(|index| index + 1)
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        matches!(args.gpu_mode.as_str(), "auto" | "cpu"),
        "invalid GPU mode"
    );
    let root = config::canonical(&args.root)?;
    let data_path = if args.data.exists() {
        config::canonical(&args.data)?
    } else {
        std::path::absolute(&args.data)?
    };
    ensure!(
        !config::within(&data_path, &root) && !config::within(&root, &data_path),
        "source and benchmark data must be separate"
    );
    let preflight = Instant::now();
    models::verify_model(&args.model, ModelKind::Embedding)?;
    let preflight_seconds = preflight.elapsed().as_secs_f64();
    let report_parent = config::canonical(args.report.parent().context("report parent required")?)?;
    ensure!(
        !config::within(&report_parent, &root),
        "reports must be outside the source root"
    );
    ensure!(
        !config::within(&config::canonical(&args.scenarios)?, &root),
        "scenarios must be outside the source root"
    );
    if !args.resume {
        ensure!(
            !args.data.join("index.sqlite3").exists() && !args.data.join("moasearch.db").exists(),
            "fresh benchmark requires a new data directory"
        );
    }
    let scenarios: Scenarios = serde_json::from_slice(&std::fs::read(&args.scenarios)?)?;
    ensure!(
        !scenarios.queries.is_empty(),
        "at least one query is required"
    );
    let store = Arc::new(Store::open(args.data.clone())?);
    ensure!(
        !config::within(&store.data_dir, &root) && !config::within(&root, &store.data_dir),
        "source and benchmark data must be separate"
    );
    ensure!(
        args.resume || store.counts()?["files"].as_u64().unwrap_or(0) == 0,
        "fresh benchmark must contain no indexed files"
    );
    store.update_settings(&json!({"embedding_model":args.model,"gpu_mode":args.gpu_mode,"scan_interval":0,"chat_enabled":false}))?;
    let roots = store.roots()?;
    if roots.is_empty() {
        store.add_root(&root)?;
    } else {
        ensure!(
            roots.len() == 1
                && roots[0]["path"].as_str().is_some_and(|path| {
                    let path = std::path::Path::new(path);
                    config::within(path, &root) && config::within(&root, path)
                }),
            "resume requires the same single benchmark root"
        );
    }
    let services = AppServices::new(store.clone(), Arc::new(EmbeddedEngine::new(store.clone())));
    let finished = Arc::new(AtomicBool::new(false));
    let aborted = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::<Value>::new()));
    let started = Instant::now();
    ensure!(services.indexer.start(), "index could not start");
    let search_services = services.clone();
    let search_finished = finished.clone();
    let search_aborted = aborted.clone();
    let search_samples = samples.clone();
    let search_root = root.clone();
    let searcher = std::thread::spawn(move || -> Result<Value> {
        let mut turn = 0;
        let mut embedded_queries = HashSet::new();
        while !search_finished.load(Ordering::Acquire) {
            let before = search_services.indexer.status();
            let timer = Instant::now();
            let result = search_services.search.search_in(
                &scenarios.filename_query,
                30,
                None,
                "instant",
                Some(&search_root),
            )?;
            search_samples.lock().unwrap().push(json!({"kind":"filename","at_seconds":started.elapsed().as_secs_f64(),"milliseconds":timer.elapsed().as_secs_f64()*1000.0,"rank":rank(&result,&scenarios.filename_expected),"index_before":before,"index_after":search_services.indexer.status()}));
            // A single sampler represents a user issuing independent queries;
            // no query cache is prefilled before the measured first request.
            let case = &scenarios.queries[turn % scenarios.queries.len()];
            let before = search_services.indexer.status();
            let timer = Instant::now();
            let instant = search_services.search.search_in(
                &case.query,
                30,
                None,
                "instant",
                Some(&search_root),
            )?;
            let lexical_ms = timer.elapsed().as_secs_f64() * 1000.0;
            let has_vectors = search_services.store.counts()?["vectors"]
                .as_u64()
                .unwrap_or(0)
                > 0;
            let timer = Instant::now();
            let semantic = search_services.search.search_in(
                &case.query,
                30,
                None,
                "hybrid",
                Some(&search_root),
            )?;
            let cache = if !has_vectors {
                "not_ready"
            } else if embedded_queries.insert(case.id.clone()) {
                "first_query"
            } else {
                "repeat"
            };
            search_samples.lock().unwrap().push(json!({"kind":"natural","id":case.id,"query_cache":cache,"at_seconds":started.elapsed().as_secs_f64(),"instant_ms":lexical_ms,"hybrid_ms":timer.elapsed().as_secs_f64()*1000.0,"instant_rank":rank(&instant,&case.expected),"rank":rank(&semantic,&case.expected),"warnings":semantic["warnings"],"index_before":before,"index_after":search_services.indexer.status()}));
            turn += 1;
            std::thread::sleep(Duration::from_millis(750));
        }
        if search_aborted.load(Ordering::Acquire) {
            return Ok(json!([]));
        }
        let mut final_queries = Vec::new();
        for case in &scenarios.queries {
            let timer = Instant::now();
            let result = search_services.search.search_in(
                &case.query,
                30,
                None,
                "hybrid",
                Some(&search_root),
            )?;
            let semantic_hits = result["results"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|hit| {
                    hit["match_type"]
                        .as_str()
                        .is_some_and(|kind| kind.contains("semantic"))
                })
                .count();
            final_queries.push(json!({"id":case.id,"milliseconds":timer.elapsed().as_secs_f64()*1000.0,"mode":result["mode"],"semantic_hits":semantic_hits,"rank":rank(&result,&case.expected),"warnings":result["warnings"]}));
        }
        Ok(json!(final_queries))
    });
    let mut metadata_seconds = None;
    let mut stages = Vec::new();
    let mut last_phase = String::new();
    let mut timed_out = false;
    let mut last_progress = Instant::now();
    loop {
        let status = services.indexer.status();
        let phase = status["phase"].as_str().unwrap_or("");
        if phase != last_phase {
            stages.push(json!({"at_seconds":started.elapsed().as_secs_f64(),"status":status}));
            last_phase = phase.to_string();
        }
        if last_progress.elapsed() >= Duration::from_secs(10) {
            let progress = json!({"seconds":started.elapsed().as_secs_f64(),"metadata_seconds":metadata_seconds,"index":status,"counts":store.counts()?});
            std::fs::write(
                args.report.with_extension("progress.json"),
                serde_json::to_vec(&progress)?,
            )?;
            last_progress = Instant::now();
        }
        if metadata_seconds.is_none() && status["metadata_complete"] == true {
            metadata_seconds = Some(started.elapsed().as_secs_f64());
        }
        // Support counters as well as the explicit metadata completion flag.
        if metadata_seconds.is_none() && matches!(phase, "content" | "embedding" | "complete") {
            metadata_seconds = Some(started.elapsed().as_secs_f64());
        }
        if status["running"] != true {
            break;
        }
        if started.elapsed().as_secs() >= args.timeout_seconds {
            timed_out = true;
            aborted.store(true, Ordering::Release);
            services.indexer.cancel();
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let index_seconds = started.elapsed().as_secs_f64();
    finished.store(true, Ordering::Release);
    let (final_queries, search_error) = match searcher.join() {
        Ok(Ok(queries)) => (queries, None),
        Ok(Err(error)) => (json!([]), Some(error.to_string())),
        Err(_) => (json!([]), Some("sampler panicked".to_string())),
    };
    services.wait_shutdown()?;
    let report = json!({"fresh_database":!args.resume,"root":root,"data":args.data,"gpu_mode":args.gpu_mode,"preflight_sha256_seconds":preflight_seconds,"metadata_seconds":metadata_seconds,"index_seconds":if timed_out {None} else {Some(index_seconds)},"cancel_requested_seconds":if timed_out {Some(index_seconds)} else {None},"total_elapsed_seconds":started.elapsed().as_secs_f64(),"timed_out":timed_out,"stages":stages,"samples":*samples.lock().unwrap(),"final_queries":final_queries,"search_error":search_error,"final_status":services.indexer.status(),"counts":store.counts()?,"engine":services.engine.status()});
    if let Some(parent) = args.report.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.report, serde_json::to_vec_pretty(&report)?)
        .context("write local report")?;
    println!(
        "{}",
        json!({"report":args.report,"metadata_seconds":metadata_seconds,"index_seconds":index_seconds,"timed_out":timed_out,"counts":report["counts"],"status":report["final_status"]["phase"]})
    );
    ensure!(
        !timed_out
            && report["final_status"]["phase"] == "complete"
            && report["final_status"]["embedding_state"] == "complete"
            && search_error.is_none(),
        "index did not complete successfully"
    );
    ensure!(
        report["final_queries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|q| q["mode"] == "hybrid"
                && q["semantic_hits"].as_u64().is_some_and(|n| n > 0)
                && q["warnings"].as_array().is_some_and(Vec::is_empty)
                && q["rank"].as_u64().is_some_and(|r| r <= 5)),
        "a final query missed its expected top-five result"
    );
    ensure!(
        report["counts"]["vectors"].as_u64().is_some_and(|n| n > 0),
        "no semantic vectors were indexed"
    );
    if let Some(expected) = args.expect_backend {
        ensure!(
            report["engine"]["gpu_backend"] == expected,
            "unexpected compute backend: {}",
            report["engine"]
        );
    }
    Ok(())
}
