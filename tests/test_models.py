from __future__ import annotations

import hashlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from moasearch import models


class Response(io.BytesIO):
    def __init__(self, data: bytes, length: str | None = None):
        super().__init__(data)
        self.headers = {"Content-Length": length} if length is not None else {}

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class ModelDownloadTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.content = b"GGUF" + b"small test model"
        self.metadata = {
            "repo": "test/model", "filename": "model.gguf",
            "sha256": hashlib.sha256(self.content).hexdigest(), "bytes": len(self.content),
            "description": "test", "source": "https://huggingface.co/test/model/blob/main/model.gguf",
            "url": "https://huggingface.co/test/model/resolve/main/model.gguf",
        }
        self.catalog_patch = patch.dict(models._CATALOG, {"embedding": self.metadata})
        self.catalog_patch.start()
        self.addCleanup(self.catalog_patch.stop)

    def test_catalog_has_fixed_model_provenance_and_public_metadata(self) -> None:
        with patch.dict(models._CATALOG, {"embedding": self.metadata, "chat": self.metadata}):
            result = models.catalog()
        self.assertEqual(result["embedding"]["sha256"], self.metadata["sha256"])
        self.assertIn("source", result["chat"])
        self.assertNotIn("url", result["chat"])

    def test_download_verifies_signature_hash_reports_progress_and_installs(self) -> None:
        progress = []
        with patch("moasearch.models.urlopen", return_value=Response(self.content)) as fetch:
            result = models.download_model("embedding", self.directory, lambda received, total: progress.append((received, total)))
        self.assertEqual(result.read_bytes(), self.content)
        self.assertEqual(fetch.call_args.kwargs["timeout"], 60)
        self.assertEqual(progress, [(len(self.content), len(self.content))])
        self.assertEqual(list(self.directory.glob("*.part")), [])

    def test_existing_valid_model_is_reused_without_network(self) -> None:
        target = self.directory / "model.gguf"
        target.write_bytes(self.content)
        with patch("moasearch.models.urlopen") as fetch:
            result = models.download_model("embedding", self.directory)
        self.assertEqual(result, target)
        fetch.assert_not_called()

    def test_existing_invalid_user_model_is_never_overwritten(self) -> None:
        target = self.directory / "model.gguf"
        original = b"keep this user file"
        target.write_bytes(original)
        with patch("moasearch.models.urlopen") as fetch:
            with self.assertRaises(FileExistsError):
                models.download_model("embedding", self.directory)
        self.assertEqual(target.read_bytes(), original)
        fetch.assert_not_called()

    def test_hash_failure_removes_partial_download_and_leaves_destination_absent(self) -> None:
        with patch("moasearch.models.urlopen", return_value=Response(b"GGUF incorrect bytes")):
            with self.assertRaisesRegex(ValueError, "SHA-256"):
                models.download_model("embedding", self.directory)
        self.assertFalse((self.directory / "model.gguf").exists())
        self.assertEqual(list(self.directory.iterdir()), [])

    def test_bad_magic_is_rejected_and_temp_file_is_removed(self) -> None:
        with patch("moasearch.models.urlopen", return_value=Response(b"NOPE" + self.content)):
            with self.assertRaisesRegex(ValueError, "GGUF"):
                models.download_model("embedding", self.directory)
        self.assertEqual(list(self.directory.iterdir()), [])

    def test_size_limit_rejects_response_and_cleans_partial_file(self) -> None:
        with patch.object(models, "_MAX_DOWNLOAD_BYTES", 8), patch(
                "moasearch.models.urlopen", return_value=Response(b"GGUF" + b"0123456789")):
            with self.assertRaisesRegex(ValueError, "1 GiB"):
                models.download_model("embedding", self.directory)
        self.assertEqual(list(self.directory.iterdir()), [])

    def test_unknown_model_kind_is_rejected_before_network(self) -> None:
        with patch("moasearch.models.urlopen") as fetch:
            with self.assertRaises(ValueError):
                models.download_model("other", self.directory)
        fetch.assert_not_called()

    def ocr_catalog(self):
        return {role: {"filename": f"{role}.onnx", "bytes": len(content),
                       "sha256": hashlib.sha256(content).hexdigest(),
                       "url": f"https://example.invalid/{role}.onnx"}
                for role, content in [("det", b"onnx detector"), ("rec", b"onnx Korean recognizer")]}

    def test_ocr_downloads_both_pinned_models_without_gguf_signature(self) -> None:
        progress = []
        with patch.dict(models._OCR_MODELS, self.ocr_catalog(), clear=True), patch(
                "moasearch.models.urlopen", side_effect=[Response(b"onnx detector"), Response(b"onnx Korean recognizer")]):
            result = models.download_model("ocr", self.directory, lambda got, total: progress.append((got, total)))
            self.assertEqual(result, self.directory)
            self.assertEqual(set(models.verify_ocr_models(result)), {"det", "rec"})
        self.assertEqual(progress[-1], (35, 35))

    def test_incomplete_ocr_download_is_not_ready_and_can_resume(self) -> None:
        with patch.dict(models._OCR_MODELS, self.ocr_catalog(), clear=True):
            with patch("moasearch.models.urlopen", side_effect=[Response(b"onnx detector"), Response(b"bad recognizer")]):
                with self.assertRaisesRegex(ValueError, "SHA-256"):
                    models.download_model("ocr", self.directory)
            self.assertTrue((self.directory / "det.onnx").is_file())
            self.assertFalse((self.directory / "rec.onnx").exists())
            with self.assertRaises(ValueError):
                models.verify_ocr_models(self.directory)
            with patch("moasearch.models.urlopen", return_value=Response(b"onnx Korean recognizer")) as fetch:
                models.download_model("ocr", self.directory)
                self.assertEqual(fetch.call_count, 1)
            self.assertEqual(list(self.directory.glob("*.part")), [])

    def test_tampered_ocr_model_is_rejected_before_inference(self) -> None:
        with patch.dict(models._OCR_MODELS, self.ocr_catalog(), clear=True):
            (self.directory / "det.onnx").write_bytes(b"onnx detector")
            (self.directory / "rec.onnx").write_bytes(b"untrusted model")
            with self.assertRaisesRegex(ValueError, "SHA-256"):
                models.verify_ocr_models(self.directory)


if __name__ == "__main__":
    unittest.main()
