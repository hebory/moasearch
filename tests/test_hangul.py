from __future__ import annotations

import io
import struct
import sys
import tempfile
import types
import unittest
import zipfile
import zlib
from pathlib import Path
from unittest.mock import patch

from moasearch.hangul import extract_hwp, extract_hwpx


class HangulExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix=".moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def make_hwpx(self, entries: dict[str, bytes]) -> Path:
        path = self.root / "sample.hwpx"
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
            for name, data in entries.items():
                archive.writestr(name, data)
        return path

    def test_hwpx_reads_sections_paragraphs_and_table_in_xml_order(self) -> None:
        xml = '''<?xml version="1.0" encoding="UTF-8"?>
        <hs:sec xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section"
          xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph">
        <hp:p><hp:run><hp:t>첫 문단</hp:t>꼬리</hp:run></hp:p>
        <hp:p><hp:run><hp:t>A</hp:t><hp:lineBreak/>B</hp:run></hp:p>
          <hp:tbl><hp:tr><hp:tc><hp:subList>
            <hp:p><hp:run><hp:t>표 셀 A</hp:t></hp:run></hp:p>
            <hp:p><hp:run><hp:t>표 셀 B</hp:t></hp:run></hp:p>
          </hp:subList></hp:tc></hp:tr></hp:tbl>
          <hp:p><hp:run><hp:t>마지막 문단</hp:t></hp:run></hp:p>
        </hs:sec>'''.encode("utf-8")
        path = self.make_hwpx({
            "Contents/section1.xml": '<sec><p><run><t>둘째 섹션</t></run></p></sec>'.encode("utf-8"),
            "Contents/section0.xml": xml,
            "Preview/PrvText.txt": "프리뷰 텍스트면 안 됨".encode(),
            "Contents/content.hpf": b"metadata only",
        })
        self.assertEqual(extract_hwpx(path, 1000), [
            ("첫 문단꼬리\nA\nB\n표 셀 A\n표 셀 B\n마지막 문단", "섹션 1"),
            ("둘째 섹션", "섹션 2"),
        ])

    def test_hwpx_limits_characters_and_ignores_text_outside_paragraphs(self) -> None:
        path = self.make_hwpx({"Contents/section0.xml": b'<sec><title>ignored</title><p><t>abcdef</t></p></sec>'})
        self.assertEqual(extract_hwpx(path, 3), [("abc", "섹션 1")])

    def test_hwpx_rejects_dtd_and_encryption(self) -> None:
        dtd = self.make_hwpx({"Contents/section0.xml": b'<!DOCTYPE x [<!ENTITY y "z">]><sec><p><t>&y;</t></p></sec>'})
        with self.assertRaisesRegex(ValueError, "DTD"):
            extract_hwpx(dtd)
        utf16_dtd = self.make_hwpx({
            "Contents/section0.xml": "<!DOCTYPE x><sec><p><t>blocked</t></p></sec>".encode("utf-16"),
        })
        with self.assertRaises(ValueError):
            extract_hwpx(utf16_dtd)
        encrypted = self.make_hwpx({
            "META-INF/manifest.xml": b'<manifest><encryption-data/></manifest>',
            "Contents/section0.xml": b'<sec><p><t>secret</t></p></sec>',
        })
        with self.assertRaisesRegex(ValueError, "암호화"):
            extract_hwpx(encrypted)

    @staticmethod
    def record(tag: int, payload: bytes) -> bytes:
        size = len(payload)
        if size < 0xFFF:
            return struct.pack("<I", tag | (size << 20)) + payload
        return struct.pack("<II", tag | (0xFFF << 20), size) + payload

    @staticmethod
    def file_header(flags: int) -> bytes:
        header = bytearray(256)
        header[:17] = b"HWP Document File"
        header[35] = 5
        struct.pack_into("<I", header, 36, flags)
        return bytes(header)

    @classmethod
    def make_cfb(cls, file_header: bytes, section: bytes) -> bytes:
        """Write a tiny v3 CFB container with ordinary (non-mini) streams."""
        # Pad both logical streams beyond the mini-stream cutoff. For the body,
        # zero padding becomes a sequence of valid zero-length record headers.
        free = 0xFFFFFFFF
        end = 0xFFFFFFFE
        fat_sector = 0xFFFFFFFD
        header_data = (file_header + b"\0" * 4096)[:4096]
        body_data = section + b"\0" * ((4 - len(section) % 4) % 4)
        if len(body_data) > 4096:
            raise ValueError("synthetic section exceeds fixture stream size")
        body_data += b"\0" * (4096 - len(body_data))
        header_sectors = 8
        body_sectors = 8
        header_first = 2
        body_first = header_first + header_sectors
        fat = [free] * 128
        fat[0] = fat_sector
        fat[1] = end
        for first, count in ((header_first, header_sectors), (body_first, body_sectors)):
            for sector_id in range(first, first + count):
                fat[sector_id] = sector_id + 1 if sector_id < first + count - 1 else end

        def sectors(data: bytes, count: int) -> bytes:
            padded = data + b"\0" * (count * 512 - len(data))
            return padded

        def directory_entry(name: str, kind: int, *, left: int = free, right: int = free,
                            child: int = free, start: int = end, size: int = 0) -> bytes:
            raw_name = (name + "\0").encode("utf-16le")
            entry = bytearray(128)
            entry[:len(raw_name)] = raw_name
            struct.pack_into("<H", entry, 64, len(raw_name))
            entry[66] = kind
            entry[67] = 1  # black node
            struct.pack_into("<III", entry, 68, left, right, child)
            struct.pack_into("<I", entry, 116, start)
            struct.pack_into("<Q", entry, 120, size)
            return bytes(entry)

        # The directory sibling tree has BodyText as root and FileHeader on
        # its right; BodyText contains Section0.
        directory = b"".join([
            directory_entry("Root Entry", 5, child=2),
            directory_entry("FileHeader", 2, start=header_first, size=len(header_data)),
            directory_entry("BodyText", 1, right=1, child=3),
            directory_entry("Section0", 2, start=body_first, size=len(body_data)),
        ])
        directory += b"\0" * (512 - len(directory))

        header = bytearray(512)
        header[:8] = bytes.fromhex("D0CF11E0A1B11AE1")
        struct.pack_into("<H", header, 24, 0x003E)  # minor version
        struct.pack_into("<H", header, 26, 3)       # major version 3
        struct.pack_into("<H", header, 28, 0xFFFE)  # little endian
        struct.pack_into("<H", header, 30, 9)       # 512-byte sectors
        struct.pack_into("<H", header, 32, 6)       # 64-byte mini sectors
        struct.pack_into("<I", header, 40, 0)       # directory sectors (v3)
        struct.pack_into("<I", header, 44, 1)       # FAT sectors
        struct.pack_into("<I", header, 48, 1)       # first directory sector
        struct.pack_into("<I", header, 56, 4096)    # mini-stream cutoff
        struct.pack_into("<II", header, 60, end, 0) # mini-FAT
        struct.pack_into("<II", header, 68, end, 0) # DIFAT
        struct.pack_into("<I", header, 76, 0)       # first FAT sector
        for index in range(1, 109):
            struct.pack_into("<I", header, 76 + 4 * index, free)

        return (bytes(header) + struct.pack("<128I", *fat) + directory
                + sectors(header_data, header_sectors) + sectors(body_data, body_sectors))

    def test_real_cfb_fixture_exercises_olefile_parser_and_nested_table_text(self) -> None:
        # These are real HWP5 record structures inside a generated CFB file:
        # an inline table control, table record, and cell paragraph records.
        before = self.record(0x43, ("앞 문단" + chr(0x0B) + "1234567").encode("utf-16le"))
        table = self.record(0x3D, bytes(4))
        cell = self.record(0x43, ("표 안" + chr(0x0D)).encode("utf-16le"))
        after = self.record(0x43, "뒤 문단".encode("utf-16le"))
        section = self.record(0x42, bytes(22)) + before + table + cell + after
        if len(section) % 4:
            section += self.record(0, bytes(2))
        path = self.root / "generated.hwpx.hwp"
        path.write_bytes(self.make_cfb(self.file_header(0), section))

        self.assertEqual(extract_hwp(path), [("앞 문단\n표 안\n뒤 문단", "섹션 1")])

    def test_hwp_parser_handles_raw_deflate_controls_and_locations(self) -> None:
        chars = "첫" + chr(0x0B) + "1234567" + "표" + chr(0x0D) + "둘째"
        section = self.record(0x43, chars.encode("utf-16le"))
        compressor = zlib.compressobj(wbits=-15)
        compressed = compressor.compress(section) + compressor.flush()
        path = self.root / "mock.hwp"
        path.write_bytes(b"fake compound file")
        streams = {
            ("FileHeader",): self.file_header(1),
            ("BodyText", "Section0"): compressed,
        }
        ole = _FakeOle(streams)
        fake_olefile = types.SimpleNamespace(OleFileIO=lambda *args, **kwargs: ole)
        with patch.dict(sys.modules, {"olefile": fake_olefile}):
            self.assertEqual(extract_hwp(path, 1000), [("첫표\n둘째", "섹션 1")])

    def test_hwp_rejects_encrypted_distribution_and_drm_flags(self) -> None:
        for flags, message in (
            (1 << 1, "암호화된"), (1 << 2, "배포용"), (1 << 4, "DRM"),
            (1 << 8, "인증서 암호화"), (1 << 10, "인증서 DRM"),
        ):
            with self.subTest(flags=flags):
                path = self.root / "mock.hwp"
                path.write_bytes(b"fake compound file")
                ole = _FakeOle({("FileHeader",): self.file_header(flags)})
                with patch.dict(sys.modules, {"olefile": types.SimpleNamespace(OleFileIO=lambda *a, **kw: ole)}):
                    with self.assertRaisesRegex(ValueError, message):
                        extract_hwp(path)

    def test_hwp_rejects_non_v5_file_header(self) -> None:
        path = self.root / "mock.hwp"
        path.write_bytes(b"fake compound file")
        header = bytearray(self.file_header(0))
        header[35] = 3
        ole = _FakeOle({("FileHeader",): bytes(header)})
        with patch.dict(sys.modules, {"olefile": types.SimpleNamespace(OleFileIO=lambda *a, **kw: ole)}):
            with self.assertRaisesRegex(ValueError, "HWP 5"):
                extract_hwp(path)

    def test_hwp_rejects_malformed_record_without_guessing_text(self) -> None:
        path = self.root / "mock.hwp"
        path.write_bytes(b"fake compound file")
        streams = {
            ("FileHeader",): self.file_header(0),
            ("BodyText", "Section0"): struct.pack("<I", 0x43 | (100 << 20)) + b"x",
        }
        ole = _FakeOle(streams)
        with patch.dict(sys.modules, {"olefile": types.SimpleNamespace(OleFileIO=lambda *a, **kw: ole)}):
            with self.assertRaisesRegex(ValueError, "범위를 벗어났습니다"):
                extract_hwp(path)

    def test_hwp_preserves_tab_and_surrogate_pair_and_rejects_orphan_surrogate(self) -> None:
        path = self.root / "mock.hwp"
        path.write_bytes(b"fake compound file")
        text = "A" + chr(9) + "1234567" + "😀"
        valid = self.record(0x43, text.encode("utf-16le"))
        for payload, expected in ((valid[4:], "A\t😀"), (b"\x00\xd8", None)):
            streams = {
                ("FileHeader",): self.file_header(0),
                ("BodyText", "Section0"): self.record(0x43, payload),
            }
            ole = _FakeOle(streams)
            with patch.dict(sys.modules, {"olefile": types.SimpleNamespace(OleFileIO=lambda *a, **kw: ole)}):
                if expected is None:
                    with self.assertRaisesRegex(ValueError, "서로게이트"):
                        extract_hwp(path)
                else:
                    self.assertEqual(extract_hwp(path), [(expected, "섹션 1")])


class _FakeStream(io.BytesIO):
    @property
    def size(self) -> int:
        return len(self.getvalue())


class _FakeOle:
    def __init__(self, streams: dict[tuple[str, ...], bytes]):
        self.streams = streams

    def exists(self, name: str) -> bool:
        return (name,) in self.streams

    def openstream(self, name):
        parts = tuple(name.split("/")) if isinstance(name, str) else tuple(name)
        return _FakeStream(self.streams[parts])

    def listdir(self, streams=True, storages=False):
        return [list(name) for name in self.streams if len(name) > 1]

    def close(self) -> None:
        pass


if __name__ == "__main__":
    unittest.main()
