//! Process-isolated smoke test for long EmbeddingGemma inputs.
//!
//! Run in its own process with MOASEARCH_EMBEDDING_MODEL set to the pinned
//! embeddinggemma-2-Q8_0.gguf file. Inputs are synthetic and never read from disk.
use anyhow::{Context, Result, bail, ensure};
use llama_cpp_2::{
    llama_backend::LlamaBackend,
    model::{LlamaModel, params::LlamaModelParams},
};
use moasearch::{
    config,
    engine::{EmbeddedEngine, Embedder},
    extract::{Passage, chunk_passages},
    models::{self, ModelKind},
    store::Store,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const MICRO_BATCH: usize = 512;
const DEFAULT_CONTEXT: usize = 4096;
const LONGER_TOKEN_BOUND: usize = 8192;

struct InputCase {
    name: &'static str,
    language: &'static str,
    role: &'static str,
    text: String,
    query: bool,
    token_count: usize,
    source_chars: usize,
    chunked_1200: bool,
}

fn wrapped(text: &str, query: bool) -> String {
    if query {
        format!("task: search result | query: {text}")
    } else {
        format!("title: none | text: {text}")
    }
}

fn count_tokens(vocab: &llama_cpp_2::vocab::LlamaVocab<'_>, text: &str, query: bool) -> usize {
    vocab
        .tokenize(wrapped(text, query).as_bytes(), true, true)
        .len()
}

fn synthetic_over_tokens(
    vocab: &llama_cpp_2::vocab::LlamaVocab<'_>,
    query: bool,
    phrase: &str,
    minimum_tokens: usize,
) -> Result<(String, usize)> {
    let mut text = String::new();
    for index in 0..LONGER_TOKEN_BOUND {
        use std::fmt::Write as _;
        write!(&mut text, "{phrase}{index} ")?;
        let tokens = count_tokens(vocab, &text, query);
        if tokens > minimum_tokens {
            return Ok((text, tokens));
        }
    }
    bail!("합성 입력이 {minimum_tokens} 토큰을 넘지 못했습니다.");
}

fn korean_chunk_over_micro_batch(
    vocab: &llama_cpp_2::vocab::LlamaVocab<'_>,
) -> Result<(String, usize, usize)> {
    // Deliberately varied Hangul syllables keep the sample synthetic and avoid
    // accidentally compressing a repeated common phrase into a tiny token set.
    let mut seed = 0x4d6f_6153_u32;
    let text: String = (0..4800)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from_u32(0xAC00 + seed % 11_172).expect("valid Hangul syllable")
        })
        .collect();
    let passages = chunk_passages(
        vec![Passage {
            text,
            location: "합성 색인 본문".into(),
        }],
        1200,
        160,
    )?;
    for passage in passages {
        let tokens = count_tokens(vocab, &passage.text, false);
        if tokens > MICRO_BATCH && tokens <= DEFAULT_CONTEXT - 2 {
            return Ok((passage.text.clone(), tokens, passage.text.chars().count()));
        }
    }
    bail!("chunk_passages(1200, 160)에서 {MICRO_BATCH} 토큰 초과 한국어 청크를 만들지 못했습니다.");
}

fn build_cases(model_path: &PathBuf) -> Result<Vec<InputCase>> {
    models::verify_model(model_path, ModelKind::Embedding)?;
    let mut backend = LlamaBackend::init().context("토크나이저 backend 초기화 실패")?;
    backend.void_logs();
    let params = LlamaModelParams::default().with_vocab_only(true);
    let tokenizer_model = LlamaModel::load_from_file(&backend, model_path, &params)
        .context("고정 임베딩 모델의 tokenizer를 열지 못했습니다")?;
    let vocab = tokenizer_model.vocab();

    let (ko_body, ko_body_tokens, ko_body_chars) = korean_chunk_over_micro_batch(&vocab)?;
    let (ko_query, ko_query_tokens) = synthetic_over_tokens(
        &vocab,
        true,
        "가상의 한국어 검색어 토큰 경계 검증 문장 ",
        MICRO_BATCH,
    )?;
    let (en_body, en_body_tokens) = synthetic_over_tokens(
        &vocab,
        false,
        "Synthetic English source-code body token-boundary check item ",
        MICRO_BATCH,
    )?;
    let (en_query, en_query_tokens) = synthetic_over_tokens(
        &vocab,
        true,
        "Synthetic English long search query token-boundary verification ",
        MICRO_BATCH,
    )?;
    let (context_body, context_body_tokens) = synthetic_over_tokens(
        &vocab,
        false,
        "Synthetic body for exact 4094-token context truncation verification ",
        DEFAULT_CONTEXT - 2,
    )?;
    drop(vocab);
    drop(tokenizer_model);
    drop(backend);

    Ok(vec![
        InputCase {
            name: "korean_index_chunk_1200",
            language: "ko",
            role: "body",
            source_chars: ko_body_chars,
            text: ko_body,
            query: false,
            token_count: ko_body_tokens,
            chunked_1200: true,
        },
        InputCase {
            name: "korean_query",
            language: "ko",
            role: "query",
            source_chars: ko_query.chars().count(),
            text: ko_query,
            query: true,
            token_count: ko_query_tokens,
            chunked_1200: false,
        },
        InputCase {
            name: "english_body",
            language: "en",
            role: "body",
            source_chars: en_body.chars().count(),
            text: en_body,
            query: false,
            token_count: en_body_tokens,
            chunked_1200: false,
        },
        InputCase {
            name: "english_query",
            language: "en",
            role: "query",
            source_chars: en_query.chars().count(),
            text: en_query,
            query: true,
            token_count: en_query_tokens,
            chunked_1200: false,
        },
        InputCase {
            name: "default_context_truncation_body",
            language: "en",
            role: "body",
            source_chars: context_body.chars().count(),
            text: context_body,
            query: false,
            token_count: context_body_tokens,
            chunked_1200: false,
        },
    ])
}

