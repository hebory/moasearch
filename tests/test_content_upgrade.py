"""Format-upgrade indexing: existing indexes migrate without destructive rebuilds."""

import os
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest
from unittest.mock import patch

import numpy as np

from moasearch.extractors import Passage
from moasearch.indexer import Indexer
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


if __name__ == "__main__":
    unittest.main()
