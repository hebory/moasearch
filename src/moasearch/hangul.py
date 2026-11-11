"""Bounded extraction of text from HWP 5 and HWPX documents."""

from __future__ import annotations

from pathlib import Path
import re
import struct
import zipfile
import zlib
from defusedxml import ElementTree
from defusedxml.common import DefusedXmlException

_MAX_SOURCE = 100 * 1024 * 1024
_MAX_ZIP_ENTRIES = 30_000
_MAX_ZIP_TOTAL = 128 * 1024 * 1024
_MAX_ENTRY = 16 * 1024 * 1024
_MAX_BODY = 128 * 1024 * 1024
_HWP_SIGNATURE = b"HWP Document File"


def _limit(max_chars: int) -> None:
    if max_chars < 1:
        raise ValueError("max_chars는 1 이상이어야 합니다.")


def _source_size(path: Path) -> None:
    try:
        size = path.stat().st_size
    except OSError as exc:
        raise ValueError(f"파일을 확인할 수 없습니다: {exc}") from exc
    if size > _MAX_SOURCE:
        raise ValueError("한글 문서가 허용 크기(100 MiB)를 넘었습니다.")


def extract_hwp(path: Path, max_chars: int = 200_000) -> list[tuple[str, str]]:
    """Read text from HWP 5 Section streams; previews are never consulted."""
    path = Path(path)
    _limit(max_chars)
    _source_size(path)
    try:
        import olefile
    except ImportError as exc:
        raise RuntimeError("HWP 읽기에는 olefile 패키지가 필요합니다.") from exc

    try:
        ole = olefile.OleFileIO(str(path), write_mode=False)
    except Exception as exc:
        raise ValueError(f"HWP OLE 컨테이너를 읽을 수 없습니다: {exc}") from exc
    try:
        if not ole.exists("FileHeader"):
            raise ValueError("HWP 5 FileHeader가 없습니다. HWP 3 형식은 지원하지 않습니다.")
        header_stream = ole.openstream("FileHeader")
        header = header_stream.read(257)
        if len(header) < 40 or header[: len(_HWP_SIGNATURE)] != _HWP_SIGNATURE:
            raise ValueError("유효한 HWP 5 FileHeader가 아닙니다. HWP 3 형식은 지원하지 않습니다.")
        if len(header) < 256:
            raise ValueError("HWP FileHeader가 잘렸습니다.")
        if header[35] != 5:
            raise ValueError("지원하지 않는 HWP 문서 버전입니다. HWP 5 형식만 지원합니다.")
        flags = struct.unpack_from("<I", header, 36)[0]
        if flags & (1 << 1):
            raise ValueError("암호화된 HWP 문서는 검색할 수 없습니다.")
        if flags & (1 << 2):
            raise ValueError("배포용(편집 제한) HWP 문서는 검색할 수 없습니다.")
        if flags & (1 << 4):
            raise ValueError("DRM 보호 HWP 문서는 검색할 수 없습니다.")
        if flags & (1 << 8):
            raise ValueError("인증서 암호화 HWP 문서는 검색할 수 없습니다.")
        if flags & (1 << 10):
            raise ValueError("인증서 DRM 보호 HWP 문서는 검색할 수 없습니다.")
        compressed = bool(flags & 1)
        names = [parts for parts in ole.listdir(streams=True, storages=False)
                 if len(parts) == 2 and parts[0] == "BodyText"
                 and re.fullmatch(r"Section\d+", parts[1])]
        names.sort(key=lambda parts: int(parts[1][7:]))
        if not names:
            raise ValueError("HWP 본문 Section 스트림이 없습니다.")
        passages: list[tuple[str, str]] = []
        remaining = max_chars
        inflated_total = 0
        for parts in names:
            stream = ole.openstream(parts)
            if getattr(stream, "size", 0) > _MAX_BODY:
                raise ValueError("HWP 본문 스트림이 허용 크기(128 MiB)를 넘었습니다.")
            raw = stream.read(_MAX_BODY + 1)
            if len(raw) > _MAX_BODY:
                raise ValueError("HWP 본문 스트림이 허용 크기(128 MiB)를 넘었습니다.")
            body = _inflate(raw, _MAX_BODY - inflated_total) if compressed else raw
            inflated_total += len(body)
            if inflated_total > _MAX_BODY:
                raise ValueError("HWP 본문 압축 해제 크기가 허용 한도(128 MiB)를 넘었습니다.")
            text = _records_text(body, remaining)
            if text:
                label = f"섹션 {int(parts[1][7:]) + 1}"
                passages.append((text, label))
                remaining -= len(text)
            if remaining <= 0:
                break
        return passages
    finally:
        ole.close()


