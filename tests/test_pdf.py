from __future__ import annotations

from pathlib import Path
import tempfile
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from pypdf import PdfReader, PdfWriter
from pypdf.generic import DecodedStreamObject, DictionaryObject, NameObject

from moasearch.extractors import ExtractionError, Passage, extract
from moasearch.pdf import PdfReadError, _Renderer, _render_scale, extract_pdf


class PdfOcrTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.model_dir = self.root / "models-does-not-exist"

    def text_page(self, writer, text):
        page = writer.add_blank_page(width=612, height=792)
        font = DictionaryObject({
            NameObject("/Type"): NameObject("/Font"),
            NameObject("/Subtype"): NameObject("/Type1"),
            NameObject("/BaseFont"): NameObject("/Helvetica"),
        })
        page[NameObject("/Resources")] = DictionaryObject({
            NameObject("/Font"): DictionaryObject({NameObject("/F1"): font}),
        })
        stream = DecodedStreamObject()
        escaped = text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
        stream.set_data(f"BT /F1 12 Tf 50 700 Td ({escaped}) Tj ET".encode("ascii"))
        page[NameObject("/Contents")] = writer._add_object(stream)
        return page

    def image_pdf(self, filename="image.pdf"):
        from PIL import Image, ImageDraw

        path = self.root / filename
        image = Image.new("RGB", (400, 200), "white")
        ImageDraw.Draw(image).rectangle((15, 15, 385, 185), fill="black")
        image.save(path, "PDF", resolution=72)
        return PdfReader(path).pages[0]

    def write_pdf(self, writer, filename="sample.pdf"):
        path = self.root / filename
        with path.open("wb") as stream:
            writer.write(stream)
        return path

    def test_digital_text_page_needs_no_ocr_models_and_keeps_location(self):
        writer = PdfWriter()
        self.text_page(writer, "Quarterly report 2026")
        path = self.write_pdf(writer)
        with patch("moasearch.pdf.recognize_frame") as ocr:
            self.assertEqual(extract_pdf(path, 1000, self.model_dir),
                             [("Quarterly report 2026", "1페이지")])
        ocr.assert_not_called()

    def test_scanned_image_page_is_really_rendered_for_ocr(self):
        writer = PdfWriter()
        writer.add_page(self.image_pdf())
        path = self.write_pdf(writer)
        seen = []

        def recognize(image, max_chars, model_dir):
            seen.append((image.size, image.mode, max_chars, model_dir))
            return "스캔 계약 승인 번호 SCAN-5521"

        with patch("moasearch.pdf.recognize_frame", side_effect=recognize):
            result = extract_pdf(path, 1000, self.model_dir)
        self.assertEqual(result, [("스캔 계약 승인 번호 SCAN-5521", "1페이지 OCR")])
        self.assertEqual(len(seen), 1)
        self.assertEqual(seen[0][1:], ("RGB", 1000, self.model_dir))
        self.assertGreater(seen[0][0][0], 400)
        self.assertLessEqual(seen[0][0][0] * seen[0][0][1], 8_000_000)

    def test_mixed_digital_and_scanned_pages_preserve_each_page_location(self):
        writer = PdfWriter()
        self.text_page(writer, "Existing digital page")
        writer.add_page(self.image_pdf("second.pdf"))
        path = self.write_pdf(writer)
        with patch("moasearch.pdf.recognize_frame", return_value="스캔한 두 번째 페이지") as ocr:
            result = extract_pdf(path, 1000, self.model_dir)
        self.assertEqual(result, [
            ("Existing digital page", "1페이지"),
            ("스캔한 두 번째 페이지", "2페이지 OCR"),
        ])
        ocr.assert_called_once()

    def test_sparse_header_and_image_text_are_merged(self):
        writer = PdfWriter()
        page = self.text_page(writer, "FORM")
        page.merge_page(self.image_pdf("header.pdf"))
        path = self.write_pdf(writer)
        with patch("moasearch.pdf.recognize_frame", return_value="FORM\n청구 합계 52,000원"):
            result = extract_pdf(path, 1000, self.model_dir)
        self.assertEqual(result[0][0], "FORM\n청구 합계 52,000원")
        self.assertEqual(result[0][1], "1페이지 OCR")

    def test_blank_page_needs_no_ocr(self):
        writer = PdfWriter()
        writer.add_blank_page(width=612, height=792)
        path = self.write_pdf(writer)
        with patch("moasearch.pdf.recognize_frame") as ocr:
            self.assertEqual(extract_pdf(path, 1000, self.model_dir), [])
        ocr.assert_not_called()

    def test_failure_keeps_prior_pages_and_reads_later_pages(self):
        writer = PdfWriter()
        writer.add_page(self.image_pdf("first.pdf"))
        writer.add_page(self.image_pdf("second.pdf"))
        path = self.write_pdf(writer)
        calls = 0

        def recognize(*_args):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise RuntimeError("OCR model is corrupt")
            return "두 번째 스캔의 검색할 내용"

        with patch("moasearch.pdf.recognize_frame", side_effect=recognize):
            with self.assertRaises(PdfReadError) as caught:
                extract_pdf(path, 1000, self.model_dir)
        self.assertEqual(caught.exception.passages,
                         [("두 번째 스캔의 검색할 내용", "2페이지 OCR")])
        self.assertIn("1페이지", str(caught.exception))
        self.assertEqual(calls, 2)

    def test_failure_keeps_text_before_and_after_failed_scan_through_extractor(self):
        writer = PdfWriter()
        self.text_page(writer, "First digital page")
        writer.add_page(self.image_pdf("failed-scan.pdf"))
        self.text_page(writer, "Third digital page")
        path = self.write_pdf(writer)
        with patch("moasearch.pdf.recognize_frame", side_effect=RuntimeError("OCR unavailable")):
            with self.assertRaises(ExtractionError) as caught:
                extract(path, 1000, model_dir=self.model_dir)
        self.assertEqual(caught.exception.passages, [
            Passage("First digital page", "1페이지"),
            Passage("Third digital page", "3페이지"),
        ])
        self.assertIn("2페이지", str(caught.exception))

    def test_rotated_render_is_rotated_and_copy_survives_native_close(self):
        writer = PdfWriter()
        writer.add_page(self.image_pdf("rotated.pdf"))
        writer.pages[0].rotate(90)
        path = self.write_pdf(writer)
        renderer = _Renderer(path.read_bytes())
        rendered = renderer.render(0)
        renderer.close()
        try:
            self.assertGreater(rendered.height, rendered.width)
            # The source image has a large, unmistakable black rectangle on white.
            self.assertEqual(rendered.getpixel((rendered.width // 2, rendered.height // 2)), (0, 0, 0))
            self.assertEqual(rendered.getpixel((0, 0)), (255, 255, 255))
        finally:
            rendered.close()

    def test_native_bitmap_and_page_close_when_to_pil_raises(self):
        state = {"page_closed": False, "bitmap_closed": False, "document_closed": False}

        class Bitmap:
            def to_pil(self):
                raise RuntimeError("conversion failed")

            def close(self):
                state["bitmap_closed"] = True

        class Page:
            def get_size(self):
                return (612, 792)

            def render(self, *, scale):
                self.scale = scale
                return Bitmap()

            def close(self):
                state["page_closed"] = True

        class Document:
            def __getitem__(self, index):
                return Page()

            def close(self):
                state["document_closed"] = True

        with patch.dict("sys.modules", {"pypdfium2": SimpleNamespace(PdfDocument=lambda _raw: Document())}):
            renderer = _Renderer(b"synthetic PDF")
            with self.assertRaisesRegex(RuntimeError, "conversion failed"):
                renderer.render(0)
            renderer.close()
        self.assertEqual(state, {"page_closed": True, "bitmap_closed": True, "document_closed": True})

    def test_cancel_is_checked_between_pages(self):
        writer = PdfWriter()
        self.text_page(writer, "First page text")
        self.text_page(writer, "Second page text")
        path = self.write_pdf(writer)
        cancelled = threading.Event()

        def progress(page, _total, _phase):
            if page == 2:
                cancelled.set()

        with self.assertRaisesRegex(InterruptedError, "취소"):
            extract_pdf(path, 1000, self.model_dir, cancelled=cancelled.is_set, progress=progress)

    def test_progress_reports_text_and_ocr_phases(self):
        writer = PdfWriter()
        self.text_page(writer, "Digital first")
        writer.add_page(self.image_pdf("progress.pdf"))
        path = self.write_pdf(writer)
        events = []
        with patch("moasearch.pdf.recognize_frame", return_value="스캔 텍스트"):
            extract_pdf(path, 1000, self.model_dir,
                        progress=lambda *event: events.append(event))
        self.assertEqual(events, [(1, 2, "pdf_text"), (2, 2, "pdf_text"), (2, 2, "pdf_ocr")])

    def test_limits_chars_pages_and_pdf_bytes(self):
        writer = PdfWriter()
        self.text_page(writer, "1234567890")
        self.text_page(writer, "second page has content")
        path = self.write_pdf(writer)
        self.assertEqual(extract_pdf(path, 5, self.model_dir), [("12345", "1페이지")])
        with patch("moasearch.pdf._MAX_PAGES", 1):
            self.assertEqual(extract_pdf(path, 1000, self.model_dir), [("1234567890", "1페이지")])
        with patch("moasearch.pdf._MAX_PDF_BYTES", 10):
            with self.assertRaisesRegex(ValueError, "100 MiB"):
                extract_pdf(path, 1000, self.model_dir)

    def test_encrypted_and_invalid_pdf_fail_cleanly(self):
        writer = PdfWriter()
        self.text_page(writer, "secret")
        writer.encrypt("password")
        encrypted = self.write_pdf(writer, "encrypted.pdf")
        with self.assertRaisesRegex(ValueError, "암호화"):
            extract_pdf(encrypted, 1000, self.model_dir)
        invalid = self.root / "invalid.pdf"
        invalid.write_bytes(b"not a PDF")
        with self.assertRaises(Exception):
            extract_pdf(invalid, 1000, self.model_dir)

    def test_render_scale_bounds_and_rejects_invalid_page_sizes(self):
        scale = _render_scale(1_000_000, 1_000_000)
        from math import ceil
        self.assertLessEqual(ceil(1_000_000 * scale) ** 2, 8_000_000)
        self.assertLessEqual(ceil(1_000_000 * scale), 3200)
        for size in ((0, 100), (-2, 10), (float("inf"), 50)):
            with self.subTest(size=size), self.assertRaisesRegex(ValueError, "크기"):
                _render_scale(*size)


if __name__ == "__main__":
    unittest.main()
