from __future__ import annotations

from contextlib import contextmanager
import tempfile
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

import numpy as np

from moasearch.extractors import Passage
from moasearch.search import Search
from moasearch.store import Store


class FakeEngine:
    def __init__(self, model_id="test-model", vector=(1, 0)):
        self.current_model = model_id
        self.current_vector = vector
        self.embeds = 0

    def model_id(self):
        return self.current_model

    def embed(self, texts, query=False):
        self.embeds += 1
        return [np.asarray(self.current_vector, dtype=np.float32).tobytes() for _ in texts]


class SearchPerformanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.library = self.base / "documents"
        self.library.mkdir()
        self.store = Store(self.base / "data")
        self.root_id = self.store.add_root(self.library)["id"]
        self.engine = FakeEngine()
        self.search = Search(self.store, self.engine)

    def add_file(self, name, vectors, text="unique body text"):
        path = self.library / name
        path.write_text(text, encoding="utf-8")
        vectors = [np.asarray(vector, dtype=np.float32).tobytes() for vector in vectors]
        passages = [Passage(text + f" {i}", "본문") for i in range(len(vectors))]
        self.store.replace_file(self.root_id, path, path.stat(), passages, vectors, "test-model")
        return path

    def test_warm_matrix_and_query_cache_do_not_read_vector_blobs_again(self):
        for index in range(20):
            self.add_file(f"file-{index:03}.txt", [(1, index / 100 + .1)])
        statements = []
        original_connect = self.store.connect

        @contextmanager
        def traced_connect():
            with original_connect() as db:
                db.set_trace_callback(lambda sql: statements.append(sql.casefold()))
                yield db

        with patch.object(self.store, "connect", side_effect=traced_connect):
            first = self.search.search("query not present in corpus")
            self.assertEqual(first["mode"], "hybrid")
            statements.clear()
            second = self.search.search("query not present in corpus")
        self.assertEqual(len(first["results"]), 20)
        self.assertEqual(len(second["results"]), 20)
        self.assertEqual(self.engine.embeds, 1)
        self.assertFalse(any("select c.id as chunk_id,c.file_id,f.extension,c.vector" in sql
                             or "select c.id as chunk_id,c.file_id,c.vector" in sql
                             for sql in statements))
        self.assertEqual(self.search._vectors.snapshot.revision, self.store.index_state()["revision"])

    def test_file_replacement_and_raw_sql_deletion_invalidate_cached_revision(self):
        first = self.add_file("a.txt", [(1, 0)])
        second = self.add_file("b.txt", [(.8, .2)])
        query = "unlistedsemanticprobe"
        result = self.search.search(query)
        self.assertEqual(result["results"][0]["name"], "a.txt")
        old_revision = self.search._vectors.snapshot.revision

        first.write_text("updated", encoding="utf-8")
        self.store.replace_file(self.root_id, first, first.stat(), [Passage("new body", "본문")],
                                [np.asarray((0, 1), dtype=np.float32).tobytes()], "test-model")
        changed = self.search.search(query)
        self.assertEqual(changed["results"][0]["name"], "b.txt")
        self.assertGreater(self.search._vectors.snapshot.revision, old_revision)

        file_id = self.store.file_by_path(second)["id"]
        with self.store.connect() as db:
            db.execute("DELETE FROM chunks WHERE file_id=?", (file_id,))
        deleted = self.search.search(query)
        self.assertEqual([item["name"] for item in deleted["results"]], ["a.txt"])

    def test_extension_filter_is_applied_before_exact_file_ranking(self):
        self.add_file("excellent.txt", [(1, 0)])
        self.add_file("scan.pdf", [(-1, 0)])
        result = self.search.search("semantic only words", extension="pdf")
        self.assertEqual([item["name"] for item in result["results"]], ["scan.pdf"])
        self.assertLess(result["results"][0]["score"], .03)

    def test_long_filename_substrings_use_fts_and_short_substrings_stay_exact(self):
        named = self.add_file("Alpha-contract_notes.txt", [])
        self.add_file("other.txt", [])
        statements = []
        original_connect = self.store.connect

        @contextmanager
        def traced_connect():
            with original_connect() as db:
                db.set_trace_callback(lambda sql: statements.append(sql.casefold()))
                yield db

        with patch.object(self.store, "connect", side_effect=traced_connect):
            long_result = Search(self.store).search("alpha-contract", mode="keyword")
            short_result = Search(self.store).search("ph", mode="keyword")
        self.assertEqual(long_result["results"][0]["path"], str(named))
        self.assertEqual([row["path"] for row in short_result["results"]], [str(named)])
        self.assertTrue(any("files_fts match" in sql for sql in statements))

    def test_cached_and_bounded_streaming_results_preserve_best_negative_chunk(self):
        self.add_file("negative.txt", [(-1, 0), (-.2, -.98)])
        self.add_file("less-negative.md", [(-.8, -.6)])
        self.engine.current_vector = (1, 0)
        cached = Search(self.store, self.engine, vector_cache_bytes=100_000).search("unmatched phrase")
        streamed = Search(self.store, self.engine, vector_cache_bytes=0).search("another unmatched phrase")
        cached_names = [row["name"] for row in cached["results"]]
        streamed_names = [row["name"] for row in streamed["results"]]
        self.assertEqual(cached_names, ["negative.txt", "less-negative.md"])
        self.assertEqual(streamed_names, cached_names)
        self.assertIn("unique body text 1", cached["results"][0]["snippet"])

    def test_streaming_fallback_bounds_file_state_across_batch_boundaries(self):
        # One file spans two vector batches; its best chunk is in the second
        # batch. Other files include exact score ties and negative scores.
        self.add_file("many-chunks.txt", [(0, 1)] * 260 + [(1, 0)])
        for index, vector in enumerate([
            (1, 0), (1, 0), (.8, .6), (.6, .8), (.2, .98),
            (-.1, .995), (-.8, .6), (-1, 0),
        ]):
            self.add_file(f"rank-{index:02}.md", [vector])

        query = np.array([1, 0], dtype=np.float32)
        with self.store.connect() as db:
            revision = self.store.index_state(db=db)["revision"]
            cached = self.search._vectors.get_or_build(db, "test-model", 2, revision)
            expected = cached.candidates(query, file_limit=5)
            actual = Search._streaming_candidates(db, "test-model", query, None, 5)
        self.assertEqual(actual, expected)
        self.assertEqual(len(actual), 5)
        self.assertEqual(actual[0][0], 1.0)
        self.assertIn(actual[0][1], cached.chunk_ids)

    def test_memory_budget_and_bad_vectors_use_exact_fallback_without_crashing(self):
        self.add_file("bad.txt", [(np.nan, 0), (1, 0, 0)])
        self.add_file("good.txt", [(0, 1)])
        query = "no keyword matches these files"
        expected = [row["name"] for row in Search(self.store, self.engine, vector_cache_bytes=100_000)
                    .search(query)["results"]]
        fallback = Search(self.store, self.engine, vector_cache_bytes=0)
        actual = [row["name"] for row in fallback.search(query)["results"]]
        self.assertEqual(actual, expected)
        self.assertNotIn("bad.txt", actual)
        self.assertIsNone(fallback._vectors.snapshot)

    def test_argpartition_keeps_old_stable_order_for_equal_scores(self):
        names = []
        for index in range(65):
            path = self.add_file(f"tie-{index:02}.txt", [(1, 0)])
            names.append(path.name)
        result = self.search.search("unmatchedsemanticquery", limit=1)
        expected = sorted(names, reverse=True)[:60]
        self.assertEqual([row["name"] for row in result["results"]], [expected[0]])
        # The semantic candidate order itself is bounded to three times limit.
        snapshot = self.search._vectors.snapshot
        candidates = snapshot.candidates(np.array([1, 0], dtype=np.float32), file_limit=60)
        self.assertEqual(candidates[0][1], max(snapshot.chunk_ids))

    def test_query_vector_cache_is_separated_by_embedding_model(self):
        self.add_file("memo.txt", [(1, 0)])
        self.search.search("query with no matching word")
        self.engine.current_model = "replacement-model"
        self.engine.current_vector = (0, 1)
        self.search.search("query with no matching word")
        self.assertEqual(self.engine.embeds, 2)
        self.assertEqual(len(self.search._query_vectors), 2)
        self.assertEqual(self.search._vectors.snapshot.model_id, "replacement-model")

    def test_concurrent_queries_build_one_revision_snapshot(self):
        for index in range(16):
            self.add_file(f"many-{index:02}.md", [(1, index / 20)])
        outcomes, failures = [], []
        barrier = threading.Barrier(4)

        def run(query):
            try:
                barrier.wait(timeout=5)
                outcomes.append(self.search.search(query))
            except Exception as exc:
                failures.append(exc)

        threads = [threading.Thread(target=run, args=(f"different query {index}",))
                   for index in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
        self.assertFalse(any(thread.is_alive() for thread in threads))
        self.assertEqual(failures, [])
        self.assertEqual(len(outcomes), 4)
        self.assertEqual(self.search._vectors.snapshot.revision, self.store.index_state()["revision"])

    def test_writer_commit_during_vector_read_uses_one_sqlite_revision(self):
        first = self.add_file("first.txt", [(1, 0)])
        self.add_file("second.txt", [(0, 1)])
        first_revision = self.store.index_state()["revision"]
        snapshot_started, writer_finished = threading.Event(), threading.Event()
        original_build = self.search._vectors._get_or_build
        outcome = []

        def delayed_build(db, model_id, dimension, revision):
            snapshot_started.set()
            self.assertTrue(writer_finished.wait(timeout=5))
            return original_build(db, model_id, dimension, revision)

        def run_search():
            outcome.append(self.search.search("never-matches-any-path"))

        with patch.object(self.search._vectors, "_get_or_build", side_effect=delayed_build):
            worker = threading.Thread(target=run_search)
            worker.start()
            self.assertTrue(snapshot_started.wait(timeout=5))
            first.write_text("changed", encoding="utf-8")
            self.store.replace_file(self.root_id, first, first.stat(),
                                    [Passage("changed", "본문")],
                                    [np.asarray((-1, 0), dtype=np.float32).tobytes()], "test-model")
            writer_finished.set()
            worker.join(timeout=5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(outcome[0]["results"][0]["name"], "first.txt")
        self.assertEqual(self.search._vectors.snapshot.revision, first_revision)
        refreshed = self.search.search("never-matches-any-path")
        self.assertEqual(refreshed["results"][0]["name"], "second.txt")
        self.assertGreater(self.search._vectors.snapshot.revision, first_revision)


if __name__ == "__main__":
    unittest.main()
