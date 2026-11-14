//! Measures the pinned EmbeddingGemma 2 encoder at several llama.cpp thread
//! counts using only synthetic text. No indexed files or application database
//! are opened by this example.
use anyhow::{Context, Result, ensure};
use llama_cpp_2::{
    context::params::{LlamaContextParams, LlamaPoolingType},
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{LlamaModel, params::LlamaModelParams},
};
use moasearch::{
    config,
    models::{self, ModelKind},
};
use serde_json::{Value, json};
use std::{num::NonZeroU32, path::PathBuf, time::Instant};

const CONTEXT: u32 = 4096;
const THREAD_COUNTS: [i32; 4] = [4, 8, 12, 16];
const REPETITIONS: usize = 2;

struct Sample {
    name: &'static str,
    text: String,
    tokens: Vec<llama_cpp_2::token::LlamaToken>,
}

fn normal_korean() -> String {
    let mut text = String::with_capacity(4096);
    for index in 0..100 {
        use std::fmt::Write as _;
        let _ = write!(
            &mut text,
            "문서 {index:03}에서 계약 내용과 회의 일정을 확인합니다. 모아검색은 파일 본문을 읽고 필요한 정보를 빠르게 찾습니다. "
        );
    }
    text.chars().take(1200).collect()
}

fn rare_hangul() -> String {
    let mut seed = 0x4d6f_6153_u32;
    (0..1200)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from_u32(0xAC00 + seed % 11_172).expect("valid Hangul syllable")
        })
        .collect()
}

fn prepare_samples(model: &LlamaModel) -> Result<Vec<Sample>> {
    let vocab = model.vocab();
    let inputs = [
        ("korean_words_1200_chars", normal_korean()),
        ("rare_hangul_1200_chars", rare_hangul()),
    ];
    inputs
        .into_iter()
        .map(|(name, text)| {
            ensure!(
                text.chars().count() == 1200,
                "{name}: 문자가 1200개가 아닙니다"
            );
            let formatted = format!("title: none | text: {text}");
            let tokens = vocab.tokenize(formatted.as_bytes(), true, true);
            ensure!(
                !tokens.is_empty() && tokens.len() <= (CONTEXT - 2) as usize,
                "{name}: 입력 토큰 수가 컨텍스트 한도를 벗어났습니다: {}",
                tokens.len()
            );
            Ok(Sample { name, text, tokens })
        })
        .collect()
}

fn one_embedding(
    model: &LlamaModel,
    backend: &LlamaBackend,
    sample: &Sample,
    threads: i32,
) -> Result<(std::time::Duration, Vec<f32>)> {
    let capacity = u32::try_from(sample.tokens.len())?;
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(CONTEXT))
        .with_n_batch(capacity)
        .with_n_ubatch(capacity)
        .with_embeddings(true)
        .with_pooling_type(LlamaPoolingType::Mean)
        .with_n_threads(threads)
        .with_n_threads_batch(threads);
    let mut context = model
        .new_context(backend, params)
        .context("임베딩 컨텍스트 생성 실패")?;
    let mut batch = LlamaBatch::new(sample.tokens.len(), 1);
    batch.add_sequence(&sample.tokens, 0, false)?;

    let start = Instant::now();
    context.encode(&mut batch).context("임베딩 encode 실패")?;
    let vector = context
        .embeddings_seq_ith(0)
        .context("임베딩 벡터 동기화/조회 실패")?
        .to_vec();
    Ok((start.elapsed(), vector))
}

fn cosine(a: &[f32], b: &[f32]) -> Result<f64> {
    ensure!(
        a.len() == b.len() && !a.is_empty(),
        "벡터 차원이 일치하지 않습니다"
    );
    ensure!(
        a.iter().chain(b).all(|value| value.is_finite()),
        "벡터에 NaN 또는 무한대가 있습니다"
    );
    let dot = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum::<f64>();
    let norm_a = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let norm_b = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    ensure!(norm_a > 0.0 && norm_b > 0.0, "임베딩 벡터 norm이 0입니다");
    Ok(dot / (norm_a * norm_b))
}

fn run_sample(model: &LlamaModel, backend: &LlamaBackend, sample: &Sample) -> Result<Value> {
    let mut baselines = Vec::new();
    let mut measurements = Vec::new();
    for threads in THREAD_COUNTS {
        // One unmeasured run warms the model's kernels for each thread setting.
        let (_, warm) = one_embedding(model, backend, sample, threads)?;
        ensure!(
            warm.len() == 768,
            "예상 임베딩 차원 768과 다릅니다: {}",
            warm.len()
        );
        ensure!(
            warm.iter().all(|value| value.is_finite()),
            "warm-up 벡터가 유한하지 않습니다"
        );
        let mut elapsed_ms = Vec::with_capacity(REPETITIONS);
        let mut vectors = Vec::with_capacity(REPETITIONS);
        for _ in 0..REPETITIONS {
            let (elapsed, vector) = one_embedding(model, backend, sample, threads)?;
            ensure!(
                vector.len() == 768,
                "예상 임베딩 차원 768과 다릅니다: {}",
                vector.len()
            );
            ensure!(
                vector.iter().all(|value| value.is_finite()),
                "벡터가 유한하지 않습니다"
            );
            elapsed_ms.push(elapsed.as_secs_f64() * 1000.0);
            vectors.push(vector);
        }
        if threads == 4 {
            baselines = vectors.clone();
        }
        let similarities = if threads == 4 {
            vec![1.0; vectors.len()]
        } else {
            vectors
                .iter()
                .zip(&baselines)
                .map(|(vector, baseline)| cosine(vector, baseline))
                .collect::<Result<Vec<_>>>()?
        };
        ensure!(
            similarities.iter().all(|similarity| *similarity >= 0.999),
            "{threads} thread 벡터가 4 thread 기준과 다릅니다: {similarities:?}"
        );
        measurements.push(json!({
            "threads": threads,
            "warmup": true,
            "encode_and_vector_sync_ms": elapsed_ms,
            "mean_ms": elapsed_ms.iter().sum::<f64>() / elapsed_ms.len() as f64,
            "vector_dim": 768,
            "cosine_vs_4_threads": similarities,
        }));
    }
    Ok(json!({
        "name": sample.name,
        "source_chars": sample.text.chars().count(),
        "input_tokens": sample.tokens.len(),
        "context_capacity": CONTEXT,
        "measurements": measurements,
    }))
}

fn main() -> Result<()> {
    let model_arg = std::env::args_os().nth(1).map(PathBuf::from);
    let model_path = model_arg
        .or_else(|| std::env::var_os("MOASEARCH_EMBEDDING_MODEL").map(PathBuf::from))
        .context("모델 경로 인자 또는 MOASEARCH_EMBEDDING_MODEL을 지정하세요")?;
    let model_path = config::canonical(&model_path)?;
    models::verify_model(&model_path, ModelKind::Embedding)?;

    let mut backend = LlamaBackend::init().context("llama.cpp backend 초기화 실패")?;
    backend.void_logs();
    let model = LlamaModel::load_from_file(&backend, &model_path, &LlamaModelParams::default())
        .context("고정 EmbeddingGemma 2 모델 로드 실패")?;
    let samples = prepare_samples(&model)?;
    let report = samples
        .iter()
        .map(|sample| run_sample(&model, &backend, sample))
        .collect::<Result<Vec<_>>>()?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "model": models::EMBEDDING.filename,
            "model_sha256": models::EMBEDDING.sha256,
            "threads": THREAD_COUNTS,
            "repetitions_after_warmup": REPETITIONS,
            "passed": true,
            "samples": report,
        }))?
    );
    Ok(())
}
