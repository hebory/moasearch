"""Bounded, local-only text extraction for files indexed by MoaSearch."""

from __future__ import annotations

from dataclasses import dataclass
from html.parser import HTMLParser
import io
from pathlib import Path
import re
import zipfile
from xml.etree import ElementTree


@dataclass(frozen=True, slots=True)
class Passage:
    text: str
    location: str


_TEXT_SUFFIXES = {
    ".txt", ".text", ".md", ".markdown", ".rst", ".log", ".csv", ".tsv",
    ".json", ".jsonl", ".xml", ".yaml", ".yml", ".toml", ".ini", ".cfg",
    ".conf", ".html", ".htm", ".css", ".js", ".jsx", ".ts", ".tsx",
    ".py", ".pyi", ".ipynb", ".java", ".kt", ".kts", ".scala", ".go",
    ".rs", ".c", ".h", ".cc", ".cpp", ".hpp", ".cs", ".fs", ".vb",
    ".php", ".rb", ".swift", ".m", ".mm", ".sh", ".bash", ".ps1",
    ".bat", ".cmd", ".sql", ".graphql", ".proto", ".dockerfile", ".make",
    ".gradle", ".properties", ".tex", ".bib", ".srt", ".vtt",
}
_ARCHIVE_SUFFIXES = {".docx", ".pptx", ".xlsx", ".odt", ".odp", ".ods"}
IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".bmp", ".tif", ".tiff", ".webp"}
_MAX_ARCHIVE_ENTRIES = 30_000
_MAX_ENTRY_BYTES = 16 * 1024 * 1024
_MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
_MAX_PDF_BYTES = 100 * 1024 * 1024
_MAX_PDF_PAGES = 250


def supports_content(path: Path) -> bool:
    """Return whether the filename has a supported text/document format."""
    suffix = path.suffix.lower()
    return (suffix in _TEXT_SUFFIXES or suffix == ".pdf" or suffix in _ARCHIVE_SUFFIXES
            or suffix in {".hwp", ".hwpx"} or suffix in IMAGE_SUFFIXES)


def _decode(data: bytes) -> str:
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        return data.decode("utf-16", errors="replace")
    if data.startswith(b"\xef\xbb\xbf"):
        return data.decode("utf-8-sig", errors="replace")
    try:
        value = data.decode("utf-8")
    except UnicodeDecodeError:
        try:
            value = data.decode("cp949")
        except UnicodeDecodeError:
            value = data.decode("utf-8", errors="replace")
    if value and sum(ord(c) < 32 and c not in "\n\r\t\f" for c in value) / len(value) > 0.02:
        return ""
    return value


