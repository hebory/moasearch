//! Bounded local extraction of text from searchable files.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{error::Error as StdError, fmt};

use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

const MAX_SOURCE: u64 = 100 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 30_000;
const MAX_ZIP_TOTAL: u64 = 128 * 1024 * 1024;
const MAX_ZIP_ENTRY: u64 = 16 * 1024 * 1024;
const MAX_HWP_BODY: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passage {
    pub text: String,
    pub location: String,
}

/// A reader failure with text from pages that were read successfully.
#[derive(Debug)]
pub struct ExtractionError {
    pub passages: Vec<Passage>,
    pub message: String,
}

impl fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl StdError for ExtractionError {}

impl Passage {
    pub(crate) fn new(text: String, location: impl Into<String>) -> Option<Self> {
        let text = text.trim().to_owned();
        (!text.is_empty()).then(|| Self {
            text,
            location: location.into(),
        })
    }
}

const TEXT_SUFFIXES: &[&str] = &[
    "txt",
    "text",
    "md",
    "markdown",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "xml",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "html",
    "htm",
    "css",
    "js",
    "jsx",
    "ts",
    "tsx",
    "py",
    "pyi",
    "ipynb",
    "java",
    "kt",
    "kts",
    "scala",
    "go",
    "rs",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "fs",
    "vb",
    "php",
    "rb",
    "swift",
    "m",
    "mm",
    "sh",
    "bash",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "graphql",
    "proto",
    "dockerfile",
    "make",
    "gradle",
    "properties",
    "tex",
    "bib",
    "srt",
    "vtt",
];
const ARCHIVE_SUFFIXES: &[&str] = &["docx", "pptx", "xlsx", "odt", "odp", "ods", "hwpx"];
const IMAGE_SUFFIXES: &[&str] = &["png", "jpg", "jpeg", "bmp", "tif", "tiff", "webp", "gif"];

fn suffix(path: &Path) -> String {
    path.extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn supports_content(path: &Path) -> bool {
    let ext = suffix(path);
    TEXT_SUFFIXES.contains(&ext.as_str())
        || ARCHIVE_SUFFIXES.contains(&ext.as_str())
        || IMAGE_SUFFIXES.contains(&ext.as_str())
        || matches!(ext.as_str(), "pdf" | "hwp")
}

/// Extract up to `max_chars`; cancellation and page progress are shared with indexing.
pub fn extract(
    path: &Path,
    max_chars: usize,
    cancelled: &AtomicBool,
    progress: &dyn Fn(u32, u32, &str),
) -> Result<Vec<Passage>> {
    let model_dir = crate::config::data_directory().join("models");
    extract_with_models(path, max_chars, cancelled, progress, &model_dir)
}

/// Extract using explicitly configured local model files.
pub fn extract_with_models(
    path: &Path,
    max_chars: usize,
    cancelled: &AtomicBool,
    progress: &dyn Fn(u32, u32, &str),
    model_dir: &Path,
) -> Result<Vec<Passage>> {
    if max_chars == 0 {
        bail!("max_chars must be positive");
    }
    if cancelled.load(Ordering::Relaxed) {
        bail!("색인을 취소했습니다.");
    }
    let ext = suffix(path);
    match ext.as_str() {
        "pdf" => {
            return crate::pdf::extract_pdf(path, max_chars, cancelled, progress, model_dir);
        }
        "hwp" => return extract_hwp(path, max_chars),
        "hwpx" => return extract_hwpx(path, max_chars),
        image if IMAGE_SUFFIXES.contains(&image) => {
            return crate::ocr::extract_image(path, max_chars, cancelled, model_dir);
        }
        archive if ARCHIVE_SUFFIXES.contains(&archive) => {
            return extract_archive(path, &ext, max_chars);
        }
        text if TEXT_SUFFIXES.contains(&text) => return extract_plain(path, max_chars),
        _ => Ok(Vec::new()),
    }
}

fn extract_plain(path: &Path, max_chars: usize) -> Result<Vec<Passage>> {
    let file =
        File::open(path).with_context(|| format!("파일을 열 수 없습니다: {}", path.display()))?;
    let cap = (max_chars.saturating_mul(4).saturating_add(4)) as u64;
    let mut bytes = Vec::with_capacity(cap.min(1024 * 1024) as usize);
    file.take(cap).read_to_end(&mut bytes)?;
    let text = decode_text(&bytes);
    let text = if matches!(suffix(path).as_str(), "html" | "htm") {
        visible_html(&text)
    } else {
        text
    };
    Ok(Passage::new(text.chars().take(max_chars).collect(), "본문")
        .into_iter()
        .collect())
}

fn decode_text(bytes: &[u8]) -> String {
    let mut text = if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let words = rest
            .chunks_exact(2)
            .map(|x| u16::from_le_bytes([x[0], x[1]]));
        let mut decoded: String = char::decode_utf16(words)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        if rest.len() % 2 != 0 {
            decoded.push(char::REPLACEMENT_CHARACTER);
        }
        decoded
    } else if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let words = rest
            .chunks_exact(2)
            .map(|x| u16::from_be_bytes([x[0], x[1]]));
        let mut decoded: String = char::decode_utf16(words)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        if rest.len() % 2 != 0 {
            decoded.push(char::REPLACEMENT_CHARACTER);
        }
        decoded
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        if let Ok(text) = std::str::from_utf8(bytes) {
            text.to_owned()
        } else {
            let (text, _, _) = encoding_rs::EUC_KR.decode(bytes);
            text.into_owned()
        }
    };
    let total = text.chars().count();
    let controls = text
        .chars()
        .filter(|c| c.is_control() && !"\n\r\t\x0c".contains(*c))
        .count();
    if total > 0 && controls.saturating_mul(50) > total {
        text.clear();
    }
    text
}

