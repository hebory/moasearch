"""Bounded, offline image OCR using bundled RapidOCR ONNX models."""

from __future__ import annotations

from pathlib import Path
import threading
import warnings

IMAGE_SUFFIXES = frozenset({".png", ".jpg", ".jpeg", ".bmp", ".tif", ".tiff", ".webp"})

_MAX_SOURCE_BYTES = 100 * 1024 * 1024
_MAX_PIXELS = 40_000_000
_MAX_LONG_SIDE = 4096
_MAX_FRAMES = 16
_MODEL_FILES = {
    "det": "ch_PP-OCRv5_det_mobile.onnx",
    "rec": "korean_PP-OCRv5_rec_mobile.onnx",
}

_engine_lock = threading.RLock()
_engine = None
_engine_key: tuple[tuple[str, int, int, int], ...] | None = None


def status(model_dir: Path) -> dict:
    """Report OCR dependency availability and presence of local model files."""
    try:
        from rapidocr import RapidOCR  # noqa: F401
        import onnxruntime  # noqa: F401
        import PIL  # noqa: F401
        available = True
    except (ImportError, OSError):
        available = False

    model_dir = Path(model_dir)
    present = all((model_dir / name).is_file() and not (model_dir / name).is_symlink()
                  for name in _MODEL_FILES.values())
    ready = available and present
    if not available:
        message = "이미지 OCR 패키지가 설치되지 않았습니다."
    elif not present:
        message = "이미지 OCR 모델이 없습니다. 설정에서 OCR 모델을 내려받아 주세요."
    else:
        message = "이미지·스캔 PDF용 한국어·영어 OCR 모델이 준비되었습니다. 최초 사용 시 무결성을 확인합니다."
    return {"available": available, "ready": ready, "message": message}


def _load_engine(model_dir: Path):
    """Load the only supported ONNX engine with explicit local model paths."""
    global _engine, _engine_key
    from .models import ocr_model_paths, verify_ocr_models

    paths = {kind: Path(path).resolve() for kind, path in ocr_model_paths(model_dir).items()}
    with _engine_lock:
        try:
            key = tuple(
                (str(paths[kind]), stat.st_size, stat.st_mtime_ns, getattr(stat, "st_ino", 0))
                for kind in ("det", "rec")
                for stat in (paths[kind].stat(),)
            )
        except OSError:
            # Keep the registry's user-facing missing/corrupt-model error.
            verify_ocr_models(model_dir)
            raise
        if _engine is not None and _engine_key == key:
            return _engine
        # Hash-verify only when a new local model pair is first loaded.
        verify_ocr_models(model_dir)
        try:
            from rapidocr import EngineType, LangRec, ModelType, OCRVersion, RapidOCR
        except (ImportError, OSError) as exc:
            raise RuntimeError("이미지 OCR 패키지가 설치되지 않았습니다.") from exc
        # Supplying both paths and disabling text direction classification
        # prevents RapidOCR from resolving or downloading any other model.
        params = {
            "Global.use_cls": False,
            "EngineConfig.onnxruntime.intra_op_num_threads": 2,
            "EngineConfig.onnxruntime.inter_op_num_threads": 1,
            "Det.engine_type": EngineType.ONNXRUNTIME,
            "Det.model_path": str(paths["det"]),
            "Det.ocr_version": OCRVersion.PPOCRV5,
            "Det.model_type": ModelType.MOBILE,
            "Rec.engine_type": EngineType.ONNXRUNTIME,
            "Rec.model_path": str(paths["rec"]),
            "Rec.lang_type": LangRec.KOREAN,
            "Rec.ocr_version": OCRVersion.PPOCRV5,
            "Rec.model_type": ModelType.MOBILE,
        }
        try:
            _engine = RapidOCR(params=params)
        except Exception as exc:
            _engine = None
            _engine_key = None
            raise RuntimeError(f"이미지 OCR 엔진을 초기화하지 못했습니다: {exc}") from exc
        _engine_key = key
        return _engine


def _ocr_result_text(result) -> str:
    """Normalize RapidOCR's v3 result object (and empty result) to text."""
    if result is None:
        return ""
    txts = getattr(result, "txts", None)
    if txts is None and isinstance(result, tuple):
        # Keep compatibility with RapidOCR releases exposing (boxes, txts, scores).
        txts = result[1] if len(result) > 1 else None
    if not txts:
        return ""
    return "\n".join(str(text).strip() for text in txts if text and str(text).strip())


