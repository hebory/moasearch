//! Offline Korean OCR using the pinned PP-OCRv5 ONNX models.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use image::{AnimationDecoder, ImageDecoder, ImageReader, RgbImage};
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};
use serde_json::{Value, json};

use crate::extract::Passage;

const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PIXELS: u64 = 40_000_000;
const MAX_LONG_SIDE: u32 = 4096;
const MAX_FRAMES: usize = 16;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

type ModelFileKey = (PathBuf, u64, Option<SystemTime>);
type ModelKey = (ModelFileKey, ModelFileKey);
static ENGINE: OnceLock<Mutex<Option<(ModelKey, OAROCR)>>> = OnceLock::new();

#[cfg(windows)]
pub(crate) fn apartment() -> Result<Apartment> {
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    let result = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let initialized = result.is_ok();
    if let Err(error) = result {
        if error.code().0 != 0x80010106u32 as i32 {
            return Err(error.into());
        }
    }
    Ok(Apartment(initialized))
}

#[cfg(windows)]
pub(crate) struct Apartment(bool);

#[cfg(windows)]
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                windows::Win32::System::WinRT::RoUninitialize();
            }
        }
    }
}

fn engine(model_dir: &Path) -> Result<std::sync::MutexGuard<'static, Option<(ModelKey, OAROCR)>>> {
    let (detector, recognizer) = crate::models::ocr_paths(model_dir);
    let file_key = |path: &Path| -> Result<ModelFileKey> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("OCR 모델을 찾을 수 없습니다: {}", path.display()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!(
                "일반 파일이 아닌 OCR 모델은 사용할 수 없습니다: {}",
                path.display()
            );
        }
        Ok((path.to_path_buf(), metadata.len(), metadata.modified().ok()))
    };
    let key = (file_key(&detector)?, file_key(&recognizer)?);
    let registry = ENGINE.get_or_init(|| Mutex::new(None));
    let mut cached = registry.lock().unwrap_or_else(|error| error.into_inner());
    if cached.as_ref().is_none_or(|(old_key, _)| old_key != &key) {
        // The expensive hashes are checked once per model pair identity, never
        // for every page or image in a large indexing job.
        let (detector, recognizer) = crate::models::verify_ocr_models(model_dir)?;
        // Microsoft binaries enable telemetry by default. Configure the process
        // before OAR creates any sessions so all OCR stays offline.
        ort::init().with_telemetry(false).commit();
        let runtime = OAROCRBuilder::new(
            detector.to_string_lossy().into_owned(),
            recognizer.to_string_lossy().into_owned(),
            PathBuf::new(),
        )
        .character_dict_content(crate::models::OCR_DICT)
        .image_batch_size(1)
        .region_batch_size(4)
        .build()
        .context("PP-OCRv5 ONNX 런타임을 시작하지 못했습니다")?;
        *cached = Some((key.clone(), runtime));
    }
    Ok(cached)
}

/// Report whether the pinned local models are present without hashing 18 MiB
/// on every status poll. SHA-256 is checked on first load and when file identity changes.
pub fn status(model_dir: &Path) -> Value {
    let (detector, recognizer) = crate::models::ocr_paths(model_dir);
    let present = [&detector, &recognizer].iter().all(|path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    });
    let sized = present
        && std::fs::metadata(&detector).is_ok_and(|m| m.len() == crate::models::OCR_DET.bytes)
        && std::fs::metadata(&recognizer).is_ok_and(|m| m.len() == crate::models::OCR_REC.bytes);
    json!({
        "available": true,
        "ready": sized,
        "languages": ["ko", "en"],
        "message": if sized {
            "PP-OCRv5 한국어 OCR 모델이 준비되었습니다. 첫 실행 때 무결성을 확인합니다."
        } else {
            "OCR 모델이 없습니다. 모델 설정에서 PP-OCRv5 한국어 모델을 준비하세요."
        }
    })
}

