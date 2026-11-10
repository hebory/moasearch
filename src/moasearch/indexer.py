"""Single-writer, cancellable incremental filesystem indexing."""

import os
from pathlib import Path
import threading
import time
from contextlib import contextmanager

from .config import excluded, within
from .extractors import extract, chunk_passages, supports_content


class Indexer:
    def __init__(self, store, engine=None):
        self.store, self.engine = store, engine
        self._guard = threading.Lock()
        self._cancel = threading.Event()
        self._stop = threading.Event()
        self._thread = None
        self._scheduler = None
        self._state = dict(running=False, scanned=0, indexed=0, skipped=0, errors=0,
                           current_path="", last_error="", phase="idle")

    def status(self):
        with self._guard:
            return self._state.copy()

    def _update(self, **values):
        with self._guard:
            self._state.update(values)

    def start(self):
        with self._guard:
            if self._state["running"]:
                return False
            self._cancel.clear()
            self._state.update(running=True, scanned=0, indexed=0, skipped=0, errors=0,
                               current_path="", last_error="", phase="scanning")
            self._thread = threading.Thread(target=self._run, name="moasearch-index", daemon=True)
            self._thread.start()
            return True

    def cancel(self):
        self._cancel.set()

    @contextmanager
    def idle_changes(self):
        with self._guard:
            if self._state["running"]:
                raise ValueError("색인이 끝나거나 취소된 뒤 설정과 폴더를 변경하세요.")
            yield

    def start_scheduler(self):
        def schedule():
            last = time.monotonic()
            while not self._stop.wait(2):
                interval = self.store.settings()["scan_interval"]
                if interval and time.monotonic() - last >= interval:
                    self.start()
                    last = time.monotonic()
        self._scheduler = threading.Thread(target=schedule, name="moasearch-watch", daemon=True)
        self._scheduler.start()

    def close(self):
        self._stop.set()
        self.cancel()
        if self._thread:
            self._thread.join(timeout=5)
        if self._scheduler:
            self._scheduler.join(timeout=3)

    def _error(self, message):
        state = self.status()
        self._update(errors=state["errors"] + 1, last_error=str(message)[:500])

    def _walk(self, root, failures):
        pending = [root]
        while pending and not self._cancel.is_set():
            directory = pending.pop()
            try:
                with os.scandir(directory) as entries:
                    for entry in entries:
                        if self._cancel.is_set():
                            return
                        path = Path(entry.path)
                        if excluded(path) or within(path, self.store.data_dir):
                            continue
                        try:
                            st = entry.stat(follow_symlinks=False)
                            # Reparse points include junctions and cloud placeholders: never hydrate or follow.
                            if entry.is_symlink() or getattr(st, "st_file_attributes", 0) & (0x400 | 0x1000 | 0x40000 | 0x400000):
                                continue
                            if entry.is_dir(follow_symlinks=False):
                                pending.append(path)
                            elif entry.is_file(follow_symlinks=False):
                                yield path, st
                        except OSError as exc:
                            failures.append(str(exc))
                            self._error(exc)
            except OSError as exc:
                failures.append(str(exc))
                self._error(exc)

    def _run(self):
        try:
            settings = self.store.settings()
            model_id = self.engine.model_id() if self.engine else None
            for root in self.store.roots():
                if self._cancel.is_set():
                    break
                seen, failures = set(), []
                base = Path(root["path"])
                if not base.is_dir():
                    self._error(f"폴더에 접근할 수 없습니다: {base}")
                    continue
                for path, stat in self._walk(base, failures):
                    seen.add(str(path))
                    state = self.status()
                    self._update(scanned=state["scanned"] + 1, current_path=str(path))
                    old = self.store.file_by_path(path)
                    if (old and old["modified"] == stat.st_mtime_ns and old["size"] == stat.st_size
                            and old["status"] != "error"
                            and not (old["status"] == "metadata" and supports_content(path)
                                     and stat.st_size <= settings["max_file_mb"] * 1024 * 1024)
                            and (not model_id or old["model_id"] == model_id or old["status"] == "metadata")):
                        self._update(skipped=self.status()["skipped"] + 1)
                        continue
                    passages, vectors, error = [], None, None
                    try:
                        if stat.st_size <= settings["max_file_mb"] * 1024 * 1024 and supports_content(path):
                            # Resolve immediately before read so a replaced parent cannot escape the root.
                            resolved = path.resolve()
                            if not within(resolved, base) or excluded(resolved):
                                raise ValueError("색인 범위 밖으로 변경된 파일입니다.")
                            passages = chunk_passages(extract(path))
                        if passages and model_id and not self._cancel.is_set():
                            try:
                                self._update(phase="embedding")
                                vectors = []
                                for offset in range(0, len(passages), 4):
                                    if self._cancel.is_set():
                                        break
                                    vectors.extend(self.engine.embed([p.text for p in passages[offset:offset + 4]]))
                            except Exception as exc:
                                self._error(f"의미 색인 실패, 본문은 저장합니다: {exc}")
                                vectors = None
                                model_id = None  # Avoid repeating a model failure for every file in this pass.
                            finally:
                                self._update(phase="scanning")
                        after = path.stat()
                        if (after.st_mtime_ns, after.st_size) != (stat.st_mtime_ns, stat.st_size):
                            self._error(f"읽는 중 변경되어 다음 색인으로 미룹니다: {path}")
                            continue
                    except Exception as exc:
                        error = str(exc)[:500]
                        self._error(f"{path.name}: {error}")
                    if self._cancel.is_set():
                        break
                    self.store.replace_file(root["id"], path, stat, passages, vectors,
                                            model_id if vectors else None, error)
                    self._update(indexed=self.status()["indexed"] + 1)
                if not failures and not self._cancel.is_set():
                    self.store.prune(root["id"], seen)
        except Exception as exc:
            self._error(exc)
            self._update(phase="error")
        finally:
            phase = "cancelled" if self._cancel.is_set() else "error" if self.status()["phase"] == "error" else "complete"
            self._update(running=False, current_path="", phase=phase)

