from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import patch

from moasearch import ocr


def _save_image(path: Path, *, size=(80, 40), color="white", frames=None):
    from PIL import Image

    if frames:
        images = [Image.new("RGB", size, value) for value in frames]
        images[0].save(path, save_all=True, append_images=images[1:], duration=10, loop=0)
    else:
        Image.new("RGB", size, color).save(path)
    return path


class FakeEngine:
    def __init__(self):
        self.values = iter(())
        self.images = []

    def __call__(self, image):
        self.images.append(image)
        return next(self.values)


class OcrTests(unittest.TestCase):
    def setUp(self):
        self.real_status = ocr.status
        self.temp = tempfile.TemporaryDirectory(prefix="moasearch-ocr-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.model_dir = self.root / "models"
        self.model_dir.mkdir()
        for name in ocr._MODEL_FILES.values():
            (self.model_dir / name).write_bytes(b"synthetic model placeholder")
        self.engine = FakeEngine()
        self.status_patch = patch.object(ocr, "status", return_value={
            "available": True, "ready": True, "message": "ok",
        })
        self.load_patch = patch.object(ocr, "_load_engine", return_value=self.engine)
        self.status_patch.start()
        self.load_patch.start()
        self.addCleanup(self.load_patch.stop)
        self.addCleanup(self.status_patch.stop)

    def test_status_requires_dependencies_and_both_model_files(self):
        result = self.real_status(self.root / "missing-models")
        self.assertIsInstance(result["available"], bool)
        self.assertFalse(result["ready"])
        self.assertTrue("모델" in result["message"] or "패키지" in result["message"])

    def test_extract_image_returns_text_and_location(self):
        self.engine.values = iter([SimpleNamespace(txts=["안녕하세요", "local OCR"])])
        image_path = _save_image(self.root / "sample.png")
        self.assertEqual(ocr.extract_image(image_path, 200, self.model_dir), [
            ("안녕하세요\nlocal OCR", "이미지 OCR"),
        ])

    def test_empty_or_blank_ocr_result_returns_no_passage(self):
        self.engine.values = iter([SimpleNamespace(txts=[]), None])
        image_path = _save_image(self.root / "sample.png")
        self.assertEqual(ocr.extract_image(image_path, 200, self.model_dir), [])

    def test_output_respects_character_limit(self):
        self.engine.values = iter([SimpleNamespace(txts=["abcdefghijk"])])
        image_path = _save_image(self.root / "sample.png")
        self.assertEqual(ocr.extract_image(image_path, 5, self.model_dir), [("abcde", "이미지 OCR")])

    def test_animated_image_is_processed_with_frame_locations(self):
        self.engine.values = iter([SimpleNamespace(txts=["첫 장"]), SimpleNamespace(txts=["둘째 장"])])
        image_path = _save_image(self.root / "sample.webp", frames=["white", "black"])
        self.assertEqual(ocr.extract_image(image_path, 200, self.model_dir), [
            ("첫 장", "이미지 1 OCR"), ("둘째 장", "이미지 2 OCR"),
        ])

    def test_oversized_source_is_rejected_before_decoder(self):
        path = self.root / "large.png"
        path.write_bytes(b"x")
        with patch.object(ocr, "_MAX_SOURCE_BYTES", 0):
            with self.assertRaisesRegex(ValueError, "100 MiB"):
                ocr.extract_image(path, 100, self.model_dir)

    def test_over_limit_pixel_dimensions_are_rejected(self):
        image_path = _save_image(self.root / "large.png", size=(11, 10))
        from PIL import Image

        with patch.object(ocr, "_MAX_PIXELS", 100), patch.object(
            Image.Image, "copy", side_effect=AssertionError("copied before dimensions check"),
        ):
            with self.assertRaisesRegex(ValueError, "픽셀"):
                ocr.extract_image(image_path, 100, self.model_dir)

    def test_invalid_image_has_clear_error(self):
        path = self.root / "broken.png"
        path.write_bytes(b"not an image")
        with self.assertRaisesRegex(ValueError, "이미지를 해석"):
            ocr.extract_image(path, 100, self.model_dir)

    def test_unsupported_suffix_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "지원하지 않는"):
            ocr.extract_image(self.root / "document.pdf", 100, self.model_dir)

    def test_engine_failure_is_reported_as_korean_runtime_error(self):
        with patch.object(ocr, "_load_engine", side_effect=RuntimeError("runtime exploded")):
            image_path = _save_image(self.root / "sample.png")
            with self.assertRaisesRegex(RuntimeError, "runtime exploded"):
                ocr.extract_image(image_path, 100, self.model_dir)

    def test_missing_models_fail_without_network_access(self):
        import requests

        self.load_patch.stop()
        with patch.object(requests.sessions.Session, "request",
                          side_effect=AssertionError("network access attempted")):
            with self.assertRaisesRegex(ValueError, "OCR 모델"):
                ocr._load_engine(self.root / "absent-models")

    def test_model_pair_is_verified_only_when_engine_cache_misses(self):
        from moasearch import models
        from rapidocr import EngineType, LangRec, ModelType, OCRVersion

        self.load_patch.stop()
        verify = unittest.mock.Mock(return_value={
            "det": self.model_dir / ocr._MODEL_FILES["det"],
            "rec": self.model_dir / ocr._MODEL_FILES["rec"],
        })
        created = []

        class FakeRapidOCR:
            def __init__(self, *, params):
                created.append(params)

        with patch.object(models, "ocr_model_paths", return_value={
            kind: self.model_dir / name for kind, name in ocr._MODEL_FILES.items()
        }), patch.object(models, "verify_ocr_models", verify), \
             patch("rapidocr.RapidOCR", FakeRapidOCR), \
             patch.object(ocr, "_engine", None), patch.object(ocr, "_engine_key", None):
            first = ocr._load_engine(self.model_dir)
            second = ocr._load_engine(self.model_dir)

        self.assertIs(first, second)
        verify.assert_called_once_with(self.model_dir)
        self.assertEqual(len(created), 1)
        self.assertIs(created[0]["Det.engine_type"], EngineType.ONNXRUNTIME)
        self.assertIs(created[0]["Det.ocr_version"], OCRVersion.PPOCRV5)
        self.assertIs(created[0]["Det.model_type"], ModelType.MOBILE)
        self.assertIs(created[0]["Rec.lang_type"], LangRec.KOREAN)

    def test_single_cached_engine_serializes_parallel_inference(self):
        active = 0
        max_active = 0
        counter_lock = threading.Lock()

        class ConcurrentEngine:
            def __call__(self, image):
                nonlocal active, max_active
                with counter_lock:
                    active += 1
                    max_active = max(max_active, active)
                time.sleep(0.05)
                with counter_lock:
                    active -= 1
                return SimpleNamespace(txts=["test"])

        image_a = _save_image(self.root / "a.png")
        image_b = _save_image(self.root / "b.png")
        with patch.object(ocr, "_load_engine", return_value=ConcurrentEngine()):
            with ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(
                    lambda path: ocr.extract_image(path, 100, self.model_dir),
                    (image_a, image_b),
                ))

        self.assertEqual(results, [[("test", "이미지 OCR")], [("test", "이미지 OCR")]])
        self.assertEqual(max_active, 1)


if __name__ == "__main__":
    unittest.main()