fn visible_html(html: &str) -> String {
    let mut out = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    let mut quote = None;
    let mut hidden: Vec<String> = Vec::new();
    let mut entity = String::new();
    let mut in_entity = false;
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if in_tag {
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                continue;
            }
            if c == '>' {
                let raw = tag.trim();
                let closing = raw.starts_with('/');
                let name = raw
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ["script", "style", "noscript", "svg"].contains(&name.as_str()) {
                    if closing {
                        if !hidden.is_empty() {
                            hidden.pop();
                        }
                    } else if !raw.ends_with('/') {
                        hidden.push(name);
                    }
                } else if !closing
                    && ["br", "p", "div", "li", "tr", "h1", "h2", "h3"].contains(&name.as_str())
                    && hidden.is_empty()
                {
                    out.push('\n');
                } else if closing
                    && ["p", "div", "li", "tr", "h1", "h2", "h3"].contains(&name.as_str())
                    && hidden.is_empty()
                {
                    out.push('\n');
                }
                tag.clear();
                in_tag = false;
                continue;
            }
            tag.push(c);
            continue;
        }
        if in_entity {
            if c == ';' {
                entity.push(';');
                out.push_str(
                    &quick_xml::escape::unescape(&entity).unwrap_or_else(|_| entity.clone().into()),
                );
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 && (c.is_ascii_alphanumeric() || c == '#') {
                entity.push(c);
            } else {
                out.push('&');
                out.push_str(&entity);
                out.push(c);
                entity.clear();
                in_entity = false;
            }
            continue;
        }
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '&' => {
                in_entity = true;
                entity.clear();
                entity.push('&');
            }
            _ if hidden.is_empty() => out.push(c),
            _ => {}
        }
    }
    let mut normalized = String::new();
    let mut spaces = false;
    for c in out.chars() {
        if matches!(c, ' ' | '\t' | '\r' | '\x0c' | '\x0b') {
            spaces = true;
        } else {
            if spaces && !normalized.ends_with('\n') {
                normalized.push(' ');
            }
            spaces = false;
            normalized.push(c);
        }
    }
    normalized.trim().to_owned()
}

