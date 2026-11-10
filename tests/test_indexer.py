from __future__ import annotations

import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from moasearch.extractors import Passage
from moasearch.indexer import Indexer
from moasearch.store import Store


class IndexerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "library"
        self.root.mkdir()
        self.store = Store(self.base / "data")
        self.indexer = Indexer(self.store)
        self.root_id = self.store.add_root(self.root)["id"]
        self.addCleanup(self.indexer.close)

    def scan(self, timeout: float = 5) -> dict:
        self.assertTrue(self.indexer.start())
        deadline = time.monotonic() + timeout
        while self.indexer.status()["running"] and time.monotonic() < deadline:
            time.sleep(0.01)
        state = self.indexer.status()
        self.assertFalse(state["running"], "색인 작업이 시간 안에 끝나지 않았습니다")
        return state

    def indexed_rows(self) -> list[dict]:
        with self.store.connect() as db:
            return [dict(row) for row in db.execute("SELECT * FROM files ORDER BY path")]

    def test_incremental_scan_changes_additions_and_deletions(self) -> None:
        file = self.root / "note.txt"
        file.write_text("alpha before", encoding="utf-8")
        self.scan()
        self.assertEqual(self.store.counts()["files"], 1)
        self.assertEqual(self.store.counts()["chunks"], 1)
        initial_id = self.indexed_rows()[0]["id"]

        file.write_text("alpha after", encoding="utf-8")
        stat = file.stat()
        os.utime(file, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000_000))
        state = self.scan()
        self.assertEqual(state["indexed"], 1)
        with self.store.connect() as db:
            chunk = db.execute("SELECT text FROM chunks").fetchone()[0]
            self.assertEqual(db.execute("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'after'").fetchone()[0], 1)
        self.assertEqual(chunk, "alpha after")
        self.assertEqual(self.indexed_rows()[0]["id"], initial_id)

        file.unlink()
        self.scan()
        self.assertEqual(self.store.counts(), {"files": 0, "chunks": 0, "vectors": 0})
        with self.store.connect() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM chunks_fts").fetchone()[0], 0)

    def test_unreadable_root_preserves_its_existing_index(self) -> None:
        file = self.root / "keep.txt"
        file.write_text("keep this indexed", encoding="utf-8")
        self.scan()
        indexed = self.store.file_by_path(file)
        moved_root = self.base / "moved-away"
        self.root.rename(moved_root)
        state = self.scan()
        self.assertEqual(self.store.file_by_path(file)["id"], indexed["id"])
        self.assertEqual(self.store.counts()["files"], 1)
        self.assertGreater(state["errors"], 0)

    def test_cancel_does_not_commit_partial_file_or_prune(self) -> None:
        file = self.root / "slow.txt"
        file.write_text("existing content", encoding="utf-8")
        self.scan()
        indexed_before = self.store.file_by_path(file)["id"]
        file2 = self.root / "blocked.txt"
        file2.write_text("new content", encoding="utf-8")
        entered, release = threading.Event(), threading.Event()

        def slow_extract(path):
            entered.set()
            release.wait(2)
            return [Passage("partial", "본문")]

        with patch("moasearch.indexer.extract", side_effect=slow_extract):
            self.assertTrue(self.indexer.start())
            self.assertTrue(entered.wait(2))
            self.indexer.cancel()
            release.set()
            deadline = time.monotonic() + 5
            while self.indexer.status()["running"] and time.monotonic() < deadline:
                time.sleep(0.01)
        self.assertEqual(self.indexer.status()["phase"], "cancelled")
        self.assertEqual(self.store.file_by_path(file)["id"], indexed_before)
        self.assertIsNone(self.store.file_by_path(file2))
        self.assertEqual(self.store.counts()["files"], 1)

    def test_cancel_does_not_prune_previous_index_rows(self) -> None:
        original = self.root / "kept.txt"
        original.write_text("keep", encoding="utf-8")
        self.scan()
        original_id = self.store.file_by_path(original)["id"]
        original.unlink()
        entered, release = threading.Event(), threading.Event()
        (self.root / "blocking.txt").write_text("block", encoding="utf-8")

        def slow_extract(path):
            entered.set()
            release.wait(2)
            return [Passage("block", "본문")]

        with patch("moasearch.indexer.extract", side_effect=slow_extract):
            self.assertTrue(self.indexer.start())
            self.assertTrue(entered.wait(2))
            self.indexer.cancel()
            release.set()
            deadline = time.monotonic() + 5
            while self.indexer.status()["running"] and time.monotonic() < deadline:
                time.sleep(0.01)
        self.assertEqual(self.indexer.status()["phase"], "cancelled")
        self.assertEqual(self.store.file_by_path(original)["id"], original_id)

    def test_excluded_paths_and_binary_metadata(self) -> None:
        (self.root / ".git").mkdir()
        (self.root / ".git" / "ignored.txt").write_text("secret repo", encoding="utf-8")
        (self.root / ".ssh").mkdir()
        (self.root / ".ssh" / "key.txt").write_text("secret key", encoding="utf-8")
        (self.root / ".env.local").write_text("secret env", encoding="utf-8")
        (self.root / "credentials.json").write_text("secret credentials", encoding="utf-8")
        binary = self.root / "archive.bin"
        binary.write_bytes(bytes.fromhex("00 ff 01 fe"))
        self.scan()
        rows = self.indexed_rows()
        self.assertEqual([row["name"] for row in rows], ["archive.bin"])
        self.assertEqual(rows[0]["status"], "metadata")
        self.assertEqual(self.store.counts()["chunks"], 0)

    def test_same_and_nested_roots_are_rejected(self) -> None:
        with self.assertRaises(ValueError):
            self.store.add_root(self.root)
        child = self.root / "child"
        child.mkdir()
        with self.assertRaises(ValueError):
            self.store.add_root(child)
        self.store.remove_root(self.root_id)
        self.store.add_root(child)
        with self.assertRaises(ValueError):
            self.store.add_root(self.root)

    def test_raising_max_file_size_retries_previously_metadata_only_text(self) -> None:
        file = self.root / "large.txt"
        file.write_text("large searchable text " * 48_000, encoding="utf-8")
        self.store.update_settings({"max_file_mb": 1})
        self.scan()
        self.assertEqual(self.store.file_by_path(file)["status"], "metadata")

        self.store.update_settings({"max_file_mb": 2})
        self.scan()
        self.assertEqual(self.store.file_by_path(file)["status"], "content")
        self.assertGreater(self.store.counts()["chunks"], 0)


if __name__ == "__main__":
    unittest.main()
