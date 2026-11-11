"""Search metadata must follow atomic writes, migration and cascading deletes."""

from pathlib import Path
import tempfile
import unittest

from moasearch.extractors import Passage
from moasearch.store import Store


class StorePerformanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.library = self.base / "library"
        self.library.mkdir()
        self.store = Store(self.base / "data")
        self.root = self.store.add_root(self.library)["id"]

    def record(self, name="문서.txt"):
        path = self.library / name
        path.write_text("가상 본문", encoding="utf-8")
        return dict(root_id=self.root, path=path, stat=path.stat(),
                    passages=[Passage("가상 본문", "본문")])

    def test_counts_revision_and_cascade_match_actual_rows(self):
        self.store.replace_files([self.record("계약서.txt"), self.record("보고서.txt")])
        first = self.store.index_state()
        self.assertEqual(self.store.counts(), dict(files=2, chunks=2, vectors=0))
        with self.store.connect() as db:
            db.execute("UPDATE chunks SET vector=?,model_id='test'", (b"vector",))
        self.assertGreater(self.store.index_state()["revision"], first["revision"])
        self.assertEqual(self.store.counts()["vectors"], 2)
        self.store.remove_root(self.root)
        self.assertEqual(self.store.counts(), dict(files=0, chunks=0, vectors=0))
        with self.store.connect() as db:
            self.assertFalse(db.execute("SELECT rowid FROM files_fts WHERE files_fts MATCH '계약서'").fetchall())

    def test_failed_batch_rolls_back_files_fts_and_counters_together(self):
        good, bad = self.record("저장검증.txt"), self.record("오류검증.txt")
        bad["passages"] *= 2
        bad["vectors"] = [b"short"]
        before = self.store.index_state()
        with self.assertRaises(IndexError):
            self.store.replace_files([good, bad])
        self.assertEqual(self.store.index_state(), before)
        with self.store.connect() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM files").fetchone()[0], 0)
            self.assertFalse(db.execute("SELECT rowid FROM files_fts WHERE files_fts MATCH '저장검증'").fetchall())

    def test_revision_and_counts_are_not_visible_before_commit(self):
        record = self.record()
        before = self.store.index_state()
        with self.store.connect() as db:
            self.store._replace_file(db, **record)
            self.assertGreater(self.store.index_state(db)["revision"], before["revision"])
            self.assertEqual(self.store.index_state(), before)
        self.assertEqual(self.store.counts()["files"], 1)

    def test_old_index_migration_seeds_counts_and_path_search_once(self):
        self.store.replace_files([self.record("복원검증.txt")])
        with self.store.connect() as db:
            for name in ("files_search_ai", "files_search_ad", "files_search_au", "files_path_au",
                         "chunks_meta_ai", "chunks_meta_ad", "chunks_meta_au"):
                db.execute(f"DROP TRIGGER {name}")
            db.execute("DROP TABLE index_meta")
            db.execute("DROP TABLE files_fts")
        migrated = Store(self.store.data_dir)
        self.assertEqual(migrated.counts(), dict(files=1, chunks=1, vectors=0))
        with migrated.connect() as db:
            self.assertEqual(len(db.execute("SELECT rowid FROM files_fts WHERE files_fts MATCH '복원검증'").fetchall()), 1)
        before = migrated.index_state()
        self.assertEqual(Store(self.store.data_dir).index_state(), before)

    def test_path_update_replaces_fts_terms(self):
        self.store.replace_files([self.record("변경이전.txt")])
        with self.store.connect() as db:
            db.execute("UPDATE files SET path=?", (str(self.library / "변경이후.txt"),))
            self.assertEqual(len(db.execute("SELECT rowid FROM files_fts WHERE files_fts MATCH '변경이후'").fetchall()), 1)
            self.assertFalse(db.execute("SELECT rowid FROM files_fts WHERE files_fts MATCH '변경이전'").fetchall())


if __name__ == "__main__":
    unittest.main()