fn archive_members<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<Vec<(String, u64)>> {
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!("문서 압축 항목 수가 허용 한도를 넘었습니다.");
    }
    let mut members = Vec::with_capacity(archive.len());
    let mut total = 0u64;
    let mut names = std::collections::HashSet::new();
    for i in 0..archive.len() {
        let item = archive.by_index(i)?;
        let name = item.name().to_owned();
        let size = item.size();
        if size > MAX_ZIP_ENTRY {
            bail!("문서 내부 파일이 허용 크기를 넘었습니다.");
        }
        total = total.saturating_add(size);
        if total > MAX_ZIP_TOTAL {
            bail!("문서 압축 해제 크기가 허용 한도를 넘었습니다.");
        }
        if !names.insert(name.clone()) {
            bail!("문서에 중복된 압축 항목 이름이 있습니다.");
        }
        members.push((name, size));
    }
    Ok(members)
}

fn read_member<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    expected: u64,
) -> Result<Vec<u8>> {
    if expected > MAX_ZIP_ENTRY {
        bail!("문서 내부 파일이 허용 크기를 넘었습니다.");
    }
    let member = archive
        .by_name(name)
        .with_context(|| format!("압축 문서 항목이 없습니다: {name}"))?;
    let mut bytes = Vec::with_capacity(expected.min(MAX_ZIP_ENTRY) as usize);
    member.take(MAX_ZIP_ENTRY + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected {
        bail!("문서 내부 파일 크기가 헤더와 일치하지 않습니다.");
    }
    Ok(bytes)
}

fn xml_text(raw: &[u8], limit: usize) -> Result<String> {
    let mut reader = Reader::from_reader(raw);
    reader.config_mut().trim_text(false);
    let mut parts = Vec::new();
    let mut count = 0usize;
    loop {
        match reader.read_event()? {
            Event::Text(text) => {
                let decoded = text.xml10_content();
                let value = quick_xml::escape::unescape(&decoded)?;
                if !value.trim().is_empty() {
                    count += value.len();
                    parts.push(value.into_owned());
                }
            }
            Event::CData(text) => {
                let value = text.xml10_content();
                if !value.trim().is_empty() {
                    count += value.len();
                    parts.push(value.into_owned());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        if count > limit.saturating_mul(4) {
            break;
        }
    }
    Ok(parts.join(" ").chars().take(limit).collect())
}

fn extract_archive(path: &Path, ext: &str, max_chars: usize) -> Result<Vec<Passage>> {
    if path.metadata()?.len() > MAX_SOURCE {
        bail!("문서가 허용 크기(100 MiB)를 넘었습니다.");
    }
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file).context("문서 압축 파일을 읽을 수 없습니다")?;
    let members = archive_members(&mut archive)?;
    let sizes: HashMap<_, _> = members.iter().cloned().collect();
    let mut targets: Vec<(String, String)> = Vec::new();
    if ext == "docx" {
        targets.push(("word/document.xml".into(), "본문".into()));
    } else if ext == "pptx" {
        let mut slides: Vec<_> = members
            .iter()
            .filter_map(|(name, _)| {
                let stem = name
                    .strip_prefix("ppt/slides/slide")?
                    .strip_suffix(".xml")?;
                let number: u32 = stem.parse().ok()?;
                Some((number, name.clone()))
            })
            .collect();
        slides.sort_by_key(|v| v.0);
        targets.extend(
            slides
                .into_iter()
                .map(|(n, name)| (name, format!("슬라이드 {n}"))),
        );
    } else if ext == "xlsx" {
        let shared = if let Some(size) = sizes.get("xl/sharedStrings.xml") {
            let raw = read_member(&mut archive, "xl/sharedStrings.xml", *size)?;
            parse_shared_strings(&raw, max_chars)?
        } else {
            Vec::new()
        };
        let mut sheets: Vec<_> = members
            .iter()
            .filter_map(|(name, _)| {
                let stem = name
                    .strip_prefix("xl/worksheets/sheet")?
                    .strip_suffix(".xml")?;
                let number: u32 = stem.parse().ok()?;
                Some((number, name.clone()))
            })
            .collect();
        sheets.sort_by_key(|v| v.0);
        let mut out = Vec::new();
        let mut consumed = 0usize;
        for (number, name) in sheets {
            let raw = read_member(&mut archive, &name, *sizes.get(&name).unwrap_or(&0))?;
            let cells = parse_sheet(&raw, &shared, max_chars.saturating_sub(consumed))?;
            let text = cells
                .join(" ")
                .chars()
                .take(max_chars.saturating_sub(consumed))
                .collect::<String>();
            consumed += text.len();
            if let Some(passage) = Passage::new(text, format!("시트 {number}")) {
                out.push(passage);
            }
            if consumed >= max_chars {
                break;
            }
        }
        return Ok(out);
    } else if ext == "hwpx" {
        if let Some(size) = sizes.get("META-INF/manifest.xml") {
            let raw = read_member(&mut archive, "META-INF/manifest.xml", *size)?;
            let lower = String::from_utf8_lossy(&raw).to_ascii_lowercase();
            if lower.contains("encryption-data") || lower.contains("encrypted") {
                bail!("암호화된 HWPX 문서는 검색할 수 없습니다.");
            }
        }
        let mut sections: Vec<_> = members
            .iter()
            .filter_map(|(name, _)| {
                let stem = name
                    .strip_prefix("Contents/section")?
                    .strip_suffix(".xml")?;
                let number: u32 = stem.parse().ok()?;
                Some((number, name.clone()))
            })
            .collect();
        sections.sort_by_key(|v| v.0);
        let mut out = Vec::new();
        let mut remaining = max_chars;
        for (number, name) in sections {
            let raw = read_member(&mut archive, &name, *sizes.get(&name).unwrap_or(&0))?;
            let text = hwpx_section_text(&raw, remaining)?;
            if let Some(passage) = Passage::new(text, format!("섹션 {}", number + 1)) {
                remaining -= passage.text.len();
                out.push(passage);
            }
            if remaining == 0 {
                break;
            }
        }
        if out.is_empty() {
            bail!("HWPX 본문 Contents/sectionN.xml이 없습니다.");
        }
        return Ok(out);
    } else {
        targets.push(("content.xml".into(), "본문".into()));
    }
    let mut out = Vec::new();
    let mut remaining = max_chars;
    for (name, label) in targets {
        let Some(size) = sizes.get(&name) else {
            continue;
        };
        let raw = read_member(&mut archive, &name, *size)?;
        let text = xml_text(&raw, remaining)?;
        if let Some(passage) = Passage::new(text, label) {
            remaining -= passage.text.len();
            out.push(passage);
        }
        if remaining == 0 {
            break;
        }
    }
    Ok(out)
}

fn parse_shared_strings(raw: &[u8], limit: usize) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(raw);
    let mut values = Vec::new();
    let mut total = 0usize;
    let mut in_item = false;
    let mut in_text = false;
    let mut current = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "si" => {
                in_item = true;
                current.clear();
            }
            Event::End(e) if e.local_name().as_ref() == "si" => {
                if in_item {
                    total += current.len();
                    values.push(current.clone());
                }
                in_item = false;
            }
            Event::Start(e) if in_item && e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) if e.local_name().as_ref() == "t" => in_text = false,
            Event::Text(e) if in_text => {
                let decoded = e.xml10_content();
                current.push_str(&quick_xml::escape::unescape(&decoded)?);
            }
            Event::Eof => break,
            _ => {}
        }
        if total > limit.saturating_mul(2) {
            break;
        }
    }
    Ok(values)
}

