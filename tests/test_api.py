from __future__ import annotations

import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from fastapi.testclient import TestClient

from moasearch.app import create_app


class FakeEngine:
    def __init__(self, store):
        self.store = store
        self.resets = 0

    def status(self):
        return {"ready": False, "reason": "test"}

    def model_id(self):
        return None

    def embed(self, texts, query=False):
        raise AssertionError("키워드 전용 API 테스트에서는 임베딩을 호출하지 않아야 합니다")

    def reset(self):
        self.resets += 1

    def close(self):
        pass

    def answer(self, query, sources):
        return "테스트 답변"


class ApiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "documents"
        self.root.mkdir()
        self.data = self.base / "app-data"
        self.note = self.root / "report.txt"
        self.note.write_text("2026 분기별 매출 계획과 목표", encoding="utf-8")
        self.script = self.root / "automation.ps1"
        self.script.write_text("Write-Output 'automation'", encoding="utf-8")
        self.engine = FakeEngine(None)
        self.app = create_app(self.data, self.engine)
        self.client = TestClient(self.app, base_url="http://127.0.0.1")
        self.client.__enter__()
        self.addCleanup(self.client.__exit__, None, None, None)
        self.headers = {"x-moasearch-token": self.app.state.token}

    def test_token_foreign_host_and_foreign_origin_are_rejected(self) -> None:
        self.assertEqual(self.client.get("/api/status").status_code, 403)
        response = self.client.get("/api/status", headers=self.headers | {"host": "attacker.invalid"})
        self.assertEqual(response.status_code, 403)
        response = self.client.get("/api/status", headers=self.headers | {"origin": "https://attacker.invalid"})
        self.assertEqual(response.status_code, 403)

    def test_api_indexes_searches_and_gates_file_actions(self) -> None:
        # The unguessable per-process token is handed only to the local app page.
        page = self.client.get("/")
        self.assertEqual(page.status_code, 200)
        self.assertIn(self.app.state.token, page.text)
        self.assertIn("default-src 'self'", page.headers["content-security-policy"])

        added = self.client.post("/api/roots", headers=self.headers, json={"path": str(self.root)})
        self.assertEqual(added.status_code, 200, added.text)
        nested = self.root / "child"
        nested.mkdir()
        overlap = self.client.post("/api/roots", headers=self.headers, json={"path": str(nested)})
        self.assertEqual(overlap.status_code, 400)

        started = self.client.post("/api/index", headers=self.headers)
        self.assertTrue(started.json()["started"])
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            status = self.client.get("/api/status", headers=self.headers).json()
            if not status["index"]["running"]:
                break
            time.sleep(0.02)
        self.assertFalse(status["index"]["running"])

        result = self.client.post("/api/search", headers=self.headers,
                                  json={"query": "분기별 매출", "mode": "keyword"})
        self.assertEqual(result.status_code, 200, result.text)
        found = next(item for item in result.json()["results"] if item["name"] == self.note.name)
        details = self.client.get(f"/api/files/{found['id']}", headers=self.headers)
        self.assertEqual(details.status_code, 200)
        self.assertIn("2026", details.json()["passages"][0]["text"])

        with patch("moasearch.app.subprocess.Popen") as spawn, patch.object(os, "startfile", create=True) as open_local:
            opened = self.client.post(f"/api/files/{found['id']}/open", headers=self.headers)
            self.assertEqual(opened.status_code, 200, opened.text)
            open_local.assert_called_once_with(str(self.note.resolve()))

            script_result = self.client.post("/api/search", headers=self.headers,
                                             json={"query": "automation", "mode": "keyword"}).json()
            script_hit = next(item for item in script_result["results"] if item["name"] == self.script.name)
            blocked = self.client.post(f"/api/files/{script_hit['id']}/open", headers=self.headers)
            self.assertEqual(blocked.status_code, 400)
            spawn.assert_not_called()

    def test_missing_file_id_is_not_revealed(self) -> None:
        with patch("moasearch.app.subprocess.Popen") as spawn:
            response = self.client.post("/api/files/999999/reveal", headers=self.headers)
        self.assertEqual(response.status_code, 404)
        spawn.assert_not_called()

    def test_model_download_applies_verified_file_and_resets_engine(self) -> None:
        model = self.base / "verified.gguf"
        model.write_bytes(b"GGUFtest")
        with patch("moasearch.models.download_model", return_value=model):
            response = self.client.post("/api/models/download", headers=self.headers, json={"kind": "embedding"})
            self.assertEqual(response.status_code, 200)
            state = self.wait_download()
        self.assertEqual(state["model_download"]["error"], "")
        self.assertEqual(state["settings"]["embedding_model"], str(model.resolve()))
        self.assertEqual(self.engine.resets, 1)

    def test_disappearing_download_is_reported_without_infinite_retry(self) -> None:
        with patch("moasearch.models.download_model", return_value=self.base / "missing.gguf"):
            response = self.client.post("/api/models/download", headers=self.headers, json={"kind": "chat"})
            self.assertEqual(response.status_code, 200)
            state = self.wait_download()
        self.assertTrue(state["model_download"]["error"])
        self.assertEqual(state["settings"]["chat_model"], "")

    def wait_download(self):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            state = self.client.get("/api/status", headers=self.headers).json()
            if not state["model_download"]["running"]:
                return state
            time.sleep(.02)
        self.fail("모델 다운로드 작업이 종료되지 않았습니다")


if __name__ == "__main__":
    unittest.main()
