"""Explicit real-model smoke test; indexes only committed fictional examples."""

import argparse
from pathlib import Path
import time

from moasearch.engine import EmbeddedEngine
from moasearch.indexer import Indexer
from moasearch.search import Search
from moasearch.store import Store


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--skip-answer", action="store_true")
    args = parser.parse_args()
    store = Store(args.data_dir)
    fixture = Path(__file__).resolve().parents[1] / "examples" / "documents"
    if str(fixture) not in {root["path"] for root in store.roots()}:
        store.add_root(str(fixture))
    engine = EmbeddedEngine(store)
    try:
        indexer = Indexer(store, engine)
        started = time.monotonic()
        indexer.start()
        while indexer.status()["running"]:
            time.sleep(.2)
        assert not indexer.status()["errors"], indexer.status()
        print(f"색인 완료: {store.counts()}, {time.monotonic() - started:.1f}초", flush=True)
        search = Search(store, engine)
        for query, expected in [("이사 나가면 맡긴 돈을 언제 돌려받나요", "임대차_메모.txt"),
                                ("가족과 바닷가에서 쉬려고 해", "여행_계획.txt"),
                                ("서비스 업데이트가 실패했을 때 복구하는 절차", "개발_회의.md")]:
            result = search.search(query, limit=3)
            assert result["mode"] == "hybrid", result["warnings"]
            assert result["results"] and result["results"][0]["name"] == expected, result
            print(f"의미 검색 통과: {query} -> {expected}", flush=True)
        if not args.skip_answer:
            result = search.answer("보증금은 언제 반환되나요?", limit=2)
            assert "14" in result["answer"] and "[1]" in result["answer"] and result["sources"], result
            assert "<think>" not in result["answer"], result
            print("실제 LLM 답변:", result["answer"], flush=True)
    finally:
        engine.close()


if __name__ == "__main__":
    main()