def _prepare_frame(image, np):
    """Check dimensions before copying, orient, flatten, and make a BGR array."""
    from PIL import Image, ImageOps

    if image.width * image.height > _MAX_PIXELS:
        raise ValueError("이미지 해상도가 허용 한도(4천만 픽셀)를 넘었습니다.")
    image = ImageOps.exif_transpose(image)
    if image.mode in ("RGBA", "LA") or "transparency" in image.info:
        rgba = image.convert("RGBA")
        background = Image.new("RGB", rgba.size, "white")
        background.paste(rgba, mask=rgba.getchannel("A"))
        image = background
    else:
        image = image.convert("RGB")
    longest = max(image.size)
    if longest > _MAX_LONG_SIDE:
        ratio = _MAX_LONG_SIDE / longest
        image = image.resize((max(1, round(image.width * ratio)),
                              max(1, round(image.height * ratio))), Image.Resampling.LANCZOS)
    # RapidOCR treats ndarray inputs as BGR. The path loader normally does
    # this conversion internally; pass the already bounded pixels explicitly.
    return np.asarray(image)[:, :, ::-1].copy()


def recognize_frame(image, max_chars: int, model_dir: Path) -> str:
    """Read a bounded PIL image, including an in-memory rendered PDF page."""
    if max_chars <= 0:
        return ""
    import numpy as np

    pixels = _prepare_frame(image, np)
    # RapidOCR retains preprocessing state, so all input types share this lock.
    with _engine_lock:
        result = _load_engine(Path(model_dir))(pixels)
    return _ocr_result_text(result)[:max_chars].strip()


def extract_image(path: Path, max_chars: int, model_dir: Path) -> list[tuple[str, str]]:
    """Extract Korean/English OCR text from an image without network access.

    At most 100 MiB of source data, 40 million pixels per frame, 16 frames,
    and ``max_chars`` output characters are processed. Animated images and
    multipage TIFFs are handled frame by frame.
    """
    path = Path(path)
    if path.suffix.lower() not in IMAGE_SUFFIXES:
        raise ValueError("지원하지 않는 이미지 형식입니다.")
    try:
        size = path.stat().st_size
    except OSError as exc:
        raise RuntimeError(f"이미지 파일을 읽지 못했습니다: {exc}") from exc
    if size > _MAX_SOURCE_BYTES:
        raise ValueError("이미지 파일이 허용 크기(100 MiB)를 넘었습니다.")
    if max_chars <= 0:
        return []

    state = status(model_dir)
    if not state["available"]:
        raise RuntimeError(state["message"])
    if not state["ready"]:
        # verify_ocr_models below gives the same clear missing/corrupt-model
        # error used by the model manager and validates pinned checksums.
        _load_engine(Path(model_dir))

    try:
        from PIL import Image, UnidentifiedImageError
    except (ImportError, OSError) as exc:
        raise RuntimeError("이미지 OCR 패키지가 설치되지 않았습니다.") from exc

    try:
        # Pillow warns for decompression bombs before raising at its hard limit;
        # convert both cases to the same bounded, user-readable failure.
        with warnings.catch_warnings():
            warnings.simplefilter("error", Image.DecompressionBombWarning)
            with Image.open(path) as source:
                frames = min(int(getattr(source, "n_frames", 1)), _MAX_FRAMES)
                outputs: list[tuple[str, str]] = []
                total_chars = 0
                for frame_index in range(frames):
                    source.seek(frame_index)
                    if source.width * source.height > _MAX_PIXELS:
                        raise ValueError("이미지 해상도가 허용 한도(4천만 픽셀)를 넘었습니다.")
                    text = recognize_frame(source, max_chars - total_chars, model_dir)
                    if not text:
                        continue
                    text = text[: max_chars - total_chars].strip()
                    if not text:
                        break
                    outputs.append((text, "이미지 OCR" if frames == 1 else f"이미지 {frame_index + 1} OCR"))
                    total_chars += len(text)
                    if total_chars >= max_chars:
                        break
                return outputs
    except ValueError:
        raise
    except Image.DecompressionBombError as exc:
        raise ValueError("이미지 해상도가 허용 한도를 넘었습니다.") from exc
    except Image.DecompressionBombWarning as exc:
        raise ValueError("이미지 해상도가 허용 한도를 넘었습니다.") from exc
    except (UnidentifiedImageError, OSError) as exc:
        raise ValueError(f"이미지를 해석할 수 없습니다: {exc}") from exc
    except RuntimeError:
        raise
    except Exception as exc:
        raise RuntimeError(f"이미지 OCR 처리에 실패했습니다: {exc}") from exc
