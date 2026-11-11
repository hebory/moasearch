//! Bounded PDF text extraction, with Windows' built-in renderer for scanned pages.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use lopdf::{Object, ObjectId};

use crate::extract::{ExtractionError, Passage};

const MAX_PDF_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PAGES: usize = 250;
const OCR_TEXT_THRESHOLD: usize = 40;
const MAX_RENDER_SIDE: f32 = 3200.0;
const MAX_RENDER_PIXELS: f32 = 8_000_000.0;

pub fn extract_pdf(
    path: &Path,
    max_chars: usize,
    cancelled: &AtomicBool,
    progress: &dyn Fn(u32, u32, &str),
    model_dir: &Path,
) -> Result<Vec<Passage>> {
    let metadata = path
        .metadata()
        .with_context(|| format!("PDF 파일을 확인할 수 없습니다: {}", path.display()))?;
    if metadata.len() > MAX_PDF_BYTES {
        bail!("PDF 파일이 허용 크기(100 MiB)를 넘었습니다.");
    }
    let raw = fs::read(path)?;
    if raw.len() as u64 > MAX_PDF_BYTES {
        bail!("PDF 파일이 허용 크기(100 MiB)를 넘었습니다.");
    }
    let document = lopdf::Document::load_mem(&raw).context("PDF 파일을 읽을 수 없습니다")?;
    if document.is_encrypted() {
        bail!("암호화된 PDF는 본문을 읽을 수 없습니다.");
    }
    let pages = ordered_pages(&document)?;
    let total = pages.len().min(MAX_PAGES);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut consumed = 0usize;
    #[cfg(windows)]
    let mut renderer: Option<PdfRenderer> = None;
    #[cfg(windows)]
    let mut renderer_attempted = false;
    #[cfg(windows)]
    let mut renderer_error: Option<String> = None;
    for (index, &object_id) in pages.iter().take(total).enumerate() {
        let number = index as u32 + 1;
        if cancelled.load(Ordering::Relaxed) {
            bail!("PDF 읽기를 취소했습니다.");
        }
        progress(index as u32 + 1, total as u32, "pdf_text");
        let mut text = String::new();
        let mut ocr_used = false;
        let page_text = page_text(&document, object_id);
        let (sparse, mut page_error) = match page_text {
            Ok(found) => {
                text = found.trim().to_owned();
                (
                    text.chars().filter(|c| c.is_alphanumeric()).count() < OCR_TEXT_THRESHOLD,
                    None,
                )
            }
            Err(error) => (
                true,
                Some(format!(
                    "텍스트 읽기 실패: {}",
                    error.to_string().chars().take(200).collect::<String>()
                )),
            ),
        };
        let image_page = page_has_images(&document, object_id);
        let content_page = !document.get_page_contents(object_id).is_empty();
        if sparse && (image_page || (text.is_empty() && content_page)) {
            if cancelled.load(Ordering::Relaxed) {
                bail!("PDF 읽기를 취소했습니다.");
            }
            progress(index as u32 + 1, total as u32, "pdf_ocr");
            #[cfg(windows)]
            {
                if !renderer_attempted {
                    renderer_attempted = true;
                    match PdfRenderer::open(path) {
                        Ok(opened) => renderer = Some(opened),
                        Err(error) => renderer_error = Some(error.to_string()),
                    }
                }
                if let Some(renderer) = &renderer {
                    match renderer.recognize(
                        index as u32,
                        max_chars.saturating_sub(consumed),
                        model_dir,
                    ) {
                        Ok(recognized) => {
                            text = merge_text(&text, &recognized);
                            ocr_used = true;
                            page_error = None;
                        }
                        Err(error) => {
                            page_error = Some(format!(
                                "OCR 실패: {}",
                                error.to_string().chars().take(180).collect::<String>()
                            ))
                        }
                    }
                } else {
                    page_error = Some(format!(
                        "PDF 렌더러를 열 수 없습니다: {}",
                        renderer_error.as_deref().unwrap_or("원인을 알 수 없습니다")
                    ));
                }
            }
            #[cfg(not(windows))]
            {
                page_error = Some("스캔 PDF OCR은 Windows에서 지원됩니다.".to_owned());
            }
            if cancelled.load(Ordering::Relaxed) {
                bail!("PDF 읽기를 취소했습니다.");
            }
        }
        let text: String = text
            .chars()
            .take(max_chars.saturating_sub(consumed))
            .collect::<String>()
            .trim()
            .to_owned();
        if !text.is_empty() {
            consumed += text.chars().count();
            output.push(Passage {
                text,
                location: format!("{number}페이지{}", if ocr_used { " OCR" } else { "" }),
            });
        }
        if let Some(error) = page_error {
            errors.push(format!("{number}페이지: {error}"));
        }
        if consumed >= max_chars {
            break;
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        bail!("PDF 읽기를 취소했습니다.");
    }
    if !errors.is_empty() {
        return Err(ExtractionError {
            passages: output,
            message: format!(
                "PDF {}개 페이지 읽기 실패. {}",
                errors.len(),
                errors
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
        .into());
    }
    Ok(output)
}

fn ordered_pages(document: &lopdf::Document) -> Result<Vec<ObjectId>> {
    fn visit(
        document: &lopdf::Document,
        id: ObjectId,
        depth: usize,
        visited: &mut std::collections::HashSet<ObjectId>,
        pages: &mut Vec<ObjectId>,
    ) -> Result<()> {
        if depth > 128 || pages.len() >= MAX_PAGES {
            return Ok(());
        }
        if !visited.insert(id) {
            return Ok(());
        }
        let dict = document.get_dictionary(id)?;
        match dict.get_type()? {
            b"Page" => pages.push(id),
            b"Pages" => {
                let kids = dict.get_deref(b"Kids", document)?.as_array()?;
                for kid in kids {
                    if let Ok(child) = kid.as_reference() {
                        // A damaged page object can be absent from lopdf's object
                        // table even though a valid page-tree Kids array refers
                        // to it. Keep that leaf as a placeholder so the native
                        // Windows PDF renderer can still render/OCR it.
                        if document.objects.contains_key(&child) {
                            visit(document, child, depth + 1, visited, pages)?;
                        } else if visited.insert(child) {
                            pages.push(child);
                        }
                        if pages.len() >= MAX_PAGES {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    let root = document.catalog()?.get(b"Pages")?.as_reference()?;
    let mut pages = Vec::new();
    visit(
        document,
        root,
        0,
        &mut std::collections::HashSet::new(),
        &mut pages,
    )?;
    Ok(pages)
}

fn page_text(document: &lopdf::Document, page_id: ObjectId) -> Result<String> {
    if !document.objects.contains_key(&page_id) {
        bail!(
            "페이지 객체를 찾을 수 없습니다: {} {}",
            page_id.0,
            page_id.1
        );
    }
    let fonts = document.get_page_fonts(page_id)?;
    let content = lopdf::content::Content::decode(&document.get_page_content(page_id)?)?;
    let mut output = String::new();
    let mut current_font: Option<Vec<u8>> = None;
    for operation in content.operations {
        match operation.operator.as_str() {
            "Tf" => {
                current_font = operation
                    .operands
                    .first()
                    .and_then(|name| name.as_name().ok())
                    .map(<[u8]>::to_vec);
            }
            "Tj" | "TJ" => {
                if let Some(font_name) = &current_font {
                    collect_pdf_text(
                        &mut output,
                        document,
                        &fonts,
                        font_name,
                        &operation.operands,
                    )?;
                }
            }
            "ET" => {
                if !output.ends_with('\n') {
                    output.push('\n');
                }
            }
            _ => {}
        }
    }
    Ok(output)
}

fn collect_pdf_text(
    output: &mut String,
    document: &lopdf::Document,
    fonts: &std::collections::BTreeMap<Vec<u8>, &lopdf::Dictionary>,
    font_name: &[u8],
    operands: &[Object],
) -> Result<()> {
    let Some(font) = fonts.get(font_name) else {
        return Ok(());
    };
    let encoding = font.get_font_encoding(document)?;
    for operand in operands {
        match operand {
            Object::String(bytes, _) => {
                output.push_str(&lopdf::Document::decode_text(&encoding, bytes)?)
            }
            Object::Array(values) => {
                collect_pdf_text(output, document, fonts, font_name, values)?;
                output.push(' ');
            }
            Object::Integer(value) if *value < -100 => output.push(' '),
            _ => {}
        }
    }
    Ok(())
}

fn page_has_images(document: &lopdf::Document, page_id: lopdf::ObjectId) -> bool {
    if !document.objects.contains_key(&page_id) {
        return true;
    }
    let Ok(content) = document.get_page_content(page_id) else {
        return true;
    };
    // PDF's `Do` operator paints an image or form XObject. This shallow scan
    // avoids unpacking image data or recursively walking untrusted resources.
    let content = String::from_utf8_lossy(&content);
    content.split_ascii_whitespace().any(|token| token == "Do")
}

fn merge_text(existing: &str, recognized: &str) -> String {
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let mut known = normalize(existing);
    let mut additions = Vec::new();
    for line in recognized.lines() {
        let compact = normalize(line);
        if !compact.is_empty() && !known.contains(&compact) {
            additions.push(line);
            known.push_str(&compact);
        }
    }
    [existing.to_owned(), additions.join("\n")]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(windows)]
struct PdfRenderer {
    document: windows::Data::Pdf::PdfDocument,
    _apartment: crate::ocr::Apartment,
}

#[cfg(windows)]
impl PdfRenderer {
    fn open(path: &Path) -> Result<Self> {
        use windows::Data::Pdf::PdfDocument;
        use windows::Storage::StorageFile;
        use windows::core::HSTRING;
        let apartment = crate::ocr::apartment()?;
        let file =
            StorageFile::GetFileFromPathAsync(&HSTRING::from(path.to_string_lossy().as_ref()))?
                .join()?;
        let document = PdfDocument::LoadFromFileAsync(&file)?.join()?;
        Ok(Self {
            document,
            _apartment: apartment,
        })
    }

    fn recognize(&self, index: u32, limit: usize, model_dir: &Path) -> Result<String> {
        use windows::Data::Pdf::PdfPageRenderOptions;
        use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
        let page = self.document.GetPage(index)?;
        let size = page.Size()?;
        if !size.Width.is_finite()
            || !size.Height.is_finite()
            || size.Width <= 0.0
            || size.Height <= 0.0
        {
            bail!("PDF 페이지 크기가 올바르지 않습니다.");
        }
        // Windows PDF sizes are device-independent pixels. Render at 200 DPI,
        // then cap both dimensions and total pixels before allocation.
        let scale: f32 = (200.0f32 / 96.0)
            .min(MAX_RENDER_SIDE / size.Width)
            .min(MAX_RENDER_SIDE / size.Height)
            .min((MAX_RENDER_PIXELS / (size.Width * size.Height)).sqrt())
            * 0.999;
        let width = (size.Width * scale).ceil() as u32;
        let height = (size.Height * scale).ceil() as u32;
        if width == 0
            || height == 0
            || width.max(height) > MAX_RENDER_SIDE as u32
            || width as u64 * height as u64 > MAX_RENDER_PIXELS as u64
        {
            bail!("PDF 페이지 렌더링 해상도가 허용 한도를 넘었습니다.");
        }
        let options = PdfPageRenderOptions::new()?;
        options.SetDestinationWidth(width)?;
        options.SetDestinationHeight(height)?;
        let stream = InMemoryRandomAccessStream::new()?;
        page.RenderWithOptionsToStreamAsync(&stream, &options)?
            .join()?;
        stream.Seek(0)?;
        let size = stream.Size()?;
        if size == 0 || size > (32 * 1024 * 1024) {
            bail!("렌더링된 PDF 페이지가 허용 크기를 넘었습니다.");
        }
        let input = stream.GetInputStreamAt(0)?;
        let reader = DataReader::CreateDataReader(&input)?;
        reader.LoadAsync(size as u32)?.join()?;
        let mut png = vec![0; size as usize];
        reader.ReadBytes(&mut png)?;
        let image = image::load_from_memory(&png)
            .context("Windows PDF 렌더러가 페이지 이미지로 변환하지 못했습니다")?
            .to_rgb8();
        let text = crate::ocr::recognize_rgb(&image, limit, model_dir)?;
        page.Close()?;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_page_tree_references_to_unreadable_page_objects_for_render_fallback() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("rental.pdf");
        let document = lopdf::Document::load(&path).expect("rental fixture should load");
        let pages = ordered_pages(&document).expect("page tree should be traversable");

        assert_eq!(pages.len(), 2);
        assert!(
            pages
                .iter()
                .any(|page| !document.objects.contains_key(page)),
            "the malformed cover page should remain represented for native rendering"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pdf_renderer_works_without_package_identity() {
        use windows::Data::Pdf::PdfPageRenderOptions;
        use windows::Storage::Streams::InMemoryRandomAccessStream;

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("meeting.pdf");
        let renderer =
            PdfRenderer::open(&path).expect("Windows PDF renderer should load a local PDF");
        assert!(renderer.document.PageCount().expect("page count") > 0);
        let page = renderer.document.GetPage(0).expect("first page");
        let options = PdfPageRenderOptions::new().expect("render options");
        let stream = InMemoryRandomAccessStream::new().expect("in-memory output stream");
        page.RenderWithOptionsToStreamAsync(&stream, &options)
            .expect("render request")
            .join()
            .expect("render page without package identity");
        assert!(stream.Size().expect("rendered image size") > 0);
        page.Close().expect("close page");
    }

    #[test]
    fn merge_adds_only_ocr_lines_missing_from_pdf_text() {
        assert_eq!(
            merge_text("계약 기간 12개월", "계약기간 12개월\n보증금 반환"),
            "계약 기간 12개월\n보증금 반환"
        );
    }

    #[test]
    fn render_scale_respects_memory_caps() {
        let width = 10_000.0f32;
        let height = 20_000.0f32;
        let scale: f32 = (200.0f32 / 96.0)
            .min(MAX_RENDER_SIDE / width)
            .min(MAX_RENDER_SIDE / height)
            .min((MAX_RENDER_PIXELS / (width * height)).sqrt())
            * 0.999;
        assert!((width * scale).ceil() <= MAX_RENDER_SIDE);
        assert!((width * scale).ceil() * (height * scale).ceil() <= MAX_RENDER_PIXELS);
    }
}
