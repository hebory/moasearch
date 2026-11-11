"""Pinned downloads for the built-in, on-device GGUF and OCR models."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import tempfile
from typing import Callable
from urllib.request import Request, urlopen


_MAX_DOWNLOAD_BYTES = 1024 * 1024 * 1024
_CATALOG = {
    "embedding": {
        "repo": "Qwen/Qwen3-Embedding-0.6B-GGUF",
        "filename": "Qwen3-Embedding-0.6B-Q8_0.gguf",
        "sha256": "06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439",
        "bytes": 639150592,
        "description": "한국어를 포함한 다국어 파일 의미 검색용 임베딩 모델 (약 639 MB)",
        "source": "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/blob/main/Qwen3-Embedding-0.6B-Q8_0.gguf",
        "url": "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf",
    },
    "chat": {
        "repo": "Qwen/Qwen3-0.6B-GGUF",
        "filename": "Qwen3-0.6B-Q8_0.gguf",
        "sha256": "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
        "bytes": 639446688,
        "description": "근거가 있는 한국어 답변 생성용 대화 모델 (약 639 MB)",
        "source": "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/blob/main/Qwen3-0.6B-Q8_0.gguf",
        "url": "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf",
    },
}

# RapidOCR v3.10.0's official manifest pins these PP-OCRv5 ONNX files.
_OCR_MODELS = {
    "det": {
        "filename": "ch_PP-OCRv5_det_mobile.onnx",
        "sha256": "4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae",
        "bytes": 4819576,
        "url": "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/det/ch_PP-OCRv5_det_mobile.onnx",
    },
    "rec": {
        "filename": "korean_PP-OCRv5_rec_mobile.onnx",
        "sha256": "cd6e2ea50f6943ca7271eb8c56a877a5a90720b7047fe9c41a2e541a25773c9b",
        "bytes": 13488748,
        "url": "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/rec/korean_PP-OCRv5_rec_mobile.onnx",
    },
}


def ocr_model_paths(directory: Path) -> dict[str, Path]:
    return {role: Path(directory).resolve() / spec["filename"] for role, spec in _OCR_MODELS.items()}


def verify_ocr_models(directory: Path) -> dict[str, Path]:
    paths = ocr_model_paths(directory)
    for role, path in paths.items():
        if not path.is_file() or path.is_symlink():
            raise ValueError("OCR 모델을 먼저 준비하세요. 설정의 'OCR 모델 받기'를 사용하세요.")
        if path.stat().st_size > _MAX_DOWNLOAD_BYTES or _digest(path) != _OCR_MODELS[role]["sha256"]:
            raise ValueError(f"OCR 모델 SHA-256 검증에 실패했습니다: {path.name}")
    return paths


def download_ocr_models(directory: Path, progress=None) -> Path:
    completed = 0
    estimates = {role: spec["bytes"] for role, spec in _OCR_MODELS.items()}
    for role, spec in _OCR_MODELS.items():
        def report(received, total):
            estimates[role] = total
            if progress:
                progress(completed + received, sum(estimates.values()))
        target = _download_file(spec, directory, report, gguf=False)
        completed += target.stat().st_size
        estimates[role] = target.stat().st_size
        if progress:
            progress(completed, sum(estimates.values()))
    verify_ocr_models(directory)
    return Path(directory).resolve()


def catalog() -> dict:
    """Return public model metadata, without exposing mutable internals."""
    return {kind: {key: value for key, value in item.items() if key != "url"}
            for kind, item in _CATALOG.items()} | {"ocr": {
                "description": "한국어·영어 이미지 OCR (PP-OCRv5, CPU)",
                "source": "https://github.com/RapidAI/RapidOCR/blob/v3.10.0/python/rapidocr/default_models.yaml",
                "files": {role: {k: v for k, v in spec.items() if k != "url"} for role, spec in _OCR_MODELS.items()},
            }}


def _digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def download_model(
    kind: str,
    directory: Path,
    progress: Callable[[int, int], None] | None = None,
) -> Path:
    """Download and authenticate a pinned model, installing it atomically.

    A pre-existing target is reused only when its bytes match the pinned
    checksum and GGUF signature. Any other existing target remains untouched.
    Partial downloads are stored beside the target and removed on every error.
    """
    if kind == "ocr":
        return download_ocr_models(directory, progress)
    if kind not in _CATALOG:
        raise ValueError("모델 종류는 embedding, chat 또는 ocr이어야 합니다.")
    return _download_file(_CATALOG[kind], directory, progress, gguf=True)


def _download_file(model, directory, progress=None, *, gguf):
    directory = Path(directory).expanduser().resolve()
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / model["filename"]

    if target.is_symlink():
        raise FileExistsError("같은 이름의 심볼릭 링크가 있어 모델을 설치하지 않았습니다.")
    if target.exists():
        if (not target.is_file() or target.stat().st_size > _MAX_DOWNLOAD_BYTES
                or _digest(target) != model["sha256"]):
            raise FileExistsError("같은 이름의 기존 파일이 올바른 모델이 아니어서 덮어쓰지 않았습니다.")
        with target.open("rb") as stream:
            if gguf and stream.read(4) != b"GGUF":
                raise ValueError("기존 모델 파일의 GGUF 서명이 올바르지 않습니다.")
        return target

    fd, temp_name = tempfile.mkstemp(prefix=f".{model['filename']}.", suffix=".part", dir=directory)
    temp = Path(temp_name)
    try:
        request = Request(model["url"], headers={"User-Agent": "MoaSearch/0.1"})
        digest = hashlib.sha256()
        received = 0
        with os.fdopen(fd, "wb") as output, urlopen(request, timeout=60) as response:
            header = response.headers.get("Content-Length")
            total = int(header) if header and header.isdigit() else model["bytes"]
            while True:
                chunk = response.read(min(1024 * 1024, _MAX_DOWNLOAD_BYTES - received + 1))
                if not chunk:
                    break
                received += len(chunk)
                if received > _MAX_DOWNLOAD_BYTES:
                    raise ValueError("모델 다운로드가 1 GiB 상한을 넘었습니다.")
                output.write(chunk)
                digest.update(chunk)
                if progress:
                    progress(received, total)
            output.flush()
            os.fsync(output.fileno())

        if received < 4:
            raise ValueError("다운로드 파일이 너무 작습니다.")
        with temp.open("rb") as stream:
            if gguf and stream.read(4) != b"GGUF":
                raise ValueError("다운로드 파일이 GGUF 형식이 아닙니다.")
        if digest.hexdigest() != model["sha256"]:
            raise ValueError("모델 SHA-256 검증에 실패했습니다.")

        # The temporary name is in the target directory, so rename is atomic.
        # Windows rename refuses an existing destination instead of replacing
        # a file another process may have written during this download.
        try:
            if os.name != "nt" and target.exists():
                raise FileExistsError(target)
            os.rename(temp, target)
        except FileExistsError:
            if (target.is_file() and not target.is_symlink()
                    and target.stat().st_size <= _MAX_DOWNLOAD_BYTES
                    and _digest(target) == model["sha256"]):
                with target.open("rb") as stream:
                    if not gguf or stream.read(4) == b"GGUF":
                        return target
            raise FileExistsError("같은 이름의 파일이 다운로드 중 생겨 모델을 덮어쓰지 않았습니다.")
        return target
    finally:
        try:
            temp.unlink(missing_ok=True)
        except OSError:
            pass