fn composite_white(image: image::RgbaImage) -> RgbImage {
    let (width, height) = image.dimensions();
    let mut rgb = RgbImage::new(width, height);
    for (dst, src) in rgb.pixels_mut().zip(image.pixels()) {
        let alpha = u16::from(src[3]);
        for channel in 0..3 {
            let value = u16::from(src[channel]) * alpha + 255 * (255 - alpha);
            dst[channel] = ((value + 127) / 255) as u8;
        }
    }
    rgb
}

fn bound_rgb(mut image: RgbImage) -> Result<RgbImage> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("이미지 해상도가 허용 한도(4천만 픽셀)를 넘었습니다.");
    }
    let longest = width.max(height);
    if longest > MAX_LONG_SIDE {
        let scale = MAX_LONG_SIDE as f64 / f64::from(longest);
        let resized_width = (f64::from(width) * scale).round().max(1.0) as u32;
        let resized_height = (f64::from(height) * scale).round().max(1.0) as u32;
        image = image::imageops::resize(
            &image,
            resized_width,
            resized_height,
            image::imageops::FilterType::Lanczos3,
        );
    }
    Ok(image)
}

/// Run the cached local ONNX pipeline for one decoded RGB frame.
pub(crate) fn recognize_rgb(
    image: &RgbImage,
    max_chars: usize,
    model_dir: &Path,
) -> Result<String> {
    if max_chars == 0 {
        return Ok(String::new());
    }
    let image = bound_rgb(image.clone())?;
    let cached = engine(model_dir)?;
    let runtime = &cached.as_ref().expect("engine initialized").1;
    let results = runtime
        .predict(vec![image])
        .context("PP-OCRv5 텍스트 인식을 실패했습니다")?;
    let text = results
        .into_iter()
        .flat_map(|result| result.text_regions)
        .filter_map(|region| region.text)
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(text
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned())
}

fn push_frame(
    image: RgbImage,
    frame_index: usize,
    label_frames: bool,
    max_chars: usize,
    consumed: &mut usize,
    model_dir: &Path,
    output: &mut Vec<Passage>,
) -> Result<()> {
    let text = recognize_rgb(&image, max_chars.saturating_sub(*consumed), model_dir)?;
    if let Some(passage) = Passage::new(
        text,
        if label_frames {
            format!("이미지 {} OCR", frame_index + 1)
        } else {
            "이미지 OCR".into()
        },
    ) {
        *consumed += passage.text.chars().count();
        output.push(passage);
    }
    Ok(())
}

fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PIXELS as u32);
    limits.max_image_height = Some(MAX_PIXELS as u32);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

fn tiff_page(decoder: &mut tiff::decoder::Decoder<BufReader<File>>) -> Result<RgbImage> {
    let (width, height) = decoder.dimensions()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("이미지 해상도가 허용 한도(4천만 픽셀)를 넘었습니다.");
    }
    let color = decoder.colortype()?;
    let data = decoder.read_image()?;
    let pixels = match (color, data) {
        (tiff::ColorType::Gray(8), tiff::decoder::DecodingResult::U8(data)) => {
            let image = image::GrayImage::from_raw(width, height, data)
                .context("TIFF 회색조 페이지 크기가 올바르지 않습니다")?;
            image::DynamicImage::ImageLuma8(image).to_rgb8()
        }
        (tiff::ColorType::Gray(16), tiff::decoder::DecodingResult::U16(data)) => {
            let gray = data.into_iter().map(|v| (v / 257) as u8).collect();
            let image = image::GrayImage::from_raw(width, height, gray)
                .context("TIFF 회색조 페이지 크기가 올바르지 않습니다")?;
            image::DynamicImage::ImageLuma8(image).to_rgb8()
        }
        (tiff::ColorType::GrayA(8), tiff::decoder::DecodingResult::U8(data)) => {
            let rgba = data
                .chunks_exact(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect();
            composite_white(
                image::RgbaImage::from_raw(width, height, rgba)
                    .context("TIFF 투명도 페이지 크기가 올바르지 않습니다")?,
            )
        }
        (tiff::ColorType::RGB(8), tiff::decoder::DecodingResult::U8(data)) => {
            RgbImage::from_raw(width, height, data)
                .context("TIFF RGB 페이지 크기가 올바르지 않습니다")?
        }
        (tiff::ColorType::RGBA(8), tiff::decoder::DecodingResult::U8(data)) => composite_white(
            image::RgbaImage::from_raw(width, height, data)
                .context("TIFF RGBA 페이지 크기가 올바르지 않습니다")?,
        ),
        (tiff::ColorType::RGB(16), tiff::decoder::DecodingResult::U16(data)) => {
            let rgb = data.into_iter().map(|v| (v / 257) as u8).collect();
            RgbImage::from_raw(width, height, rgb)
                .context("TIFF RGB16 페이지 크기가 올바르지 않습니다")?
        }
        (tiff::ColorType::RGBA(16), tiff::decoder::DecodingResult::U16(data)) => {
            let rgba = data.into_iter().map(|v| (v / 257) as u8).collect();
            composite_white(
                image::RgbaImage::from_raw(width, height, rgba)
                    .context("TIFF RGBA16 페이지 크기가 올바르지 않습니다")?,
            )
        }
        (color, _) => bail!("지원하지 않는 TIFF 색상 형식입니다: {color:?}"),
    };
    Ok(pixels)
}