class _VisibleText(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self.hidden = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag.lower() in {"script", "style", "noscript", "svg"}:
            self.hidden += 1
        elif tag.lower() in {"br", "p", "div", "li", "tr", "h1", "h2", "h3"}:
            self.parts.append("\n")

    def handle_endtag(self, tag: str) -> None:
        if tag.lower() in {"script", "style", "noscript", "svg"} and self.hidden:
            self.hidden -= 1
        elif tag.lower() in {"p", "div", "li", "tr", "h1", "h2", "h3"}:
            self.parts.append("\n")

    def handle_data(self, data: str) -> None:
        if not self.hidden:
            self.parts.append(data)


def _html_text(text: str) -> str:
    parser = _VisibleText()
    parser.feed(text)
    parser.close()
    return re.sub(r"[ \t\r\f\v]+", " ", "".join(parser.parts)).strip()


def _plain(path: Path, max_chars: int) -> list[Passage]:
    # Four bytes per requested Unicode character covers UTF-8 and BOMs while
    # keeping the file read bounded even when its reported size is enormous.
    with path.open("rb") as stream:
        raw = stream.read(max_chars * 4 + 4)
    text = _decode(raw)
    if path.suffix.lower() in {".html", ".htm"}:
        text = _html_text(text)
    text = text[:max_chars].strip()
    return [Passage(text, "본문")] if text else []


def _xml_text(raw: bytes) -> str:
    # ElementTree does not fetch external entities; archive members are also
    # size-bounded before parsing.
    root = ElementTree.fromstring(raw)
    return " ".join(t.strip() for t in root.itertext() if t and t.strip())


def _elements(root: ElementTree.Element, local_name: str):
    return (element for element in root.iter() if element.tag.rsplit("}", 1)[-1] == local_name)


def _archive_members(path: Path) -> tuple[zipfile.ZipFile, dict[str, zipfile.ZipInfo]]:
    archive = zipfile.ZipFile(path)
    infos = archive.infolist()
    if len(infos) > _MAX_ARCHIVE_ENTRIES:
        archive.close()
        raise ValueError("문서 압축 항목 수가 허용 한도를 넘었습니다.")
    if sum(i.file_size for i in infos) > _MAX_ARCHIVE_BYTES:
        archive.close()
        raise ValueError("문서의 압축 해제 크기가 허용 한도를 넘었습니다.")
    if any(i.file_size > _MAX_ENTRY_BYTES for i in infos):
        archive.close()
        raise ValueError("문서 내부 파일이 허용 크기를 넘었습니다.")
    return archive, {i.filename: i for i in infos}


def _member(archive: zipfile.ZipFile, info: zipfile.ZipInfo) -> bytes:
    # Limit bytes during decompression too; do not trust only the ZIP headers.
    with archive.open(info) as stream:
        data = stream.read(_MAX_ENTRY_BYTES + 1)
    if len(data) > _MAX_ENTRY_BYTES or len(data) != info.file_size:
        raise ValueError("문서 내부 파일 크기가 헤더와 일치하지 않습니다.")
    return data


def _ooxml(path: Path, max_chars: int) -> list[Passage]:
    archive, members = _archive_members(path)
    passages: list[Passage] = []
    consumed = 0
    try:
        suffix = path.suffix.lower()
        if suffix == ".docx":
            targets = [("word/document.xml", "본문")]
        elif suffix == ".pptx":
            targets = [(name, f"슬라이드 {i}") for i, name in enumerate(
                sorted((n for n in members if re.fullmatch(r"ppt/slides/slide\d+\.xml", n)),
                       key=lambda n: int(re.search(r"slide(\d+)", n).group(1))), 1)]
        elif suffix == ".xlsx":
            shared: list[str] = []
            info = members.get("xl/sharedStrings.xml")
            if info:
                root = ElementTree.fromstring(_member(archive, info))
                shared = [" ".join(t.text or "" for t in _elements(item, "t")) for item in _elements(root, "si")]
            targets = [(n, f"시트 {i}") for i, n in enumerate(
                sorted((n for n in members if re.fullmatch(r"xl/worksheets/sheet\d+\.xml", n)),
                       key=lambda n: int(re.search(r"sheet(\d+)", n).group(1))), 1)]
            for name, label in targets:
                info = members[name]
                root = ElementTree.fromstring(_member(archive, info))
                cells = []
                for cell in _elements(root, "c"):
                    if cell.get("t") == "inlineStr":
                        inline = next(_elements(cell, "is"), None)
                        if inline is not None:
                            value = " ".join(t.text or "" for t in _elements(inline, "t"))
                            if value:
                                cells.append(value)
                        continue
                    value = next(_elements(cell, "v"), None)
                    if value is None or value.text is None:
                        continue
                    if cell.get("t") == "s":
                        try:
                            cells.append(shared[int(value.text)])
                        except (ValueError, IndexError):
                            continue
                    else:
                        cells.append(value.text)
                text = " ".join(cells)
                if text:
                    text = text[:max_chars - consumed]
                    if text:
                        passages.append(Passage(text, label))
                        consumed += len(text)
                if consumed >= max_chars:
                    break
            return passages
        else:  # OpenDocument text content
            targets = [("content.xml", "본문")]

        for name, label in targets:
            info = members.get(name)
            if not info:
                continue
            text = _xml_text(_member(archive, info))
            if text:
                text = text[:max_chars - consumed]
                if text:
                    passages.append(Passage(text, label))
                    consumed += len(text)
            if consumed >= max_chars:
                break
    finally:
        archive.close()
    return passages


def _pdf(path: Path, max_chars: int) -> list[Passage]:
    if path.stat().st_size > _MAX_PDF_BYTES:
        raise ValueError("PDF 파일이 허용 크기(100 MiB)를 넘었습니다.")
    try:
        from pypdf import PdfReader
    except ImportError as exc:
        raise RuntimeError("PDF 읽기에는 pypdf 패키지가 필요합니다.") from exc
    raw = path.read_bytes()
    reader = PdfReader(io.BytesIO(raw), strict=False)
    passages: list[Passage] = []
    consumed = 0
    for i, page in enumerate(reader.pages[:_MAX_PDF_PAGES], 1):
        text = (page.extract_text() or "").strip()
        text = text[:max_chars - consumed].strip()
        if text:
            passages.append(Passage(text, f"{i}페이지"))
            consumed += len(text)
        if consumed >= max_chars:
            break
    return passages


def extract(path: Path, max_chars: int = 200_000, *, model_dir: Path | None = None) -> list[Passage]:
    """Extract bounded, location-labelled text from a supported local file.

    Unsupported files (including unknown binary files) return an empty list.
    Parser errors are surfaced so the indexer can report an unreadable file.
    """
    if max_chars < 1:
        raise ValueError("max_chars must be positive")
    path = Path(path)
    suffix = path.suffix.lower()
    if suffix in {".hwp", ".hwpx"}:
        from .hangul import extract_hwp, extract_hwpx
        reader = extract_hwp if suffix == ".hwp" else extract_hwpx
        return [Passage(text, location) for text, location in reader(path, max_chars)]
    if suffix in IMAGE_SUFFIXES:
        from .config import data_directory
        from .ocr import extract_image
        directory = Path(model_dir) if model_dir is not None else data_directory() / "models"
        return [Passage(text, location) for text, location in extract_image(path, max_chars, directory)]
    if suffix == ".pdf":
        return _pdf(path, max_chars)
    if suffix in _ARCHIVE_SUFFIXES:
        return _ooxml(path, max_chars)
    if suffix in _TEXT_SUFFIXES:
        return _plain(path, max_chars)
    return []


def chunk_passages(
    passages: list[Passage], chunk_size: int = 1200, overlap: int = 160
) -> list[Passage]:
    """Split passages into bounded overlapping chunks, retaining their location."""
    if chunk_size < 1 or overlap < 0 or overlap >= chunk_size:
        raise ValueError("chunk_size must be positive and overlap must be smaller than it")
    result: list[Passage] = []
    step = chunk_size - overlap
    for passage in passages:
        text = passage.text.strip()
        start = 0
        while start < len(text):
            end = min(start + chunk_size, len(text))
            if end < len(text):
                boundary = max(text.rfind("\n", start + chunk_size // 2, end),
                               text.rfind(" ", start + chunk_size // 2, end))
                if boundary > start:
                    end = boundary
            piece = text[start:end].strip()
            if piece:
                result.append(Passage(piece, passage.location))
            if end >= len(text):
                break
            start = max(start + 1, end - overlap)
    return result