fn verify_context(
    engine: &EmbeddedEngine,
    store: &Store,
    context_size: usize,
    cases: &[InputCase],
    expected_dimension: &mut Option<usize>,
) -> Result<Vec<Value>> {
    store.update_settings(&json!({"context_size":context_size}))?;
    let mut report = Vec::with_capacity(cases.len());
    for case in cases {
        ensure!(
            case.token_count > MICRO_BATCH,
            "{} 입력이 micro-batch보다 길지 않습니다: {}",
            case.name,
            case.token_count
        );
        let vectors = engine.embed(std::slice::from_ref(&case.text), case.query)?;
        ensure!(vectors.len() == 1, "{} 임베딩 개수가 다릅니다", case.name);
        let vector = &vectors[0];
        ensure!(!vector.is_empty(), "{} 임베딩 차원이 0입니다", case.name);
        ensure!(
            vector.iter().all(|value| value.is_finite()),
            "{} 임베딩에 NaN/무한대가 있습니다",
            case.name
        );
        let dimension = vector.len();
        if let Some(expected) = *expected_dimension {
            ensure!(
                dimension == expected,
                "{} 임베딩 차원이 일치하지 않습니다",
                case.name
            );
        } else {
            *expected_dimension = Some(dimension);
        }
        let norm = vector
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        ensure!(
            (norm - 1.0).abs() <= 0.002,
            "{} 임베딩 norm이 1이 아닙니다: {norm}",
            case.name
        );
        report.push(json!({
            "case": case.name,
            "language": case.language,
            "role": case.role,
            "source_chars": case.source_chars,
            "chunked_1200": case.chunked_1200,
            "input_tokens": case.token_count,
            "retained_tokens": case.token_count.min(context_size - 2),
            "truncated": case.token_count > context_size - 2,
            "vector_dim": dimension,
            "vector_norm": norm,
            "finite": true,
        }));
    }
    let short = &cases[0];
    let long = cases
        .iter()
        .find(|case| case.name == "default_context_truncation_body")
        .unwrap();
    let batch = engine.embed(&[short.text.clone(), long.text.clone()], false)?;
    ensure!(batch.len() == 2, "혼합 길이 본문 배치 임베딩 수가 다릅니다");
    let batch_report: Vec<_> = batch.iter().zip([short, long]).map(|(vector, case)| {
        let norm = vector.iter().map(|value| f64::from(*value) * f64::from(*value)).sum::<f64>().sqrt();
        json!({"case":case.name,"input_tokens":case.token_count,"retained_tokens":case.token_count.min(context_size - 2),"vector_dim":vector.len(),"vector_norm":norm,"finite":vector.iter().all(|value| value.is_finite())})
    }).collect();
    ensure!(
        batch_report.iter().all(|entry| entry["finite"] == true
            && (entry["vector_norm"].as_f64().unwrap_or_default() - 1.0).abs() <= 0.002),
        "혼합 길이 배치 벡터 검증 실패"
    );
    ensure!(
        batch
            .iter()
            .all(|vector| vector.len() == expected_dimension.unwrap_or_default()),
        "혼합 길이 배치 차원이 다릅니다"
    );
    report.push(json!({"case":"mixed_body_batch","items":batch_report}));
    Ok(report)
}

fn main() -> Result<()> {
    let model_path = std::env::var_os("MOASEARCH_EMBEDDING_MODEL")
        .map(PathBuf::from)
        .context("MOASEARCH_EMBEDDING_MODEL에 고정 EmbeddingGemma GGUF 경로를 지정하세요")?;
    let model_path = config::canonical(&model_path)?;
    let cases = build_cases(&model_path)?;
    let temp = tempfile::Builder::new()
        .prefix("long-embedding-")
        .tempdir()?;
    let store = Arc::new(Store::open(temp.path())?);
    store.update_settings(&json!({
        "embedding_model": model_path,
        "context_size": 512,
        "scan_interval": 0,
    }))?;
    let engine = EmbeddedEngine::new(store.clone());
    let mut vector_dimension = None;
    let contexts = [512usize, DEFAULT_CONTEXT]
        .into_iter()
        .map(|context_size| {
            Ok(json!({
                "context_size": context_size,
                "cases": verify_context(
                    &engine,
                    &store,
                    context_size,
                    &cases,
                    &mut vector_dimension,
                )?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "model": models::EMBEDDING.description,
            "file": models::EMBEDDING.filename,
            "micro_batch": MICRO_BATCH,
            "contexts": contexts,
            "passed": true,
        }))?
    );
    Ok(())
}
