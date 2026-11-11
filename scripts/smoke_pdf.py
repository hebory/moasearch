"""Exercise scanned/mixed PDFs with real offline OCR and GGUF embeddings."""

import argparse
import io
import json
from pathlib import Path
import shutil
import tempfile
import time
from unittest.mock import patch

from PIL import Image, ImageDraw, ImageFont
from pypdf import PdfReader, PdfWriter
from pypdf.generic import DictionaryObject, NameObject, DecodedStreamObject

from moasearch.engine import EmbeddedEngine
from moasearch.indexer import Indexer
from moasearch.models import verify_ocr_models
from moasearch.search import Search
from moasearch.store import Store


def scan_pdf(path, lines, font, *, cover=False):
    with Image.new("RGB", (1400, 800), "white") as image:
        draw = ImageDraw.Draw(image)
        for index, line in enumerate(lines):
            draw.text((65, 65 + index * 110), line, fill="black", font=font)
        buffer = io.BytesIO()
        image.save(buffer, "PDF", resolution=150)
    scanned = PdfReader(buffer)
    assert not (scanned.pages[0].extract_text() or "").strip(), "Fixture must have no text layer"
    writer = PdfWriter()
    if cover:
        page = writer.add_blank_page(width=672, height=384)
        page[NameObject("/Resources")] = DictionaryObject({NameObject("/Font"): DictionaryObject({
            NameObject("/F1"): DictionaryObject({NameObject("/Type"): NameObject("/Font"),
                NameObject("/Subtype"): NameObject("/Type1"), NameObject("/BaseFont"): NameObject("/Helvetica")})})})
        content = DecodedStreamObject()
        content.set_data(b"BT /F1 18 Tf 35 300 Td (Cover: fictional rental record for offline search verification only.) Tj ET")
        page[NameObject("/Contents")] = content
    writer.add_page(scanned.pages[0])
    writer.write(path)


def main():
    parser = argparse.ArgumentParser(description="가상 스캔 PDF로 실제 로컬 OCR·의미 검색을 검증합니다.")
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--font", type=Path, default=Path("C:/Windows/Fonts/malgun.ttf"))
    parser.add_argument("--output-dir", type=Path, default=Path(".moasearch-pdf-smoke"))
    args = parser.parse_args()
    model_dir = args.model_dir.resolve()
    ocr_paths = verify_ocr_models(model_dir)
    embedding = model_dir / "Qwen3-Embedding-0.6B-Q8_0.gguf"
    if not embedding.is_file():
        parser.error("임베딩 모델을 먼저 내려받아 주세요.")
    if not args.font.is_file():
        parser.error("--font에 한국어를 지원하는 TTF 글꼴 경로를 지정하세요.")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix="run-", dir=args.output_dir.resolve()))
    documents = run_dir / "documents"
    documents.mkdir()
    font = ImageFont.truetype(str(args.font), 46)
    scan_pdf(documents / "rental.pdf", ["주택 임대차 계약서", "계약 종료 후 14일 이내에",
        "임대인은 보증금 전액을 반환합니다.", "계약 번호 LEASE-7341"], font, cover=True)
    scan_pdf(documents / "travel.pdf", ["제주 가족 여행 계획", "숙소는 해변 근처 호텔입니다.",
        "일요일에는 수족관을 방문합니다."], font)
    scan_pdf(documents / "meeting.pdf", ["서비스 개발 회의", "신규 기능 배포 일정은 수요일입니다.",
        "장애 발생 시 이전 버전으로 복구합니다."], font)
    store = Store(run_dir / "data")
    model_target = store.data_dir / "models"
    model_target.mkdir()
    for source in ocr_paths.values():
        shutil.copyfile(source, model_target / source.name)
    store.update_settings({"embedding_model": str(embedding), "scan_interval": 0})
    store.add_root(documents)
    engine = EmbeddedEngine(store)
    indexer = Indexer(store, engine)
    try:
        with patch("requests.sessions.Session.request", side_effect=AssertionError("외부 HTTP 요청")), \
             patch("urllib.request.urlopen", side_effect=AssertionError("외부 URL 요청")), \
             patch("socket.create_connection", side_effect=AssertionError("외부 소켓 연결")):
            started = time.monotonic()
            indexer.start()
            while indexer.status()["running"]:
                if time.monotonic() - started > 180:
                    indexer.cancel()
                    raise TimeoutError("PDF 검증 색인 시간이 180초를 넘었습니다.")
                time.sleep(.1)
            assert indexer.status()["errors"] == 0, indexer.status()
            search = Search(store, engine)
            keyword = search.search("LEASE-7341", mode="keyword")["results"]
            assert keyword and keyword[0]["name"] == "rental.pdf", keyword
            assert keyword[0]["location"] == "2페이지 OCR", keyword
            query = "이사 나가면 맡긴 돈을 언제 돌려받나요"
            semantic = search.search(query)
            assert semantic["mode"] == "hybrid", semantic
            assert semantic["results"][0]["name"] == "rental.pdf", semantic
            assert "semantic" in semantic["results"][0]["match_type"], semantic
            indexer.start()
            while indexer.status()["running"]:
                time.sleep(.1)
            assert indexer.status()["skipped"] == 3, indexer.status()
        report = {"counts": store.counts(), "offline": True, "keyword": keyword,
                  "query": query, "semantic": semantic, "second_scan": indexer.status()}
        output = run_dir / "report.json"
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"스캔·혼합 PDF OCR/키워드/의미 검색/증분 재사용 통과: {output}")
        print(f"미리보기: {keyword[0]['snippet']}")
    finally:
        indexer.close()
        engine.close()


if __name__ == "__main__":
    main()
