"""PDF text extraction with bounded, in-process OCR of scanned pages."""

from __future__ import annotations

import io
import math
from pathlib import Path
import re
import threading

from .ocr import recognize_frame

_MAX_PDF_BYTES = 100 * 1024 * 1024
_MAX_PAGES = 250
_OCR_TEXT_THRESHOLD = 40
_RENDER_DPI = 200
_MAX_RENDER_SIDE = 3200
_MAX_RENDER_PIXELS = 8_000_000
# PDFium is not thread safe, even for operations on different documents.
_pdfium_lock = threading.RLock()


class PdfReadError(ValueError):
    """Preserve successfully read pages when another page cannot be processed."""

    def __init__(self, message, passages):
        super().__init__(message)
        self.passages = passages


def _render_scale(width, height):
    if not all(math.isfinite(v) and v > 0 for v in (width, height)):
        raise ValueError("PDF 페이지 크기가 올바르지 않습니다.")
    scale = min(_RENDER_DPI / 72, _MAX_RENDER_SIDE / width,
                _MAX_RENDER_SIDE / height,
                math.sqrt(_MAX_RENDER_PIXELS / width / height)) * .999
    pixels = (math.ceil(width * scale), math.ceil(height * scale))
    if (min(pixels) < 1 or max(pixels) > _MAX_RENDER_SIDE
            or pixels[0] * pixels[1] > _MAX_RENDER_PIXELS):
        raise ValueError("PDF 페이지 렌더링 해상도가 허용 한도를 넘었습니다.")
    return scale


class _Renderer:
    """Keep one lazy native document; return pixels independent of PDFium."""

    def __init__(self, raw):
        self.raw, self.document = raw, None

    def render(self, index):
        with _pdfium_lock:
            if self.document is None:
                import pypdfium2 as pdfium
                self.document = pdfium.PdfDocument(self.raw)
            page = self.document[index]
            try:
                scale = _render_scale(*page.get_size())
                bitmap = page.render(scale=scale)
                try:
                    # to_pil may share the native buffer. Copy before closing it.
                    image = bitmap.to_pil()
                    try:
                        return image.convert("RGB").copy()
                    finally:
                        image.close()
                finally:
                    bitmap.close()
            finally:
                page.close()

    def close(self):
        with _pdfium_lock:
            if self.document is not None:
                self.document.close()
                self.document = None


def _has_images(page):
    """Inspect resource metadata only, without decoding embedded image data."""
    pending = [(page.get("/Resources", {}), 0)]
    seen = set()
    for _ in range(1000):
        if not pending:
            return False
        resources, depth = pending.pop()
        if hasattr(resources, "get_object"):
            resources = resources.get_object()
        objects = resources.get("/XObject", {})
        if hasattr(objects, "get_object"):
            objects = objects.get_object()
        for ref in objects.values():
            obj = ref.get_object()
            identity = id(obj)
            if identity in seen:
                continue
            seen.add(identity)
            if obj.get("/Subtype") == "/Image":
                return True
            if obj.get("/Subtype") == "/Form" and depth < 8 and "/Resources" in obj:
                pending.append((obj["/Resources"], depth + 1))
    # An unusually complex resource tree is safer to render than to skip.
    return True


def _merge_text(existing, recognized):
    """Keep reliable text and append OCR lines not already represented in it."""
    normalize = lambda value: re.sub(r"\s+", "", value).casefold()
    known = normalize(existing)
    additions = []
    for line in recognized.splitlines():
        compact = normalize(line)
        if compact and compact not in known:
            additions.append(line)
            known += compact
    return "\n".join([part for part in (existing, "\n".join(additions)) if part])


def extract_pdf(path: Path, max_chars: int, model_dir: Path, *, cancelled=None, progress=None):
    """Return (text, page location) pairs, retaining partial text on failure.

    Textless pages with content, or image pages with fewer than 40 letters and
    digits, are OCR candidates. Sufficient text layers are reused as-is.
    Cancellation is checked between pages and before/after native OCR calls.
    """
    from pypdf import PdfReader

    if max_chars < 1:
        raise ValueError("max_chars는 1 이상이어야 합니다.")
    path = Path(path)
    if path.stat().st_size > _MAX_PDF_BYTES:
        raise ValueError("PDF 파일이 허용 크기(100 MiB)를 넘었습니다.")
    with path.open("rb") as stream:
        raw = stream.read(_MAX_PDF_BYTES + 1)
    if len(raw) > _MAX_PDF_BYTES:
        raise ValueError("PDF 파일이 허용 크기(100 MiB)를 넘었습니다.")
    reader = PdfReader(io.BytesIO(raw), strict=False)
    if reader.is_encrypted:
        raise ValueError("암호화된 PDF는 본문을 읽을 수 없습니다.")
    renderer = _Renderer(raw)
    passages, errors = [], []
    consumed = 0
    total_pages = min(len(reader.pages), _MAX_PAGES)

    def check_cancel():
        if cancelled and cancelled():
            raise InterruptedError("PDF 읽기를 취소했습니다.")

    try:
        for index in range(total_pages):
            check_cancel()
            if progress:
                progress(index + 1, total_pages, "pdf_text")
            text, ocr_used = "", False
            try:
                page = reader.pages[index]
                extraction_error = None
                try:
                    text = (page.extract_text() or "").strip()
                except Exception as exc:
                    extraction_error = exc
                sparse = sum(char.isalnum() for char in text) < _OCR_TEXT_THRESHOLD
                needs_ocr = extraction_error is not None or (
                    sparse and (_has_images(page) or (not text and bool(page.get("/Contents")))))
                if needs_ocr:
                    check_cancel()
                    if progress:
                        progress(index + 1, total_pages, "pdf_ocr")
                    with renderer.render(index) as image:
                        check_cancel()
                        recognized = recognize_frame(image, max_chars - consumed, model_dir)
                    check_cancel()
                    text = _merge_text(text, recognized)
                    ocr_used = True
            except InterruptedError:
                raise
            except Exception as exc:
                errors.append(f"{index + 1}페이지: {str(exc)[:250]}")
            text = text[:max_chars - consumed].strip()
            if text:
                passages.append((text, f"{index + 1}페이지" + (" OCR" if ocr_used else "")))
                consumed += len(text)
            if consumed >= max_chars:
                break
        check_cancel()
    finally:
        renderer.close()
    if errors:
        detail = "; ".join(errors[:3])
        raise PdfReadError(f"PDF {len(errors)}개 페이지 읽기 실패. {detail}", passages)
    return passages
