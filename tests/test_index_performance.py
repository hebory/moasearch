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


class BatchedIndexingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "library"
        self.root.mkdir()
        self.store = Store(self.base / "data")
        self.root_id = self.store.add_root(self.root)["id"]
        self.indexer = Indexer(self.store)
        self.addCleanup(self.indexer.close)

    def scan(self, timeout=10):
        self.assertTrue(self.indexer.start())
        deadline = time.monotonic() + timeout
        while self.indexer.status()["running"] and time.monotonic() < deadline:
            time.sleep(.01)
        state = self.indexer.status()
        self.assertFalse(state["running"], state)
        return state

    def test_many_small_files_reuse_connection_and_commit_bounded_batches(self):
        count = 96
        for i in range(count):
            (self.root / f"note-{i:04}.txt").write_text(f"searchable note number {i}", encoding="utf-8")
        connect, replace_files = self.store.connect, self.store.replace_files
        with patch.object(self.store, "connect", wraps=connect) as opened, \
             patch.object(self.store, "replace_files", wraps=replace_files) as batches:
            state = self.scan()
        self.assertEqual(state["scanned"], count)
        self.assertEqual(state["indexed"], count)
        self.assertEqual(state["errors"], 0, state)
        # Settings and roots each use one short-lived connection; the file loop
        # itself reuses the worker connection regardless of file count.
        self.assertLessEqual(opened.call_count, 3)
        self.assertEqual(batches.call_count, 3)
        self.assertTrue(all(call.kwargs.get("db") is not None for call in batches.call_args_list))
        self.assertEqual(self.store.counts()["files"], count)
        self.assertEqual(self.store.counts()["chunks"], count)

    def test_batch_failure_is_atomic_and_does_not_publish_partial_file(self):
        first = self.root / "first.txt"
        second = self.root / "second.txt"
        first.write_text("first searchable", encoding="utf-8")
        second.write_text("second searchable", encoding="utf-8")
        real_replace = self.store._replace_file

        def fail_second(db, **record):
            if record["path"] == second:
                raise OSError("simulated batch failure")
            return real_replace(db, **record)

        with patch("moasearch.indexer._BATCH_FILES", 2), \
             patch.object(self.store, "_replace_file", side_effect=fail_second):
            state = self.scan()
        self.assertEqual(state["phase"], "error")
        self.assertEqual(state["indexed"], 0)
        self.assertIsNone(self.store.file_by_path(first))
        self.assertIsNone(self.store.file_by_path(second))
        self.assertEqual(self.store.counts()["files"], 0)

    def test_cancel_commits_completed_batch_but_keeps_existing_rows_without_pruning(self):
        removed = self.root / "removed.txt"
        removed.write_text("previously indexed", encoding="utf-8")
        self.store.replace_file(self.root_id, removed, removed.stat(),
                                [Passage("previously indexed", "본문")])
        removed.unlink()

        first = self.root / "a-complete.txt"
        second = self.root / "b-cancelled.txt"
        first.write_text("first", encoding="utf-8")
        second.write_text("second", encoding="utf-8")
        entered, release = threading.Event(), threading.Event()
        calls = 0

        def slow_second(path):
            nonlocal calls
            calls += 1
            if path == second:
                entered.set()
                release.wait(3)
            return [Passage(path.stem, "본문")]

        def ordered_walk(_root, _failures):
            return iter(((first, first.stat()), (second, second.stat())))

        with patch.object(self.indexer, "_walk", side_effect=ordered_walk), \
             patch("moasearch.indexer.extract", side_effect=slow_second):
            self.assertTrue(self.indexer.start())
            self.assertTrue(entered.wait(3))
            self.indexer.cancel()
            release.set()
            deadline = time.monotonic() + 5
            while self.indexer.status()["running"] and time.monotonic() < deadline:
                time.sleep(.01)

        state = self.indexer.status()
        self.assertEqual(state["phase"], "cancelled")
        self.assertEqual(state["indexed"], 1)
        self.assertIsNotNone(self.store.file_by_path(first))
        self.assertIsNone(self.store.file_by_path(second))
        # The missing path is only pruned after a complete, uncancelled walk.
        self.assertIsNotNone(self.store.file_by_path(removed))
        self.assertEqual(calls, 2)


if __name__ == "__main__":
    unittest.main()
