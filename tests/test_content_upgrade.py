"""Format-upgrade indexing: existing indexes migrate without destructive rebuilds."""

import os
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
from unittest.mock import patch

import numpy as np

from moasearch.extractors import ExtractionError, Passage
from moasearch.indexer import Indexer, reader_id
from moasearch.search import Search
from moasearch.store import Store


class FakeEngine:
    def model_id(self):
        return "model-v1"

    def embed(self, texts, query=False):
        return [np.array([1, 0], dtype=np.float32).tobytes() for _ in texts]


class ContentUpgradeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.library = self.base / "library"
        self.library.mkdir()
        self.data = self.base / "data"
        self.store = Store(self.data)
        self.root_id = self.store.add_root(self.library)["id"]
        self.indexer = Indexer(self.store, FakeEngine())
        self.addCleanup(self.indexer.close)

    def scan(self):
        self.assertTrue(self.indexer.start())
        deadline = time.monotonic() + 5
        while self.indexer.status()["running"] and time.monotonic() < deadline:
            time.sleep(.01)
        status = self.indexer.status()
        self.assertFalse(status["running"], status)
        return status

    def legacy_database(self):
        data = self.base / "legacy-data"
        data.mkdir()
        db = sqlite3.connect(data / "index.sqlite3")
        db.executescript("""
            CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE roots (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL);
            CREATE TABLE files (
                id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL,
                path TEXT UNIQUE NOT NULL, name TEXT NOT NULL, extension TEXT NOT NULL,
                size INTEGER NOT NULL, modified INTEGER NOT NULL, status TEXT NOT NULL,
                error TEXT, model_id TEXT);
            CREATE TABLE chunks (
                id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL,
                text TEXT NOT NULL, location TEXT NOT NULL, vector BLOB, model_id TEXT);
            CREATE INDEX files_root ON files(root_id);
            CREATE INDEX chunks_file ON chunks(file_id);
            CREATE VIRTUAL TABLE chunks_fts USING fts5(text, content='chunks', content_rowid='id', tokenize='trigram');
            CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
                INSERT INTO chunks_fts(rowid,text) VALUES(new.id,new.text);
            END;
            CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts,rowid,text) VALUES('delete',old.id,old.text);
            END;
        """)
        db.execute("INSERT INTO roots VALUES (1,?)", (str(self.library),))
        db.execute("INSERT INTO files VALUES (1,1,?,?,?,?,?,'metadata',NULL,NULL)",
                   (str(self.library / "prior.txt"), "prior.txt", ".txt", 4, 12))
        db.commit()
        db.close()

    def test_old_database_migration_preserves_rows_and_adds_legacy_reader(self):
        self.legacy_database()
        upgraded = Store(self.base / "legacy-data")
        row = upgraded.file_by_path(self.library / "prior.txt")
        self.assertEqual(row["id"], 1)
        self.assertEqual(row["reader_id"], "legacy-v1")
        with upgraded.connect() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM roots").fetchone()[0], 1)
            self.assertEqual(db.execute("SELECT count(*) FROM files").fetchone()[0], 1)

    def test_legacy_image_and_hwp_metadata_refresh_to_searchable_text(self):
        cases = [
            ("receipt.png", "ocr-ppocrv5-ko-v1", "중식 결제 내역"),
            ("agreement.hwp", "hwp-v1", "보증금은 14일 이내 반환"),
            ("memo.hwpx", "hwpx-v1", "회의는 다음 주 화요일"),
        ]
        paths = []
        for name, _version, _text in cases:
            path = self.library / name
            path.write_bytes(b"fixture")
            paths.append(path)
            self.store.replace_file(self.root_id, path, path.stat(), [], reader_id="legacy-v1")

        text_by_path = {str(path): case[2] for path, case in zip(paths, cases)}
        def new_reader(path, **kwargs):
            return [Passage(text_by_path[str(path)], "본문")]

        with patch("moasearch.indexer.extract", side_effect=new_reader) as read:
            status = self.scan()
        self.assertEqual(status["indexed"], 3)
        self.assertEqual(read.call_count, 3)
        for (name, version, expected), path in zip(cases, paths):
            indexed = self.store.file_by_path(path)
            self.assertEqual(indexed["reader_id"], version)
            self.assertEqual(indexed["status"], "content")
            result = Search(self.store, self.indexer.engine).search(expected, mode="hybrid")
            self.assertEqual(result["mode"], "hybrid")
            self.assertEqual(result["results"][0]["name"], name)

    def test_empty_image_is_not_ocr_processed_again_after_a_successful_scan(self):
        image = self.library / "blank.png"
        image.write_bytes(b"pixels")
        with patch("moasearch.indexer.extract", return_value=[]) as read:
            self.scan()
            self.assertEqual(self.store.file_by_path(image)["status"], "metadata")
            self.assertEqual(self.store.file_by_path(image)["reader_id"], "ocr-ppocrv5-ko-v1")
            self.scan()
        self.assertEqual(read.call_count, 1)

    def test_previous_ocr_error_is_retried_after_models_become_available(self):
        image = self.library / "card.jpg"
        image.write_bytes(b"pixels")
        with patch("moasearch.indexer.extract", side_effect=RuntimeError("OCR 모델이 없습니다")):
            first = self.scan()
        self.assertEqual(first["errors"], 1)
        self.assertEqual(self.store.file_by_path(image)["status"], "error")
        with patch("moasearch.indexer.extract", return_value=[Passage("승차권 출발 09:30", "이미지 OCR")]) as read:
            second = self.scan()
        self.assertEqual(second["errors"], 0)
        self.assertEqual(read.call_count, 1)
        self.assertEqual(self.store.file_by_path(image)["status"], "content")
        self.assertEqual(self.store.file_by_path(image)["reader_id"], "ocr-ppocrv5-ko-v1")

    def test_reader_does_not_retry_unchanged_empty_image(self):
        image = self.library / "scan.tif"
        image.write_bytes(b"pixels")
        with patch("moasearch.indexer.extract", return_value=[]) as read:
            self.scan()
            self.scan()
        self.assertEqual(read.call_count, 1)

    def test_raising_size_limit_retries_an_oversized_file(self):
        image = self.library / "large.png"
        image.write_bytes(b"x" * (1024 * 1024 + 1))
        self.store.update_settings({"max_file_mb": 1})
        self.scan()
        row = self.store.file_by_path(image)
        self.assertEqual(row["status"], "metadata")
        self.assertEqual(row["reader_id"], "size-limit-v1:1:ocr-ppocrv5-ko-v1")

        self.store.update_settings({"max_file_mb": 2})
        with patch("moasearch.indexer.extract", return_value=[Passage("큰 이미지 영수증", "이미지 OCR")]):
            self.scan()
        self.assertEqual(self.store.file_by_path(image)["status"], "content")

    def test_old_metadata_only_text_is_read_after_upgrade(self):
        path = self.library / "prior.txt"
        path.write_text("출장 경비 정산은 금요일까지", encoding="utf-8")
        self.store.replace_file(self.root_id, path, path.stat(), [], reader_id="legacy-v1")
        self.scan()
        self.assertEqual(self.store.file_by_path(path)["status"], "content")
        self.assertEqual(self.scan()["skipped"], 1)

    def test_old_empty_text_is_retried_only_once(self):
        path = self.library / "empty.txt"
        path.write_text("", encoding="utf-8")
        self.store.replace_file(self.root_id, path, path.stat(), [], reader_id="legacy-v1")
        with patch("moasearch.indexer.extract", return_value=[]) as read:
            self.scan()
            self.scan()
        self.assertEqual(read.call_count, 1)
        self.assertEqual(self.store.file_by_path(path)["reader_id"], "empty-v1:legacy-v1")

    def test_pdf_reader_upgrade_reindexes_old_content_and_metadata(self):
        cases = [
            ("searchable.pdf", "content", [Passage("이전 PDF 내용", "1페이지")]),
            ("scanned.pdf", "metadata", []),
        ]
        paths = []
        for name, status, passages in cases:
            path = self.library / name
            path.write_bytes(b"pdf")
            paths.append(path)
            self.store.replace_file(self.root_id, path, path.stat(), passages, reader_id="legacy-v1")
            if status == "metadata":
                # Metadata may be from a legacy scan that did not extract PDF text.
                with self.store.connect() as db:
                    db.execute("DELETE FROM chunks WHERE file_id=?",
                               (self.store.file_by_path(path)["id"],))
                    db.execute("UPDATE files SET status='metadata' WHERE path=?", (str(path),))

        text_by_path = {
            str(paths[0]): [Passage("계약서 PDF 본문", "1페이지")],
            str(paths[1]): [Passage("스캔 PDF 예약 번호 PDF-7319", "2페이지 OCR")],
        }
        observed = []
        def new_reader(path, **kwargs):
            self.assertIn("model_dir", kwargs)
            self.assertTrue(callable(kwargs["cancelled"]))
            self.assertTrue(callable(kwargs["progress"]))
            kwargs["progress"](2, 3, "pdf_ocr")
            observed.append(self.indexer.status().copy())
            return text_by_path[str(path)]
        with patch("moasearch.indexer.extract", side_effect=new_reader) as read:
            status = self.scan()
        self.assertEqual(status["indexed"], 2)
        self.assertEqual(read.call_count, 2)
        self.assertTrue(all(self.store.file_by_path(path)["reader_id"] == "pdf-ocr-v1" for path in paths))
        self.assertEqual(observed[-1]["phase"], "pdf_ocr")
        self.assertEqual(observed[-1]["current_page"], 2)
        self.assertEqual(observed[-1]["total_pages"], 3)
        self.assertEqual(status["current_page"], 0)
        self.assertEqual(status["total_pages"], 0)
        found = Search(self.store).search("PDF-7319", mode="keyword")["results"]
        self.assertEqual(found[0]["name"], "scanned.pdf")
        self.assertEqual(self.scan()["skipped"], 2)

    def test_pdf_partial_passages_are_searchable_and_missing_ocr_retries(self):
        pdf = self.library / "mixed.pdf"
        pdf.write_bytes(b"pdf")
        self.store.replace_file(self.root_id, pdf, pdf.stat(),
                                [Passage("기존 텍스트층 유지 내용", "1페이지")], reader_id="legacy-v1")
        partial = [Passage("2페이지 스캔에서 읽힌 주문번호 RCT-5082", "2페이지 OCR")]
        with patch("moasearch.indexer.extract", side_effect=ExtractionError("OCR 모델이 없습니다.", partial)):
            first = self.scan()
        row = self.store.file_by_path(pdf)
        self.assertEqual(first["errors"], 1)
        self.assertEqual(row["status"], "error")
        self.assertEqual(row["reader_id"], "pdf-ocr-v1")
        indexed = self.store.file(row["id"])
        texts = [passage["text"] for passage in indexed["passages"]]
        self.assertEqual(texts, ["2페이지 스캔에서 읽힌 주문번호 RCT-5082"])
        self.assertEqual(Search(self.store).search("RCT-5082", mode="keyword")["results"][0]["name"], "mixed.pdf")

        with patch("moasearch.indexer.extract", return_value=[
            Passage("1페이지 기존 텍스트층 유지 내용", "1페이지"), *partial,
        ]) as read:
            second = self.scan()
        self.assertEqual(second["errors"], 0)
        self.assertEqual(read.call_count, 1)
        refreshed = self.store.file_by_path(pdf)
        self.assertEqual(refreshed["status"], "content")
        self.assertEqual(len(self.store.file(refreshed["id"])["passages"]), 2)

    def test_pdf_progress_state_resets_when_cancelled_and_does_not_commit_partial_text(self):
        pdf = self.library / "long.pdf"
        pdf.write_bytes(b"pdf")
        old = [Passage("기존 PDF 색인", "1페이지")]
        self.store.replace_file(self.root_id, pdf, pdf.stat(), old, reader_id="legacy-v1")

        def partial_then_cancel(path, **kwargs):
            kwargs["progress"](4, 9, "pdf_ocr")
            self.assertEqual(self.indexer.status()["current_page"], 4)
            self.assertEqual(self.indexer.status()["total_pages"], 9)
            self.indexer.cancel()
            raise InterruptedError("사용자가 PDF 색인을 취소했습니다.")

        with patch("moasearch.indexer.extract", side_effect=partial_then_cancel):
            status = self.scan()
        self.assertEqual(status["phase"], "cancelled")
        self.assertEqual(status["errors"], 0)
        self.assertEqual(status["current_page"], 0)
        self.assertEqual(status["total_pages"], 0)
        row = self.store.file_by_path(pdf)
        self.assertEqual(row["reader_id"], "legacy-v1")
        indexed = self.store.file(row["id"])
        self.assertEqual([p["text"] for p in indexed["passages"]], ["기존 PDF 색인"])


if __name__ == "__main__":
    unittest.main()