fn parse_sheet(raw: &[u8], shared: &[String], limit: usize) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(raw);
    let mut cells = Vec::new();
    let mut total = 0usize;
    let mut in_cell = false;
    let mut cell_type = String::new();
    let mut in_value = false;
    let mut value = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "c" => {
                in_cell = true;
                value.clear();
                cell_type.clear();
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == "t" {
                        cell_type = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                            .into_owned();
                    }
                }
            }
            Event::End(e) if e.local_name().as_ref() == "c" => {
                if in_cell && !value.is_empty() {
                    if cell_type == "s" {
                        if let Ok(index) = value.parse::<usize>() {
                            if let Some(v) = shared.get(index) {
                                total += v.len();
                                cells.push(v.clone());
                            }
                        }
                    } else {
                        total += value.len();
                        cells.push(value.clone());
                    }
                }
                in_cell = false;
                in_value = false;
            }
            Event::Start(e) if in_cell && ["v", "t"].contains(&e.local_name().as_ref()) => {
                in_value = true
            }
            Event::End(e) if ["v", "t"].contains(&e.local_name().as_ref()) => in_value = false,
            Event::Text(e) if in_cell && in_value => {
                let decoded = e.xml10_content();
                value.push_str(&quick_xml::escape::unescape(&decoded)?);
            }
            Event::Eof => break,
            _ => {}
        }
        if total > limit.saturating_mul(2) {
            break;
        }
    }
    Ok(cells)
}

