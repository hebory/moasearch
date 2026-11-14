//! Opt-in relevance evaluation with the pinned EmbeddingGemma 2 Q8_0 model.
//! Uses fictional passages only and a disposable index, never the user's index.
use anyhow::Result;
use moasearch::{
    config,
    engine::{EmbeddedEngine, Embedder},
    extract::Passage,
    models,
    search::Search,
    store::{FileRecord, Store},
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

fn main() -> Result<()> {
    let models_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".moasearch-rust-dev/models"));
    std::fs::create_dir_all(".moasearch-tests")?;
    let temp = tempfile::Builder::new()
        .prefix("relevance-")
        .tempdir_in(".moasearch-tests")?;
    let store = Arc::new(Store::open(temp.path().join("data"))?);
    store.update_settings(&json!({"embedding_model":config::canonical(&models_dir.join(models::EMBEDDING.filename))?,"scan_interval":0}))?;
    let library = temp.path().join("library");
    std::fs::create_dir_all(&library)?;
    let root_id = store.add_root(&library)?["id"].as_i64().unwrap();
    // HWP/PDF rows represent the already extracted text; extraction has separate format tests.
    let documents = [
        (
            "hwp-aurora.hwp",
            "프로젝트 코드 오로라-3147은 부산 사무실의 파란 서랍에 보관한다.",
        ),
        (
            "hwpx-cyan.hwpx",
            "계약 코드 청록-9281은 서울 사무실 2층 서류함에 보관한다. 표 전용 단어: 나침반-6602",
        ),
        (
            "receipt.png",
            "국내 출장 정산서 교통비 합계 125000원 제출 기한 11월 30일 승인 번호 MOA-2026",
        ),
        (
            "meeting.pdf",
            "서비스 개발 회의 신규 기능 배포 일정은 수요일입니다. 장애 발생 시 이전 버전으로 복구합니다.",
        ),
        (
            "rental.pdf",
            "주택 임대차 계약서 계약 종료 후 14일 이내에 임대인은 보증금 전액을 반환합니다.",
        ),
        (
            "travel.txt",
            "가족과 함께 바닷가에서 쉬는 제주도 여행 계획입니다. 숙소는 해변 근처로 예약합니다.",
        ),
        (
            "recipe.txt",
            "김치찌개 조리법: 돼지고기와 김치를 볶은 뒤 두부와 물을 넣고 끓입니다.",
        ),
        (
            "code-guide.md",
            "소스 코드는 프로그램을 만드는 원본 파일입니다. Rust와 Python 구현은 src 폴더에 있고 Git으로 버전을 관리합니다.",
        ),
        (
            "main.rs",
            "use std::fs; fn main() { let text = fs::read_to_string(\"input.txt\").unwrap(); println!(\"{}\", text); }",
        ),
        (
            "calculator.py",
            "def add(a: int, b: int) -> int:\n    return a + b\n\nif __name__ == '__main__':\n    print(add(2, 3))",
        ),
        (
            "source-code.txt",
            "The source code contains the application implementation, functions and unit tests. Build with cargo and review changes using git diff.",
        ),
        (
            "소스코드_목록.txt",
            "이 파일은 이름으로도 정확하게 찾을 수 있어야 합니다.",
        ),
    ];
    let engine = Arc::new(EmbeddedEngine::new(store.clone()));
    let texts: Vec<String> = documents.iter().map(|(_, text)| text.to_string()).collect();
    let vectors = engine.embed(&texts, false)?;
    let records: Vec<_> = documents
        .iter()
        .zip(&vectors)
        .map(|((name, text), vector)| {
            let path = library.join(name);
            std::fs::write(&path, text).unwrap();
            FileRecord {
                root_id,
                path: config::canonical(&path).unwrap(),
                size: text.len() as i64,
                modified: 1,
                passages: vec![Passage {
                    text: text.to_string(),
                    location: "평가 본문".into(),
                }],
                vectors: Some(vec![vector.clone()]),
                model_id: engine.model_id(),
                error: None,
                reader_id: "relevance-fixture".into(),
            }
        })
        .collect();
    store.replace_files(&records)?;
    let search = Search::new(store, engine.clone());
    let mut report = Vec::new();
    let cases: &[(&str, &[&str])] = &[
        (
            "소스코드",
            &[
                "소스코드_목록.txt",
                "source-code.txt",
                "code-guide.md",
                "calculator.py",
                "main.rs",
            ],
        ),
        (
            "프로그래밍 구현 파일",
            &["code-guide.md", "소스코드_목록.txt"],
        ),
        ("출장 교통비를 청구할 지출 내역", &["receipt.png"]),
        (
            "서비스 배포가 실패하면 이전 버전으로 복구하는 방법",
            &["meeting.pdf"],
        ),
        ("이사 나간 뒤 맡긴 돈을 돌려받는 조건", &["rental.pdf"]),
        ("가족과 바닷가에서 쉬려고 해", &["travel.txt"]),
        ("양자컴퓨터의 큐비트 오류 정정", &[]),
        ("해왕성의 대기 성분", &[]),
    ];
    let mut passed = true;
    for (query, expected) in cases {
        let q = engine.embed(&[query.to_string()], true)?.remove(0);
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mut scores: Vec<_> = documents
            .iter()
            .zip(&vectors)
            .map(|((name, _), v)| {
                let cosine =
                    v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>() / (norm(v) * norm(&q));
                (*name, cosine)
            })
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1));
        let result = search.search(query, 30, None, "hybrid")?;
        let names: Vec<_> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        let matches = if query == &"소스코드" {
            names.len() == expected.len() && expected.iter().all(|name| names.contains(name))
        } else {
            names == *expected
        };
        passed &= matches;
        report.push(json!({"query":query,"raw_cosine":scores,"expected":expected,"passed":matches,"search":result}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"model":models::EMBEDDING.description,"passed":passed,"cases":report})
        )?
    );
    anyhow::ensure!(passed, "관련성 평가 결과가 기대와 다릅니다.");
    Ok(())
}
