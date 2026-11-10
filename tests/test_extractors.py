from __future__ import annotations

from pathlib import Path
import tempfile
import unittest
import zipfile

from moasearch.extractors import Passage, chunk_passages, extract, supports_content


class ExtractorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="moasearch-test-", dir=Path.cwd())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_text_decodes_utf8_bom_and_cp949(self) -> None:
        utf8 = self.root / "memo.txt"
        utf8.write_bytes(bytes.fromhex("ef bb bf") + "hello 가나다".encode("utf-8"))
        self.assertEqual(extract(utf8)[0].text, "hello 가나다")
        korean = self.root / "memo.log"
        korean.write_bytes("한글 메모".encode("cp949"))
        self.assertEqual(extract(korean)[0].text, "한글 메모")

    def test_html_excludes_script_and_style(self) -> None:
        page = self.root / "page.html"
        page.write_text("<p>visible</p><script>secret()</script><style>hidden</style>", encoding="utf-8")
        text = extract(page)[0].text
        self.assertIn("visible", text)
        self.assertNotIn("secret", text)
        self.assertNotIn("hidden", text)

    def test_unknown_binary_and_unsupported_are_not_read_as_text(self) -> None:
        binary = self.root / "blob.bin"
        binary.write_bytes(b"\x00\xff\x01\xfe")
        self.assertFalse(supports_content(binary))
        self.assertEqual(extract(binary), [])
        self.assertEqual(extract(self.root / "absent.exe"), [])

    def test_text_limit_and_invalid_limit(self) -> None:
        source = self.root / "large.txt"
        source.write_text("가" * 100, encoding="utf-8")
        self.assertEqual(len(extract(source, max_chars=10)[0].text), 10)
        with self.assertRaises(ValueError):
            extract(source, max_chars=0)

    def test_docx_preserves_body_location_and_rejects_bad_xml(self) -> None:
        docx = self.root / "note.docx"
        with zipfile.ZipFile(docx, "w") as zf:
            zf.writestr("word/document.xml", "<w:document xmlns:w='w'><w:p><w:t>회의 결과</w:t></w:p></w:document>")
        self.assertEqual(extract(docx), [Passage("회의 결과", "본문")])
        broken = self.root / "broken.docx"
        with zipfile.ZipFile(broken, "w") as zf:
            zf.writestr("word/document.xml", "<document>")
        with self.assertRaises(Exception):
            extract(broken)

    def test_xlsx_reads_shared_and_inline_cell_text(self) -> None:
        xlsx = self.root / "table.xlsx"
        with zipfile.ZipFile(xlsx, "w") as zf:
            zf.writestr("xl/sharedStrings.xml", "<sst><si><t>매출 계획</t></si></sst>")
            zf.writestr("xl/worksheets/sheet1.xml", (
                "<worksheet><sheetData><row><c t='s'><v>0</v></c>"
                "<c t='inlineStr'><is><t>서울 지점</t></is></c></row></sheetData></worksheet>"
            ))
        result = extract(xlsx)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].location, "시트 1")
        self.assertIn("매출 계획", result[0].text)
        self.assertIn("서울 지점", result[0].text)

    def test_archive_uncompressed_size_limit(self) -> None:
        import moasearch.extractors as module
        archive = self.root / "huge.docx"
        with zipfile.ZipFile(archive, "w") as zf:
            zf.writestr("word/document.xml", "<root>123456789</root>")
        old = module._MAX_ENTRY_BYTES
        try:
            module._MAX_ENTRY_BYTES = 8
            with self.assertRaises(ValueError):
                extract(archive)
        finally:
            module._MAX_ENTRY_BYTES = old

    def test_chunks_cover_all_text_with_overlap_and_keep_location(self) -> None:
        source = Passage("a" * 10 + " " + "b" * 10 + " " + "c" * 10, "4페이지")
        chunks = chunk_passages([source], chunk_size=12, overlap=3)
        self.assertGreater(len(chunks), 2)
        self.assertTrue(all(len(chunk.text) <= 12 for chunk in chunks))
        self.assertTrue(all(chunk.location == "4페이지" for chunk in chunks))
        for char in "abc":
            self.assertIn(char, "".join(chunk.text for chunk in chunks))
        with self.assertRaises(ValueError):
            chunk_passages([source], chunk_size=5, overlap=5)


if __name__ == "__main__":
    unittest.main()