fn hwpx_section_text(raw: &[u8], limit: usize) -> Result<String> {
    let mut reader = Reader::from_reader(raw);
    let mut out = String::new();
    let mut paragraph = 0usize;
    let mut in_text = 0u32;
    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.local_name().as_ref() {
                "p" => paragraph += 1,
                "t" if paragraph > 0 => in_text += 1,
                "lineBreak" if paragraph > 0 => out.push('\n'),
                "tab" if paragraph > 0 => out.push('\t'),
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                "p" if paragraph > 0 => {
                    paragraph -= 1;
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "t" if in_text > 0 => in_text -= 1,
                _ => {}
            },
            Event::Text(e) if paragraph > 0 => {
                let decoded = e.xml10_content();
                if in_text > 0 {
                    out.push_str(&quick_xml::escape::unescape(&decoded)?);
                }
            }
            Event::CData(e) if paragraph > 0 && in_text > 0 => out.push_str(&e.xml10_content()),
            Event::Eof => break,
            _ => {}
        }
        if out.chars().count() > limit.saturating_mul(2) {
            break;
        }
    }
    Ok(out
        .chars()
        .take(limit)
        .collect::<String>()
        .trim()
        .to_owned())
}

fn extract_hwpx(path: &Path, max_chars: usize) -> Result<Vec<Passage>> {
    extract_archive(path, "hwpx", max_chars)
}

fn extract_hwp(path: &Path, max_chars: usize) -> Result<Vec<Passage>> {
    if path.metadata()?.len() > MAX_SOURCE {
        bail!("한글 문서가 허용 크기(100 MiB)를 넘었습니다.");
    }
    let mut cfb = cfb::open(path).context("HWP OLE 컨테이너를 읽을 수 없습니다")?;
    if !cfb.is_stream("/FileHeader") {
        bail!("HWP 5 FileHeader가 없습니다. HWP 3 형식은 지원하지 않습니다.");
    }
    let mut header = Vec::new();
    cfb.open_stream("/FileHeader")?
        .take(257)
        .read_to_end(&mut header)?;
    if header.len() < 40 || !header.starts_with(b"HWP Document File") {
        bail!("유효한 HWP 5 FileHeader가 아닙니다.");
    }
    if header.len() < 256 {
        bail!("HWP FileHeader가 잘렸습니다.");
    }
    if header[35] != 5 {
        bail!("지원하지 않는 HWP 문서 버전입니다. HWP 5 형식만 지원합니다.");
    }
    let flags = u32::from_le_bytes(header[36..40].try_into().unwrap());
    for (bit, reason) in [
        (1, "암호화된"),
        (2, "배포용(편집 제한)"),
        (4, "DRM 보호"),
        (8, "인증서 암호화"),
        (10, "인증서 DRM 보호"),
    ] {
        if flags & (1 << bit) != 0 {
            bail!("{reason} HWP 문서는 검색할 수 없습니다.");
        }
    }
    let mut sections: Vec<(u32, String)> = cfb
        .walk()
        .filter_map(|entry| {
            let name = entry.path().to_string_lossy().replace('\\', "/");
            let suffix = name.strip_prefix("/BodyText/Section")?;
            Some((suffix.parse().ok()?, name))
        })
        .collect();
    sections.sort_by_key(|x| x.0);
    if sections.is_empty() {
        bail!("HWP 본문 Section 스트림이 없습니다.");
    }
    let compressed = flags & 1 != 0;
    let mut out = Vec::new();
    let mut remaining = max_chars;
    let mut total = 0usize;
    for (index, name) in sections {
        if cfb.entry(&name)?.len() > MAX_HWP_BODY as u64 {
            bail!("HWP 본문 스트림이 허용 크기를 넘었습니다.");
        }
        let mut raw = Vec::new();
        cfb.open_stream(&name)?
            .take((MAX_HWP_BODY + 1) as u64)
            .read_to_end(&mut raw)?;
        let body = if compressed {
            inflate_raw(&raw, MAX_HWP_BODY.saturating_sub(total))?
        } else {
            raw
        };
        total = total.saturating_add(body.len());
        if total > MAX_HWP_BODY {
            bail!("HWP 본문 압축 해제 크기가 허용 한도를 넘었습니다.");
        }
        let text = hwp_records_text(&body, remaining)?;
        if let Some(passage) = Passage::new(text, format!("섹션 {}", index + 1)) {
            remaining = remaining.saturating_sub(passage.text.chars().count());
            out.push(passage);
        }
        if remaining == 0 {
            break;
        }
    }
    Ok(out)
}

