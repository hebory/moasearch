"""Exact substring search plus model-versioned semantic ranking and cited answers."""

import re

import numpy as np


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
    def __init__(self, store, engine=None):
        self.store, self.engine = store, engine

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
                clause = " OR ".join("f.path LIKE ? ESCAPE '\\'" for _ in terms)
                rows = db.execute(f"SELECT f.*, '' AS text, '파일명·경로' AS location FROM files f WHERE ({clause}){ext_sql} ORDER BY f.modified DESC LIMIT 300",
                                  [_like(t) for t in terms] + ext_params).fetchall()
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
                        query_vector = np.frombuffer(self.engine.embed([query], query=True)[0], dtype=np.float32)
                        best = {}
                        cursor = db.execute(f"""SELECT c.id AS chunk_id,c.file_id,c.vector FROM chunks c
                            JOIN files f ON f.id=c.file_id WHERE c.model_id=? AND c.vector IS NOT NULL{ext_sql}""",
                                            [model_id] + ext_params)
                        while batch := cursor.fetchmany(256):
                            valid = [(r, np.frombuffer(r["vector"], dtype=np.float32)) for r in batch]
                            valid = [(r, v) for r, v in valid if v.shape == query_vector.shape and np.isfinite(v).all()]
                            if not valid:
                                continue
                            matrix = np.stack([v for _, v in valid])
                            norms = np.linalg.norm(matrix, axis=1) * np.linalg.norm(query_vector)
                            scores = matrix @ query_vector / np.maximum(norms, 1e-12)
                            for (row, _), score in zip(valid, scores):
                                previous = best.get(row["file_id"])
                                if previous is None or score > previous[0]:
                                    best[row["file_id"]] = (float(score), row["chunk_id"])
                        ranked = sorted(best.values(), reverse=True)[:max(limit * 3, 60)]
                        for rank, (_, chunk_id) in enumerate(ranked, 1):
                            row = db.execute("SELECT f.*,c.text,c.location FROM chunks c JOIN files f ON f.id=c.file_id WHERE c.id=?", (chunk_id,)).fetchone()
                            if row:
                                add(row, rank, "semantic")
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