pub fn extract_image(
    path: &Path,
    max_chars: usize,
    cancelled: &AtomicBool,
    model_dir: &Path,
) -> Result<Vec<Passage>> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !["png", "jpg", "jpeg", "bmp", "tif", "tiff", "webp", "gif"].contains(&ext.as_str()) {
        bail!("지원하지 않는 이미지 형식입니다.");
    }
    let metadata = path
        .metadata()
        .with_context(|| format!("이미지 파일을 확인할 수 없습니다: {}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!("이미지 파일이 허용 크기(100 MiB)를 넘었습니다.");
    }
    if max_chars == 0 {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    let mut consumed = 0usize;

    if ext == "tif" || ext == "tiff" {
        let file = File::open(path)?;
        let mut decoder = tiff::decoder::Decoder::new(BufReader::new(file))?;
        let mut index = 0usize;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("색인을 취소했습니다.");
            }
            let image = tiff_page(&mut decoder)?;
            let label_frames = index > 0 || decoder.more_images();
            push_frame(
                image,
                index,
                label_frames,
                max_chars,
                &mut consumed,
                model_dir,
                &mut output,
            )?;
            index += 1;
            if consumed >= max_chars || index >= MAX_FRAMES || !decoder.more_images() {
                break;
            }
            decoder.next_image()?;
        }
    } else if ext == "gif" {
        let file = File::open(path)?;
        let mut decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))?;
        decoder.set_limits(image_limits())?;
        for (index, frame) in decoder.into_frames().take(MAX_FRAMES).enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                bail!("색인을 취소했습니다.");
            }
            let image = frame?.into_buffer();
            let image = composite_white(image);
            push_frame(
                image,
                index,
                true,
                max_chars,
                &mut consumed,
                model_dir,
                &mut output,
            )?;
            if consumed >= max_chars {
                break;
            }
        }
    } else {
        let mut reader = ImageReader::open(path)?.with_guessed_format()?;
        reader.limits(image_limits());
        let image = reader
            .decode()
            .context("이미지 파일을 해석하지 못했습니다")?;
        let image = if image.color().has_alpha() {
            composite_white(image.to_rgba8())
        } else {
            image.to_rgb8()
        };
        push_frame(
            image,
            0,
            false,
            max_chars,
            &mut consumed,
            model_dir,
            &mut output,
        )?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_does_not_hash_model_files_on_poll() {
        let absent = status(Path::new("missing-ocr-model-directory"));
        assert_eq!(absent["available"], true);
        assert_eq!(absent["ready"], false);
    }

    #[test]
    fn alpha_is_composited_on_white_and_pixels_are_bounded() {
        let rgba = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
        let rgb = composite_white(rgba);
        assert_eq!(rgb.get_pixel(0, 0), &image::Rgb([255, 255, 255]));
        let large = RgbImage::new(5000, 1000);
        assert_eq!(bound_rgb(large).unwrap().dimensions(), (4096, 819));
    }
}