fn inflate_raw(raw: &[u8], limit: usize) -> Result<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress};
    let mut decoder = Decompress::new(false);
    let mut output = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    let mut input_pos = 0usize;
    loop {
        let before_in = decoder.total_in() as usize;
        let before_out = decoder.total_out() as usize;
        let status = decoder
            .decompress(&raw[input_pos..], &mut chunk, FlushDecompress::None)
            .context("HWP 본문 압축 데이터가 손상되었습니다")?;
        input_pos = decoder.total_in() as usize;
        let produced = decoder.total_out() as usize - before_out;
        if produced > limit.saturating_sub(output.len()) {
            bail!("HWP 본문 압축 해제 크기가 허용 한도를 넘었습니다.");
        }
        output.extend_from_slice(&chunk[..produced]);
        if status == flate2::Status::StreamEnd {
            break;
        }
        if decoder.total_in() as usize == before_in && produced == 0 {
            bail!("HWP 본문 압축 데이터가 잘렸습니다.");
        }
        if input_pos >= raw.len() {
            bail!("HWP 본문 압축 데이터가 잘렸습니다.");
        }
    }
    if input_pos != raw.len() {
        bail!("HWP 본문 압축 데이터 뒤에 불필요한 데이터가 있습니다.");
    }
    Ok(output)
}

