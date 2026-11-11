use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use moasearch::{extract, ocr};

fn fixture(name: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .ancestors()
        .find(|candidate| candidate.join("tests").join("fixtures").is_dir())
        .expect("tests/fixtures should be included in this checkout");
    root.join("tests").join("fixtures").join(name)
}

fn extract_file(path: &Path) -> anyhow::Result<Vec<extract::Passage>> {
    extract::extract_with_models(
        path,
        200_000,
        &AtomicBool::new(false),
        &|_, _, _| {},
        &model_dir(),
    )
}

fn model_dir() -> PathBuf {
    std::env::var_os("MOASEARCH_OCR_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| moasearch::config::data_directory().join("models"))
}

#[test]
fn extracts_hwp5_and_hwpx_body_and_table_text() {
    let hwp =
        extract_file(&fixture("hwp-aurora.hwp")).expect("HWP5 file header and body should parse");
    assert!(
        hwp.iter().any(|p| p.text.contains("오로라-3147")),
        "{hwp:?}"
    );
    assert!(hwp.iter().any(|p| p.location == "섹션 1"), "{hwp:?}");

    let hwpx = extract_file(&fixture("hwpx-cyan.hwpx")).expect("HWPX section should parse");
    let body = hwpx
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(body.contains("청록-9281"), "{hwpx:?}");
    assert!(body.contains("나침반-6602"), "{hwpx:?}");
}

#[test]
fn extracts_digital_pdf_text_layer() {
    use lopdf::{
        Document, Object, Stream,
        content::{Content, Operation},
        dictionary,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("digital.pdf");
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id=doc.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica","Encoding"=>"WinAnsiEncoding"});
    let resources_id = doc.add_object(dictionary! {"Font"=>dictionary! {"F1"=>font_id}});
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 18.into()]),
            Operation::new("Td", vec![35.into(), 300.into()]),
            Operation::new(
                "Tj",
                vec![Object::string_literal(
                    "Cover: fictional rental record for offline search verification only.",
                )],
            ),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
    let page_id = doc.add_object(
        dictionary! {"Type"=>"Page","Parent"=>pages_id,"Contents"=>content_id,
        "Resources"=>resources_id,"MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()]},
    );
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {"Type"=>"Pages","Kids"=>vec![page_id.into()],"Count"=>1}),
    );
    let catalog_id = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", catalog_id);
    doc.save(&path).unwrap();
    let passages = extract_file(&path).expect("digital PDF text layer should extract");
    assert!(
        passages
            .iter()
            .any(|p| p.text.contains("fictional rental record")),
        "{passages:?}"
    );
}

#[test]
#[ignore = "Requires the pinned PP-OCRv5 model files in MOASEARCH_OCR_MODELS"]
fn oar_ocr_reads_korean_image_and_scanned_and_mixed_pdf_pages() {
    let status = ocr::status(&model_dir());
    assert!(
        status["ready"].as_bool().unwrap_or(false),
        "Install the pinned PP-OCRv5 models: {status}"
    );

    let image = extract_file(&fixture("receipt.png")).expect("receipt OCR should succeed");
    let text = image
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("출장") || text.contains("정산"), "{image:?}");
    assert!(
        text.contains("MOA") || text.contains("2026"),
        "English letters and digits should also be read by the Korean recognizer: {image:?}"
    );

    let scanned = extract_file(&fixture("meeting.pdf")).expect("scanned PDF OCR should succeed");
    let text = scanned
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("서비스") || text.contains("회의"),
        "{scanned:?}"
    );
    assert!(
        scanned.iter().any(|p| p.location.contains("OCR")),
        "{scanned:?}"
    );

    let mixed = extract_file(&fixture("rental.pdf")).expect("mixed PDF OCR should succeed");
    assert!(
        mixed
            .iter()
            .any(|p| p.text.contains("fictional rental record")),
        "{mixed:?}"
    );
    assert!(
        mixed
            .iter()
            .any(|p| p.text.contains("보증금") || p.text.contains("LEASE-7341")),
        "{mixed:?}"
    );
    assert!(
        mixed.iter().any(|p| p.location.contains("OCR")),
        "{mixed:?}"
    );
}