def _inflate(raw: bytes, limit: int) -> bytes:
    if limit < 0:
        raise ValueError("HWP 본문 압축 해제 크기가 허용 한도(128 MiB)를 넘었습니다.")
    inflater = zlib.decompressobj(-15)
    try:
        out = inflater.decompress(raw, limit + 1)
        if len(out) > limit or inflater.unconsumed_tail:
            raise ValueError("HWP 본문 압축 해제 크기가 허용 한도(128 MiB)를 넘었습니다.")
        out += inflater.flush(limit + 1 - len(out))
    except zlib.error as exc:
        raise ValueError(f"HWP 본문 압축 데이터가 손상되었습니다: {exc}") from exc
    if len(out) > limit:
        raise ValueError("HWP 본문 압축 해제 크기가 허용 한도(128 MiB)를 넘었습니다.")
    if not inflater.eof or inflater.unused_data:
        raise ValueError("HWP 본문 압축 데이터가 잘렸거나 뒤에 불필요한 데이터가 있습니다.")
    return out


# Control codes whose representation consumes the control WCHAR and seven
# following WCHARs. See Hancom's HWP 5 file-format specification.
_EXTENDED_CONTROLS = {1, 2, 3, 6, 7, 11, 12, 14, 15, 16, 17, 18, 21, 22, 23}
_INLINE_CONTROLS = {4, 5, 8, 19, 20}


def _records_text(data: bytes, max_chars: int) -> str:
    result: list[str] = []
    pos = 0
    total = 0
    while pos < len(data) and total < max_chars:
        if len(data) - pos < 4:
            raise ValueError("HWP 레코드 헤더가 잘렸습니다.")
        header = struct.unpack_from("<I", data, pos)[0]
        pos += 4
        tag = header & 0x3FF
        size = (header >> 20) & 0xFFF
        if size == 0xFFF:
            if len(data) - pos < 4:
                raise ValueError("HWP 확장 레코드 길이가 잘렸습니다.")
            size = struct.unpack_from("<I", data, pos)[0]
            pos += 4
        end = pos + size
        if end > len(data):
            raise ValueError("HWP 레코드 본문이 파일 범위를 벗어났습니다.")
        if tag == 0x43:  # PARA_TEXT
            payload = data[pos:end]
            if len(payload) % 2:
                raise ValueError("HWP 문단 텍스트의 UTF-16 데이터가 잘못되었습니다.")
            units = memoryview(payload)
            unit_count = len(units) // 2
            i = 0
            chars: list[str] = []
            while i < unit_count and total + len(chars) < max_chars:
                value = struct.unpack_from("<H", units, i * 2)[0]
                if value == 0x09:
                    if i + 8 > unit_count:
                        raise ValueError("HWP 문단 제어 문자가 잘렸습니다.")
                    chars.append("\t")
                    i += 8
                    continue
                if value in _EXTENDED_CONTROLS or value in _INLINE_CONTROLS:
                    if i + 8 > unit_count:
                        raise ValueError("HWP 문단 제어 문자가 잘렸습니다.")
                    i += 8
                    continue
                if value == 0x0A or value == 0x0D:
                    chars.append("\n")
                elif value == 0x1E or value == 0x1F:
                    chars.append(" ")
                elif value == 0x18:
                    chars.append("-")
                elif 0xD800 <= value <= 0xDBFF:
                    if i + 1 >= unit_count:
                        raise ValueError("HWP 문단 텍스트의 UTF-16 서로게이트가 잘못되었습니다.")
                    low = struct.unpack_from("<H", units, (i + 1) * 2)[0]
                    if not 0xDC00 <= low <= 0xDFFF:
                        raise ValueError("HWP 문단 텍스트의 UTF-16 서로게이트가 잘못되었습니다.")
                    chars.append(chr(0x10000 + ((value - 0xD800) << 10) + low - 0xDC00))
                    i += 2
                    continue
                elif 0xDC00 <= value <= 0xDFFF:
                    raise ValueError("HWP 문단 텍스트의 UTF-16 서로게이트가 잘못되었습니다.")
                elif value >= 0x20:
                    chars.append(chr(value))
                i += 1
            paragraph = "".join(chars)
            if paragraph:
                if result and not result[-1].endswith("\n"):
                    result.append("\n")
                result.append(paragraph)
                total += len(paragraph)
        pos = end
    return "".join(result)[:max_chars].strip()


