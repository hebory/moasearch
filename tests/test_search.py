import tempfile
from pathlib import Path
import unittest

import numpy as np

from moasearch.store import Store
from moasearch.extractors import Passage
from moasearch.search import Search


class FakeEngine:
    def model_id(self):
        return "test-model"

    def embed(self, texts, query=False):
        return [np.array([1, 0], dtype=np.float32).tobytes() for _ in texts]


class SearchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.folder = self.base / "documents"
        self.folder.mkdir()
        self.store = Store(self.base / "index")
        self.root = self.store.add_root(str(self.folder))

    def add(self, name, text, vector=None, model=None):
        path = self.folder / name
        path.write_text(text, encoding="utf-8")
        self.store.replace_file(self.root["id"], path, path.stat(), [Passage(text, "3페이지")],
                                [np.array(vector, dtype=np.float32).tobytes()] if vector else None, model)

    def test_korean_substrings_and_punctuation_are_searchable(self):
        self.add("프로젝트.txt", "계약서에는 보증금 반환 기한이 명시되어 있습니다. 오류 ERR-42")
        search = Search(self.store)
        for query in ["보증금", "계약", '" *', "ERR-42"]:
            result = search.search(query, mode="keyword")
            if query == '" *':
                self.assertEqual(result["results"], [])
            else:
                self.assertEqual(len(result["results"]), 1)
                self.assertEqual(result["results"][0]["location"], "3페이지")

    def test_semantic_search_without_shared_words_and_model_isolation(self):
        self.add("first.txt", "바닷가로 떠났던 기록", [1, 0], "test-model")
        self.add("second.txt", "오래된 모델 문서", [1, 0], "old-model")
        self.add("third.md", "프로그램 개발 노트", [0, 1], "test-model")
        result = Search(self.store, FakeEngine()).search("휴가", extension="txt")
        self.assertEqual(result["mode"], "hybrid")
        self.assertEqual([r["name"] for r in result["results"]], ["first.txt"])

    def test_unavailable_engine_keeps_keywords(self):
        self.add("보고서.txt", "매출 전망 분석")
        result = Search(self.store).search("매출")
        self.assertEqual(result["mode"], "keyword")
        self.assertTrue(result["warnings"])
        self.assertEqual(len(result["results"]), 1)

    def test_chunk_count_does_not_multiply_file_rank(self):
        self.add("memo.txt", "회의 결과")
        path = self.folder / "long.txt"
        path.write_text("회의 결과" * 10, encoding="utf-8")
        self.store.replace_file(self.root["id"], path, path.stat(), [Passage("회의 결과", "본문")] * 20)
        result = Search(self.store).search("회의", mode="keyword")
        self.assertEqual(len(result["results"]), 2)
        self.assertLess(max(r["score"] for r in result["results"]), .03)

