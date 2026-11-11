"""Exact substring search plus model-versioned semantic ranking and cited answers."""

import re
import threading
import heapq
from collections import OrderedDict

import numpy as np

from .vector_search import ExactVectorCache


def _terms(query):
    return list(dict.fromkeys(re.findall(r"[\w.-]+", query.casefold())))[:24]


def _like(value):
    return "%" + value.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"


def _preview(text, terms, length=360):
    folded = text.casefold()
    positions = [folded.find(t) for t in terms if t in folded]
    start = max(0, min(positions) - 70) if positions else 0
    return ("…" if start else "") + text[start:start + length] + ("…" if start + length < len(text) else "")


class Search:
    def __init__(self, store, engine=None, *, vector_cache_bytes=128 * 1024 * 1024):
        self.store, self.engine = store, engine
        self._vectors = ExactVectorCache(vector_cache_bytes)
        self._query_lock = threading.Lock()
        self._query_vectors = OrderedDict()
        self._query_cache_size = 64

    def _query_vector(self, model_id, query):
        key = (model_id, query)
        with self._query_lock:
            vector = self._query_vectors.get(key)
            if vector is not None:
                self._query_vectors.move_to_end(key)
                return vector
            raw = self.engine.embed([query], query=True)[0]
            vector = (np.frombuffer(raw, dtype=np.float32).copy()
                      if isinstance(raw, (bytes, bytearray, memoryview))
                      else np.asarray(raw, dtype=np.float32).reshape(-1))
            if not vector.size or not np.isfinite(vector).all():
                raise ValueError("검색 임베딩이 비어 있거나 올바르지 않습니다.")
            norm = float(np.linalg.norm(vector.astype(np.float64)))
            vector = (vector / norm if norm > 0 else np.zeros_like(vector)).astype(np.float32, copy=False)
            vector.setflags(write=False)
            self._query_vectors[key] = vector
            if len(self._query_vectors) > self._query_cache_size:
                self._query_vectors.popitem(last=False)
            return vector

    @staticmethod
    def _streaming_candidates(db, model_id, query_vector, extension, file_limit):
        """Exact, bounded-memory fallback when the model matrix exceeds budget."""
        ext_sql = " AND f.extension=?" if extension else ""
        params = [model_id] + ([extension] if extension else [])
        cursor = db.execute(f"""SELECT c.id AS chunk_id,c.file_id,c.vector FROM chunks c
            JOIN files f ON f.id=c.file_id WHERE c.model_id=? AND c.vector IS NOT NULL{ext_sql}
            ORDER BY c.file_id,c.id""", params)
        # Keep only the best chunk for the current file and the best N files.
        # This bounds auxiliary memory even when the exact fallback scans a
        # very large corpus. Chunk IDs provide the historical tie-breaker.
        top_files = []
        current_file_id = None
        current_best = None

        def retain_current():
            if current_best is None:
                return
            score, chunk_id = current_best
            entry = (score, chunk_id)
            if len(top_files) < file_limit:
                heapq.heappush(top_files, entry)
            elif entry > top_files[0]:
                heapq.heapreplace(top_files, entry)

        while batch := cursor.fetchmany(256):
            valid = []
            for row in batch:
                try:
                    vector = np.frombuffer(row["vector"], dtype=np.float32)
                except (TypeError, ValueError):
                    continue
                if vector.shape != query_vector.shape or not np.isfinite(vector).all():
                    continue
                norm = float(np.linalg.norm(vector.astype(np.float64)))
                valid.append((row, vector, norm))
            if not valid:
                continue
            matrix = np.stack([vector / norm if norm > 0 else np.zeros_like(vector)
                               for _, vector, norm in valid])
            scores = matrix @ query_vector
            for (row, _, _), score in zip(valid, scores):
                file_id = row["file_id"]
                if current_file_id is None:
                    current_file_id = file_id
                elif file_id != current_file_id:
                    retain_current()
                    current_file_id = file_id
                    current_best = None
                candidate = (float(score), row["chunk_id"])
                if current_best is None or candidate[0] > current_best[0]:
                    current_best = candidate
        retain_current()
        return sorted(top_files, reverse=True)

    def search(self, query, limit=30, extension=None, mode="hybrid"):
        query = query.strip()
        if not query or len(query) > 2000:
            raise ValueError("검색어는 1~2,000자로 입력하세요.")
        limit = max(1, min(100, limit))
        terms = _terms(query)
        extension = ("." + extension.lstrip(".").lower()) if extension else None
        warnings, hits = [], {}
        with self.store.connect() as db:
            ext_sql = " AND f.extension=?" if extension else ""
            ext_params = [extension] if extension else []

            def add(row, rank, source):
                item = dict(row)
                key = item["id"]
                contribution = 1.0 / (40 + rank)
                existing = hits.get(key)
                if existing:
                    existing["score"] += contribution
                    if source not in existing["match_type"]:
                        existing["match_type"] += "+" + source
                    if item.get("text") and (source == "keyword" or not existing.get("text")):
                        existing["text"] = item["text"]
                        existing["location"] = item["location"]
                else:
                    hits[key] = item | {"score": contribution, "match_type": source}

            if terms:
                path_parts, path_params = [], []
                long_path_terms = [term for term in terms if len(term) >= 3]
                short_path_terms = [term for term in terms if len(term) < 3]
                if long_path_terms:
                    expression = " OR ".join('"' + term.replace('"', '""') + '"'
                                             for term in long_path_terms)
                    exact = " OR ".join("f.path LIKE ? ESCAPE '\\'" for _ in long_path_terms)
                    path_parts.append(
                        f"(f.id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?) AND ({exact}))")
                    path_params.append(expression)
                    path_params.extend(_like(term) for term in long_path_terms)
                if short_path_terms:
                    path_parts.append("(" + " OR ".join("f.path LIKE ? ESCAPE '\\'"
                                                           for _ in short_path_terms) + ")")
                    path_params.extend(_like(term) for term in short_path_terms)
                clause = " OR ".join(path_parts)
                rows = db.execute(f"SELECT f.*, '' AS text, '파일명·경로' AS location FROM files f WHERE ({clause}){ext_sql} ORDER BY f.modified DESC LIMIT 300",
                                  path_params + ext_params).fetchall()
                rows = sorted(rows, key=lambda row: sum(t in row["name"].casefold() for t in terms), reverse=True)
                for rank, row in enumerate(rows, 1):
                    add(row, rank, "filename")
                long_terms = [t for t in terms if len(t) >= 3]
                if long_terms:
                    expression = " OR ".join('"' + t.replace('"', '""') + '"' for t in long_terms)
                    rows = db.execute(f"""SELECT f.*,c.text,c.location FROM chunks_fts
                        JOIN chunks c ON c.id=chunks_fts.rowid JOIN files f ON f.id=c.file_id
                        WHERE chunks_fts MATCH ?{ext_sql} ORDER BY bm25(chunks_fts) LIMIT 400""",
                                      [expression] + ext_params).fetchall()
                else:
                    rows = []
                # Trigram FTS cannot represent one/two-character Korean words.
                short_terms = [t for t in terms if len(t) < 3]
                if short_terms:
                    clause = " OR ".join("c.text LIKE ? ESCAPE '\\'" for _ in short_terms)
                    rows += db.execute(f"""SELECT f.*,c.text,c.location FROM chunks c JOIN files f ON f.id=c.file_id
                        WHERE ({clause}){ext_sql} ORDER BY f.modified DESC LIMIT 200""",
                                       [_like(t) for t in short_terms] + ext_params).fetchall()
                seen = set()
                for row in rows:
                    if row["id"] not in seen:
                        seen.add(row["id"])
                        add(row, len(seen), "keyword")

        actual_mode = "keyword"
        if mode == "hybrid":
            model_id = self.engine.model_id() if self.engine else None
            if model_id:
                try:
                    query_vector = self._query_vector(model_id, query)
                    file_limit = max(limit * 3, 60)
                    # Embedding inference runs before opening the read snapshot,
                    # so a slow model call does not hold back WAL checkpoints.
                    with self.store.connect() as db:
                        db.execute("BEGIN")
                        state = self.store.index_state(db=db)
                        snapshot = self._vectors.get_or_build(
                            db, model_id, query_vector.size, state["revision"])
                        if snapshot is not None:
                            try:
                                ranked = snapshot.candidates(query_vector, extension, file_limit)
                            except MemoryError:
                                ranked = self._streaming_candidates(
                                    db, model_id, query_vector, extension, file_limit)
                        else:
                            ranked = self._streaming_candidates(
                                db, model_id, query_vector, extension, file_limit)
                        chunk_ids = [chunk_id for _, chunk_id in ranked]
                        rows_by_chunk = {}
                        if chunk_ids:
                            placeholders = ",".join("?" for _ in chunk_ids)
                            rows = db.execute(f"""SELECT f.*,c.id AS result_chunk_id,c.text,c.location
                                FROM chunks c JOIN files f ON f.id=c.file_id
                                WHERE c.id IN ({placeholders})""", chunk_ids).fetchall()
                            rows_by_chunk = {row["result_chunk_id"]: row for row in rows}
                        for rank, (_, chunk_id) in enumerate(ranked, 1):
                            row = rows_by_chunk.get(chunk_id)
                            if row:
                                item = dict(row)
                                item.pop("result_chunk_id", None)
                                add(item, rank, "semantic")
                    actual_mode = "hybrid"
                    if not ranked:
                        warnings.append("현재 모델의 의미 색인이 없습니다. 폴더를 다시 색인하세요.")
                except Exception as exc:
                    warnings.append(f"의미 검색을 사용할 수 없어 키워드로 검색했습니다: {exc}")
            else:
                warnings.append("내장 임베딩 엔진과 GGUF 모델을 준비하면 의미 검색을 사용할 수 있습니다.")
        results = sorted(hits.values(), key=lambda item: item["score"], reverse=True)[:limit]
        for item in results:
            item["snippet"] = _preview(item.pop("text", ""), terms)
            item.pop("model_id", None)
        return {"results": results, "mode": actual_mode, "warnings": warnings}

    def answer(self, query, limit=5):
        result = self.search(query, limit=min(8, max(1, limit)))
        sources = [item for item in result["results"] if item["snippet"]]
        if not sources:
            return {"answer": "답변에 근거로 사용할 문서 본문을 찾지 못했습니다.", "sources": [], "warnings": result["warnings"]}
        if not self.engine:
            raise ValueError("답변용 GGUF 모델을 먼저 설정하세요.")
        answer = self.engine.answer(query, sources)
        return {"answer": answer, "sources": sources, "warnings": result["warnings"]}