def extract_hwpx(path: Path, max_chars: int = 200_000) -> list[tuple[str, str]]:
    """Extract actual HWPX section XML text in document order."""
    path = Path(path)
    _limit(max_chars)
    _source_size(path)
    try:
        archive = zipfile.ZipFile(path)
    except (OSError, zipfile.BadZipFile) as exc:
        raise ValueError(f"HWPX 압축 파일을 읽을 수 없습니다: {exc}") from exc
    try:
        infos = archive.infolist()
        if len(infos) > _MAX_ZIP_ENTRIES:
            raise ValueError("HWPX 압축 항목 수가 허용 한도(30,000개)를 넘었습니다.")
        if sum(info.file_size for info in infos) > _MAX_ZIP_TOTAL:
            raise ValueError("HWPX 압축 해제 크기가 허용 한도(128 MiB)를 넘었습니다.")
        if any(info.file_size > _MAX_ENTRY for info in infos):
            raise ValueError("HWPX 내부 파일이 허용 크기(16 MiB)를 넘었습니다.")
        by_name: dict[str, zipfile.ZipInfo] = {}
        for info in infos:
            if info.filename in by_name:
                raise ValueError("HWPX에 중복된 압축 항목 이름이 있습니다.")
            by_name[info.filename] = info
        manifest = by_name.get("META-INF/manifest.xml")
        if manifest:
            raw_manifest = _zip_read(archive, manifest)
            if b"encryption-data" in raw_manifest.lower() or b"encrypted" in raw_manifest.lower():
                raise ValueError("암호화된 HWPX 문서는 검색할 수 없습니다.")
        sections = [(int(match.group(1)), name, info) for name, info in by_name.items()
                    if (match := re.fullmatch(r"Contents/section(\d+)\.xml", name))]
        sections.sort(key=lambda item: item[0])
        if not sections:
            raise ValueError("HWPX 본문 Contents/sectionN.xml이 없습니다.")
        output: list[tuple[str, str]] = []
        remaining = max_chars
        for number, _, info in sections:
            raw = _zip_read(archive, info)
            try:
                root = ElementTree.fromstring(
                    raw, forbid_dtd=True, forbid_entities=True, forbid_external=True
                )
            except (ElementTree.ParseError, DefusedXmlException) as exc:
                raise ValueError(f"HWPX 섹션 {number + 1} XML이 손상되었습니다: {exc}") from exc
            text = _hwpx_text(root, remaining)
            if text:
                output.append((text, f"섹션 {number + 1}"))
                remaining -= len(text)
            if remaining <= 0:
                break
        return output
    finally:
        archive.close()


def _zip_read(archive: zipfile.ZipFile, info: zipfile.ZipInfo) -> bytes:
    try:
        with archive.open(info) as stream:
            raw = stream.read(_MAX_ENTRY + 1)
    except (OSError, zipfile.BadZipFile, RuntimeError, zlib.error) as exc:
        raise ValueError(f"HWPX 압축 항목을 읽을 수 없습니다: {exc}") from exc
    if len(raw) > _MAX_ENTRY or len(raw) != info.file_size:
        raise ValueError("HWPX 내부 파일 크기가 헤더와 일치하지 않습니다.")
    return raw


def _hwpx_text(root: ElementTree.Element, max_chars: int) -> str:
    """Walk XML order and keep hp:t nodes under paragraph nodes only."""
    pieces: list[str] = []
    count = 0
    def append(piece: str) -> None:
        nonlocal count
        piece = piece[:max_chars - count]
        if piece:
            pieces.append(piece)
            count += len(piece)

    def walk(element: ElementTree.Element, in_paragraph: bool = False) -> None:
        nonlocal count
        if count >= max_chars:
            return
        local = element.tag.rsplit("}", 1)[-1] if isinstance(element.tag, str) else ""
        active = in_paragraph or local == "p"
        if active and local == "t" and element.text:
            append(element.text)
        elif active and local in {"lineBreak", "br"}:
            append("\n")
        elif active and local == "tab" and element.text is None:
            append("\t")
        for child in element:
            walk(child, active)
            if active and child.tail:
                append(child.tail)
        if local == "p" and active and count < max_chars:
            if pieces and not pieces[-1].endswith("\n"):
                pieces.append("\n")
                count += 1
    walk(root)
    return "".join(pieces).strip()