fn hwp_records_text(data: &[u8], max_chars: usize) -> Result<String> {
    const EXTENDED: &[u16] = &[1, 2, 3, 6, 7, 11, 12, 14, 15, 16, 17, 18, 21, 22, 23];
    const INLINE: &[u16] = &[4, 5, 8, 19, 20];
    let mut pos = 0usize;
    let mut paragraphs = Vec::new();
    let mut total = 0usize;
    while pos < data.len() {
        if data.len() - pos < 4 {
            bail!("HWP 레코드 헤더가 잘렸습니다.");
        }
        let header = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let tag = (header & 0x3ff) as u16;
        let mut size = ((header >> 20) & 0xfff) as usize;
        if size == 0xfff {
            if data.len() - pos < 4 {
                bail!("HWP 확장 레코드 길이가 잘렸습니다.");
            }
            size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
        }
        let end = pos
            .checked_add(size)
            .ok_or_else(|| anyhow!("HWP 레코드 길이가 너무 큽니다"))?;
        if end > data.len() {
            bail!("HWP 레코드 본문이 파일 범위를 벗어났습니다.");
        }
        if tag == 0x43 {
            let payload = &data[pos..end];
            if payload.len() % 2 != 0 {
                bail!("HWP 문단 UTF-16 데이터가 잘못되었습니다.");
            }
            let words: Vec<u16> = payload
                .chunks_exact(2)
                .map(|x| u16::from_le_bytes([x[0], x[1]]))
                .collect();
            let mut i = 0;
            let mut line = String::new();
            let mut line_chars = 0usize;
            while i < words.len() && line_chars < max_chars {
                let v = words[i];
                if v == 9 || EXTENDED.contains(&v) || INLINE.contains(&v) {
                    if i + 8 > words.len() {
                        bail!("HWP 문단 제어 문자가 잘렸습니다.");
                    }
                    if v == 9 {
                        line.push('\t');
                        line_chars += 1;
                    }
                    i += 8;
                    continue;
                }
                match v {
                    0x0a | 0x0d => {
                        line.push('\n');
                        line_chars += 1
                    }
                    0x1e | 0x1f => {
                        line.push(' ');
                        line_chars += 1
                    }
                    0x18 => {
                        line.push('-');
                        line_chars += 1
                    }
                    0xd800..=0xdbff => {
                        let Some(&low) = words.get(i + 1) else {
                            bail!("HWP UTF-16 서로게이트가 잘렸습니다.");
                        };
                        if !(0xdc00..=0xdfff).contains(&low) {
                            bail!("HWP UTF-16 서로게이트가 잘못되었습니다.");
                        }
                        let cp = 0x10000 + (((v as u32 - 0xd800) << 10) + (low as u32 - 0xdc00));
                        line.push(char::from_u32(cp).unwrap());
                        line_chars += 1;
                        i += 1;
                    }
                    0xdc00..=0xdfff => bail!("HWP UTF-16 서로게이트가 잘못되었습니다."),
                    0x20..=0xd7ff | 0xe000..=0xffff => {
                        line.push(char::from_u32(v as u32).unwrap());
                        line_chars += 1
                    }
                    _ => {}
                }
                i += 1;
            }
            if !line.is_empty() {
                total += line_chars;
                paragraphs.push(line);
            }
        }
        pos = end;
        if total >= max_chars {
            break;
        }
    }
    Ok(paragraphs
        .join("\n")
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned())
}

