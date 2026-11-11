"""Bounded exact cosine search cache for model-versioned SQLite vectors."""

from __future__ import annotations

from dataclasses import dataclass
import threading
import numpy as np


@dataclass(frozen=True, slots=True)
class VectorSnapshot:
    """Normalized vectors and the row metadata needed for exact file ranking."""

    model_id: str
    dimension: int
    revision: int
    vectors: np.ndarray
    chunk_ids: np.ndarray
    file_ids: np.ndarray
    extension_ids: np.ndarray
    extensions: tuple[str, ...]
    extension_lookup: dict[str, int]

    def candidates(self, query, extension=None, file_limit=300):
        """Find the best matching chunk per file, retaining exact cosine ranking."""
        if len(self.chunk_ids) == 0:
            return []
        extension_id = None
        if extension:
            try:
                extension_id = self.extension_lookup[extension]
            except ValueError:
                return []
            except KeyError:
                return []
            positions = np.flatnonzero(self.extension_ids == extension_id)
        else:
            positions = np.arange(len(self.chunk_ids))
        if not positions.size:
            return []

        unique_files, inverse = np.unique(self.file_ids[positions], return_inverse=True)
        best_scores = np.full(unique_files.size, -np.inf, dtype=np.float32)
        # Matmul the cached matrix directly so an unfiltered warm query does
        # not copy the whole matrix just to compute its similarity scores.
        all_scores = self.vectors @ query
        scores = all_scores if extension_id is None else all_scores[positions]
        np.maximum.at(best_scores, inverse, scores)
        best_chunks = np.full(unique_files.size, np.iinfo(np.int64).max, dtype=np.int64)
        is_best = scores == best_scores[inverse]
        np.minimum.at(best_chunks, inverse[is_best], self.chunk_ids[positions[is_best]])

        count = min(file_limit, len(unique_files))
        if count < len(unique_files):
            threshold = np.partition(best_scores, len(best_scores) - count)[len(best_scores) - count]
            above = np.flatnonzero(best_scores > threshold)
            tied = np.flatnonzero(best_scores == threshold)
            needed = count - len(above)
            if needed < len(tied):
                chosen = np.argpartition(best_chunks[tied], len(tied) - needed)[-needed:]
                tied = tied[chosen]
            selected = np.concatenate((above, tied))
        else:
            selected = np.arange(len(unique_files))
        ranked = sorted(((float(best_scores[i]), int(best_chunks[i])) for i in selected), reverse=True)
        return [(score, chunk_id) for score, chunk_id in ranked]


class ExactVectorCache:
    """Retain at most one normalized float32 matrix, keyed to the DB revision."""

    def __init__(self, max_bytes=128 * 1024 * 1024):
        self.max_bytes = max(0, int(max_bytes))
        self._snapshot: VectorSnapshot | None = None
        self._lock = threading.RLock()

    @property
    def snapshot(self):
        return self._snapshot

    def get_or_build(self, db, model_id, dimension, revision):
        with self._lock:
            return self._get_or_build(db, model_id, dimension, revision)

    def _get_or_build(self, db, model_id, dimension, revision):
        current = self._snapshot
        key = (model_id, dimension, revision)
        if current is not None and (current.model_id, current.dimension, current.revision) == key:
            return current
        # Keep at most one ready-to-use matrix. If a query is already using
        # the previous immutable snapshot, its local reference keeps it alive.
        self._snapshot = None
        current = None

        count = db.execute("""SELECT count(*) FROM chunks
            WHERE model_id=? AND vector IS NOT NULL""", (model_id,)).fetchone()[0]
        row_cost = dimension * np.dtype(np.float32).itemsize + 2 * np.dtype(np.int64).itemsize + np.dtype(np.int32).itemsize
        if count * row_cost > self.max_bytes:
            return None

        try:
            vectors = np.empty((count, dimension), dtype=np.float32)
            chunk_ids = np.empty(count, dtype=np.int64)
            file_ids = np.empty(count, dtype=np.int64)
            extension_ids = np.empty(count, dtype=np.int32)
        except MemoryError:
            return None
        extension_to_id: dict[str, int] = {}
        used = 0
        cursor = db.execute("""SELECT c.id AS chunk_id,c.file_id,f.extension,c.vector
            FROM chunks c JOIN files f ON f.id=c.file_id
            WHERE c.model_id=? AND c.vector IS NOT NULL ORDER BY c.file_id,c.id""", (model_id,))
        while rows := cursor.fetchmany(512):
            valid_rows = []
            for row in rows:
                try:
                    vector = np.frombuffer(row["vector"], dtype=np.float32)
                except (TypeError, ValueError):
                    continue
                if vector.shape != (dimension,):
                    continue
                valid_rows.append((row, vector))
            if not valid_rows:
                continue

            try:
                matrix = np.stack([vector for _, vector in valid_rows])
                finite = np.isfinite(matrix).all(axis=1)
                if not finite.any():
                    continue
                matrix = matrix[finite]
                valid_rows = [item for item, keep in zip(valid_rows, finite) if keep]
                norms = np.linalg.norm(matrix.astype(np.float64), axis=1)
                # Normalize a batch into the preallocated float32 matrix. A
                # zero vector stays zero, preserving its score and ranking.
                target = vectors[used:used + len(valid_rows)]
                target[:] = 0
                nonzero = norms > 0
                target[nonzero] = matrix[nonzero] / norms[nonzero, None].astype(np.float32)
            except MemoryError:
                return None

            for offset, (row, _) in enumerate(valid_rows):
                extension = row["extension"] or ""
                if extension not in extension_to_id:
                    extension_to_id[extension] = len(extension_to_id)
                position = used + offset
                chunk_ids[position] = row["chunk_id"]
                file_ids[position] = row["file_id"]
                extension_ids[position] = extension_to_id[extension]
            used += len(valid_rows)

        extensions = tuple(extension_to_id)
        try:
            snapshot = VectorSnapshot(
                model_id, dimension, revision,
                vectors[:used], chunk_ids[:used], file_ids[:used], extension_ids[:used],
                extensions, extension_to_id,
            )
        except (MemoryError, ValueError):
            return None
        self._snapshot = snapshot
        return snapshot
