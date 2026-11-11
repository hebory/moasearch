"""Reproducible synthetic indexing/search timing; never reads user documents."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import tempfile
import time

import numpy as np

from moasearch.indexer import Indexer
from moasearch.search import Search
from moasearch.store import Store


SEMANTIC_QUERY = "자연어 검색 대상 문서"
SEARCH_MODEL = "benchmark-model-v1"


def query_vector(query: str, dimension: int) -> np.ndarray:
    seed = int.from_bytes(hashlib.sha256(query.encode("utf-8")).digest()[:8], "little")
    rng = np.random.default_rng(seed)
    vector = rng.standard_normal(dimension).astype(np.float32)
    return vector / max(float(np.linalg.norm(vector)), 1e-12)


class DeterministicEngine:
    """Make vector search measurable without loading a real LLM."""

    def __init__(self, dimension):
        self.dimension = dimension

    def model_id(self):
        return SEARCH_MODEL

    def embed(self, texts, query=False):
        return [query_vector(text, self.dimension).tobytes() for text in texts]


def elapsed(call):
    start = time.perf_counter()
    result = call()
    return time.perf_counter() - start, result


def wait_for_scan(indexer, timeout=900):
    if not indexer.start():
        raise RuntimeError("색인기가 이미 실행 중입니다.")
    started = time.perf_counter()
    while indexer.status()["running"]:
        if time.perf_counter() - started > timeout:
            indexer.cancel()
            raise TimeoutError(f"합성 색인이 {timeout}초 안에 끝나지 않았습니다.")
        time.sleep(.025)
    status = indexer.status()
    if status["errors"]:
        raise RuntimeError(f"합성 색인 오류: {status}")
    return status


def percentile95(samples):
    return sorted(samples)[max(0, int(.95 * len(samples) + .999) - 1)]


def make_indexed_files(run_dir, count):
    documents = run_dir / "files"
    documents.mkdir()
    for index in range(count):
        name = "filenameprobe9342_target.txt" if index == 0 else f"memo_{index:07d}.txt"
        text = f"가상 문서 {index} 검색 성능 시험용 본문입니다. 파일 코드는 DOC-{index:07d}."
        if index == 0:
            text += " 색인 단어 PERF-TARGET-9241 포함."
        (documents / name).write_text(text, encoding="utf-8")
    return documents


def seed_vector_store(run_dir, chunk_count, dimension):
    documents = run_dir / "vector_documents"
    documents.mkdir()
    store = Store(run_dir / "vector_data")
    root_id = store.add_root(documents)["id"]
    file_count = (chunk_count + 3) // 4
    query = query_vector(SEMANTIC_QUERY, dimension)
    rng = np.random.default_rng(20261111)
    vectors = rng.standard_normal((chunk_count, dimension), dtype=np.float32)
    norms = np.linalg.norm(vectors, axis=1, keepdims=True)
    vectors /= np.maximum(norms, 1e-12)
    vectors[0] = query

    # Seed one transaction directly so setup cost is separate from query time.
    with store.connect() as db:
        db.executemany(
            "INSERT INTO files(root_id,path,name,extension,size,modified,status,error,model_id,reader_id) "
            "VALUES (?,?,?,?,?,?,?,?,?,?)",
            [(root_id, str(documents / ("filenameprobe9342_target.txt" if i == 0 else f"vector_{i:07d}.txt")),
              "filenameprobe9342_target.txt" if i == 0 else f"vector_{i:07d}.txt", ".txt", 128, i,
              "content", None, SEARCH_MODEL, "legacy-v1") for i in range(file_count)],
        )
        file_ids = [row[0] for row in db.execute("SELECT id FROM files ORDER BY id")]
        texts = []
        for index in range(chunk_count):
            file_id = file_ids[index // 4]
            text = f"가상 자료 조각 {index:08d} 본문에는 숫자와 표식만 들어 있습니다."
            if index == 0:
                text += " PERF-TARGET-9241"
            texts.append((file_id, text, f"{index % 4 + 1}페이지", vectors[index].tobytes(), SEARCH_MODEL))
        db.executemany("INSERT INTO chunks(file_id,text,location,vector,model_id) VALUES (?,?,?,?,?)", texts)
    del vectors, texts
    return store, DeterministicEngine(dimension)


def measure_search(store, engine, repeats):
    search = Search(store, engine)
    queries = [SEMANTIC_QUERY] + [f"비슷한 파일을 찾아줘 benchmark query {i}" for i in range(1, repeats)]
    connection_count = [0]
    original_connect = store.connect

    def counted_connect():
        connection_count[0] += 1
        return original_connect()

    store.connect = counted_connect
    samples = []
    first = None
    for query in queries:
        duration, result = elapsed(lambda query=query: search.search(query, limit=10, mode="hybrid"))
        samples.append(duration)
        if query == SEMANTIC_QUERY:
            first = result
    key_duration, keyword = elapsed(lambda: search.search("PERF-TARGET-9241", limit=10, mode="keyword"))
    name_duration, filename = elapsed(lambda: search.search("filenameprobe9342_target", limit=10, mode="keyword"))
    semantic_ok = bool(first and first["results"] and first["results"][0]["name"] == "filenameprobe9342_target.txt"
                       and "semantic" in first["results"][0]["match_type"])
    keyword_ok = any(row["name"] == "filenameprobe9342_target.txt" for row in keyword["results"])
    filename_ok = any(row["name"] == "filenameprobe9342_target.txt" for row in filename["results"])
    if not (semantic_ok and keyword_ok and filename_ok):
        raise AssertionError({"semantic": first, "keyword": keyword, "filename": filename})
    store.connect = original_connect
    return {
        "hybrid_queries": samples,
        "hybrid_cold_ms": round(samples[0] * 1000, 2),
        "hybrid_warm_median_ms": round(statistics.median(samples[1:] or samples) * 1000, 2),
        "hybrid_median_ms": round(statistics.median(samples) * 1000, 2),
        "hybrid_p95_ms": round(percentile95(samples) * 1000, 2),
        "keyword_ms": round(key_duration * 1000, 2),
        "filename_ms": round(name_duration * 1000, 2),
        "database_connections_for_queries": connection_count[0],
        "semantic_hit": first["results"][0]["name"],
        "keyword_hit": next(row["name"] for row in keyword["results"]
                            if row["name"] == "filenameprobe9342_target.txt"),
        "filename_hit": next(row["name"] for row in filename["results"]
                              if row["name"] == "filenameprobe9342_target.txt"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", default="baseline", choices=("baseline", "after"))
    parser.add_argument("--files", type=int, default=5000)
    parser.add_argument("--chunks", type=int, default=20000)
    parser.add_argument("--dimension", type=int, default=384)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--output-dir", type=Path, default=Path(".moasearch-perf"))
    args = parser.parse_args()
    if not (1 <= args.files <= 100_000 and 1 <= args.chunks <= 200_000
            and 4 <= args.dimension <= 4096 and 1 <= args.repeats <= 30):
        parser.error("파일/청크/차원/반복 횟수가 허용 범위를 벗어났습니다.")
    if args.chunks * args.dimension * 4 > 512 * 1024 * 1024:
        parser.error("벤치마크 벡터 생성 메모리는 512 MiB 이하로 지정하세요.")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix="run-", dir=args.output_dir.resolve()))
    progress_path = run_dir / f"{args.label}-progress.json"
    report = {
        "label": args.label,
        "synthetic_data_only": True,
        "real_documents_read": False,
        "real_llm_inference": False,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "sqlite": __import__("sqlite3").sqlite_version,
        "files": args.files,
        "chunks": args.chunks,
        "embedding_dimension": args.dimension,
        "run_directory": str(run_dir),
        "completed_phases": {},
    }

    def checkpoint(phase, details):
        report["completed_phases"][phase] = details
        progress_path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")

    documents = make_indexed_files(run_dir, args.files)
    index_store = Store(run_dir / "index_data")
    index_store.update_settings({"scan_interval": 0})
    index_store.add_root(documents)
    indexer = Indexer(index_store)  # Text indexing is measured without model inference.
    try:
        first_time, first_state = elapsed(lambda: wait_for_scan(indexer))
        checkpoint("initial_index", {"seconds": round(first_time, 3), "state": first_state})
        unchanged_time, unchanged_state = elapsed(lambda: wait_for_scan(indexer))
        checkpoint("unchanged_rescan", {"seconds": round(unchanged_time, 3), "state": unchanged_state})

        changed = []
        for path in sorted(documents.glob("*.txt"))[:max(1, args.files // 100)]:
            path.write_text(path.read_text(encoding="utf-8") + " 수정 파일 표식", encoding="utf-8")
            stat = path.stat()
            os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000_000))
            changed.append(path.name)
        modified_time, modified_state = elapsed(lambda: wait_for_scan(indexer))
        checkpoint("one_percent_modified_rescan", {
            "seconds": round(modified_time, 3), "files": len(changed), "state": modified_state,
        })
    finally:
        indexer.close()

    vector_start = time.perf_counter()
    vector_store, engine = seed_vector_store(run_dir, args.chunks, args.dimension)
    vector_seed_time = time.perf_counter() - vector_start
    checkpoint("search_fixture_seed", {"seconds": round(vector_seed_time, 3)})
    search = measure_search(vector_store, engine, args.repeats)
    report["indexing"] = {
        "initial_seconds": round(first_time, 3), "initial_state": first_state,
        "unchanged_seconds": round(unchanged_time, 3), "unchanged_state": unchanged_state,
        "one_percent_modified_seconds": round(modified_time, 3),
        "modified_files": len(changed), "modified_state": modified_state,
    }
    report["search_index_seed_seconds_excluded_from_query_timings"] = round(vector_seed_time, 3)
    report["search"] = search
    report["completed_phases"]["search_queries"] = {"passed": True, "connections": search["database_connections_for_queries"]}
    output = run_dir / f"{args.label}-report.json"
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"REPORT={output}")


if __name__ == "__main__":
    main()