pub fn chunk_passages(
    passages: Vec<Passage>,
    chunk_size: usize,
    overlap: usize,
) -> Result<Vec<Passage>> {
    if chunk_size == 0 || overlap >= chunk_size {
        bail!("chunk_size must be positive and overlap smaller than it");
    }
    let mut out = Vec::new();
    for passage in passages {
        let chars: Vec<char> = passage.text.trim().chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let mut end = (start + chunk_size).min(chars.len());
            if end < chars.len() {
                let lower = start + chunk_size / 2;
                if let Some(boundary) = (lower..end)
                    .rev()
                    .find(|&i| chars[i] == '\n' || chars[i] == ' ')
                {
                    if boundary > start {
                        end = boundary;
                    }
                }
            }
            let text: String = chars[start..end].iter().collect();
            if let Some(piece) = Passage::new(text, passage.location.clone()) {
                out.push(piece);
            }
            if end >= chars.len() {
                break;
            }
            start = (end.saturating_sub(overlap)).max(start + 1);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tempfile::tempdir;

    fn no_progress(_: u32, _: u32, _: &str) {}

    #[test]
    fn reads_utf8_and_cp949_and_excludes_html_scripts() {
        let dir = tempdir().unwrap();
        let utf = dir.path().join("a.txt");
        std::fs::write(
            &utf,
            [b"\xef\xbb\xbf".as_slice(), "안녕".as_bytes()].concat(),
        )
        .unwrap();
        assert_eq!(
            extract(&utf, 200_000, &AtomicBool::new(false), &no_progress).unwrap()[0].text,
            "안녕"
        );
        let cp = dir.path().join("b.txt");
        std::fs::write(&cp, encoding_rs::EUC_KR.encode("한글 메모").0.as_ref()).unwrap();
        assert_eq!(
            extract(&cp, 200_000, &AtomicBool::new(false), &no_progress).unwrap()[0].text,
            "한글 메모"
        );
        let html = dir.path().join("c.html");
        std::fs::write(&html, "<p>visible</p><script>secret()</script>").unwrap();
        let text = &extract(&html, 200_000, &AtomicBool::new(false), &no_progress).unwrap()[0].text;
        assert!(text.contains("visible"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn chunks_cover_text_with_overlap_and_location() {
        let input = vec![Passage {
            text: "aaaaaaaaaa bbbbbbbbbb cccccccccc".into(),
            location: "4페이지".into(),
        }];
        let chunks = chunk_passages(input, 12, 3).unwrap();
        assert!(chunks.len() > 2);
        assert!(
            chunks
                .iter()
                .all(|c| c.text.chars().count() <= 12 && c.location == "4페이지")
        );
        let joined = chunks.iter().map(|c| c.text.as_str()).collect::<String>();
        for ch in ['a', 'b', 'c'] {
            assert!(joined.contains(ch));
        }
    }

    #[test]
    fn reads_ooxml_docx_text_and_keeps_location() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.docx");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        use std::io::Write;
        zip.write_all(
            "<w:document xmlns:w=\"w\"><w:p><w:t>회의 결과</w:t></w:p></w:document>".as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
        let extracted = extract(&path, 200_000, &AtomicBool::new(false), &no_progress).unwrap();
        assert_eq!(extracted[0].text, "회의 결과");
        assert_eq!(extracted[0].location, "본문");
    }

    fn hwp_record(tag: u16, body: &[u8]) -> Vec<u8> {
        let mut out = ((tag as u32) | ((body.len() as u32) << 20))
            .to_le_bytes()
            .to_vec();
        out.extend_from_slice(body);
        out
    }

    fn make_hwp(path: &Path, flags: u32, body: &[u8]) {
        use std::io::Write;
        let mut header = vec![0u8; 256];
        header[..17].copy_from_slice(b"HWP Document File");
        header[35] = 5;
        header[36..40].copy_from_slice(&flags.to_le_bytes());
        let mut compound = cfb::create(path).unwrap();
        compound.create_storage("/BodyText").unwrap();
        compound
            .create_stream("/FileHeader")
            .unwrap()
            .write_all(&header)
            .unwrap();
        compound
            .create_stream("/BodyText/Section0")
            .unwrap()
            .write_all(body)
            .unwrap();
    }

    #[test]
    fn reads_hwp5_records_and_rejects_protected_documents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.hwp");
        let body = hwp_record(
            0x43,
            "한글 본문"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
                .as_slice(),
        );
        make_hwp(&path, 0, &body);
        let passages = extract(&path, 200_000, &AtomicBool::new(false), &no_progress).unwrap();
        assert_eq!(
            passages,
            vec![Passage {
                text: "한글 본문".into(),
                location: "섹션 1".into()
            }]
        );
        make_hwp(&path, 1 << 1, &[]);
        assert!(
            extract(&path, 200_000, &AtomicBool::new(false), &no_progress)
                .unwrap_err()
                .to_string()
                .contains("암호화")
        );
    }

    #[test]
    fn reads_hwpx_section_xml_in_document_order() {
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir.path().join("sample.hwpx");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "Contents/section0.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all("<root xmlns:hp=\"http://hancom.co.kr/hwpml/2011/paragraph\"><hp:p><hp:run><hp:t>한글 HWPX</hp:t></hp:run></hp:p></root>".as_bytes()).unwrap();
        zip.finish().unwrap();
        let passages = extract(&path, 200_000, &AtomicBool::new(false), &no_progress).unwrap();
        assert_eq!(
            passages,
            vec![Passage {
                text: "한글 HWPX".into(),
                location: "섹션 1".into()
            }]
        );
    }

    #[test]
    fn cancellation_is_checked_before_reading() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("stop.txt");
        std::fs::write(&path, "text").unwrap();
        assert!(extract(&path, 200_000, &AtomicBool::new(true), &no_progress).is_err());
    }
}
