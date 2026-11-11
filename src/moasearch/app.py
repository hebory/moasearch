"""Loopback-only UI and API for the local application."""

from contextlib import asynccontextmanager
import os
from pathlib import Path
import secrets
import string
import subprocess
import sys
import threading
import time

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

from .config import data_directory, excluded, within
from .engine import EmbeddedEngine
from .indexer import Indexer
from .search import Search
from .store import Store
from .ocr import status as ocr_status


class RootInput(BaseModel):
    path: str = Field(min_length=1, max_length=4096)


class DownloadInput(BaseModel):
    kind: str = Field(pattern="^(embedding|chat|ocr)$")


class SearchInput(BaseModel):
    query: str = Field(min_length=1, max_length=2000)
    limit: int = Field(default=30, ge=1, le=100)
    extension: str | None = Field(default=None, max_length=30)
    mode: str = Field(default="hybrid", pattern="^(hybrid|keyword)$")


class SettingsInput(BaseModel):
    embedding_model: str | None = Field(default=None, max_length=4096)
    chat_model: str | None = Field(default=None, max_length=4096)
    n_gpu_layers: int | None = Field(default=None, ge=-1, le=200)
    context_size: int | None = Field(default=None, ge=512, le=32768)
    scan_interval: int | None = Field(default=None, ge=0, le=86400)
    max_file_mb: int | None = Field(default=None, ge=1, le=512)


def suggestions():
    paths = [{"label": label, "path": str(Path.home() / name)} for name, label in
             [("Documents", "문서"), ("Desktop", "바탕 화면"), ("Downloads", "다운로드")]]
    if os.name == "nt":
        paths += [{"label": f"{letter}: 드라이브", "path": f"{letter}:\\"}
                  for letter in string.ascii_uppercase if Path(f"{letter}:\\").exists()]
    return [item for item in paths if Path(item["path"]).is_dir()]


def create_app(data_dir=None, engine=None):
    store = Store(Path(data_dir) if data_dir else data_directory())
    engine = engine or EmbeddedEngine(store)
    indexer = Indexer(store, engine)
    search = Search(store, engine)
    token = secrets.token_urlsafe(32)
    static = Path(__file__).parent / "static"
    download_lock = threading.Lock()
    download_state = dict(running=False, kind="", received=0, total=0, error="", message="")

    @asynccontextmanager
    async def lifespan(app):
        indexer.start_scheduler()
        yield
        indexer.close()
        engine.close()

    app = FastAPI(title="MoaSearch", lifespan=lifespan, docs_url=None, redoc_url=None, openapi_url=None)
    app.state.store, app.state.indexer, app.state.engine = store, indexer, engine
    app.state.token = token

    @app.middleware("http")
    async def local_only(request: Request, call_next):
        host = request.headers.get("host", "")
        hostname = host.rsplit(":", 1)[0] if ":" in host and not host.endswith("]") else host
        if hostname not in {"localhost", "127.0.0.1", "[::1]"}:
            return JSONResponse({"detail": "로컬 주소로만 접속할 수 있습니다."}, status_code=403)
        origin = request.headers.get("origin")
        if origin and origin != f"{request.url.scheme}://{host}":
            return JSONResponse({"detail": "외부 페이지의 요청은 허용하지 않습니다."}, status_code=403)
        if request.url.path.startswith("/api/") and not secrets.compare_digest(
                request.headers.get("x-moasearch-token", ""), token):
            return JSONResponse({"detail": "앱 페이지를 새로 고친 뒤 다시 시도하세요."}, status_code=403)
        response = await call_next(request)
        response.headers["Content-Security-Policy"] = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["Referrer-Policy"] = "no-referrer"
        response.headers["Cache-Control"] = "no-store"
        return response

    @app.exception_handler(ValueError)
    async def invalid(request, exc):
        return JSONResponse({"detail": str(exc)}, status_code=400)

    @app.get("/", response_class=HTMLResponse)
    def home():
        return (static / "index.html").read_text(encoding="utf-8").replace("__TOKEN__", token)

    @app.get("/api/status")
    def status():
        return {"counts": store.counts(), "roots": store.roots(), "index": indexer.status(),
                "settings": store.settings(), "engine": engine.status(), "suggestions": suggestions(),
                "ocr": ocr_status(store.data_dir / "models"),
                "model_download": download_state.copy()}

    @app.post("/api/models/download")
    def download_model(body: DownloadInput):
        from .models import download_model as fetch_model
        with download_lock:
            if download_state["running"]:
                raise HTTPException(409, "모델을 이미 다운로드 중입니다.")
            download_state.update(running=True, kind=body.kind, received=0, total=0, error="", message="다운로드 준비 중")
        def download():
            try:
                def progress(received, total):
                    download_state.update(received=received, total=total, message="모델 다운로드 및 검증 중")
                model_path = fetch_model(body.kind, store.data_dir / "models", progress)
                while True:
                    try:
                        with indexer.idle_changes():
                            if body.kind != "ocr":
                                store.update_settings({f"{body.kind}_model": str(model_path)})
                                engine.reset()
                        break
                    except ValueError:
                        if not indexer.status()["running"]:
                            raise
                        download_state["message"] = "다운로드 완료. 현재 색인이 끝나면 모델을 적용합니다."
                        time.sleep(.5)
                download_state["message"] = ("OCR 모델 준비 완료. 다시 색인하면 이미지의 글자를 검색할 수 있습니다."
                                             if body.kind == "ocr" else "모델 준비 완료. 다시 색인하면 의미 검색을 사용할 수 있습니다.")
            except Exception as exc:
                download_state.update(error=str(exc)[:500], message="모델 다운로드 실패")
            finally:
                download_state["running"] = False
        threading.Thread(target=download, name="moasearch-download", daemon=True).start()
        return {"started": True}

    @app.post("/api/roots")
    def add_root(body: RootInput):
        with indexer.idle_changes():
            return store.add_root(body.path)

    @app.delete("/api/roots/{root_id}")
    def remove_root(root_id: int):
        with indexer.idle_changes():
            store.remove_root(root_id)
        return {"ok": True}

    @app.post("/api/index")
    def start_index():
        return {"started": indexer.start()}

    @app.post("/api/index/cancel")
    def cancel_index():
        indexer.cancel()
        return {"ok": True}

    @app.patch("/api/settings")
    def settings(body: SettingsInput):
        if download_state["running"]:
            raise HTTPException(409, "모델 다운로드가 완료된 뒤 설정을 저장하세요.")
        with indexer.idle_changes():
            store.update_settings(body.model_dump(exclude_none=True))
            engine.reset()
        return store.settings()

    @app.post("/api/search")
    def search_files(body: SearchInput):
        return search.search(body.query, body.limit, body.extension, body.mode)

    @app.post("/api/answer")
    def answer(body: SearchInput):
        try:
            return search.answer(body.query, min(body.limit, 8))
        except RuntimeError as exc:
            raise HTTPException(503, str(exc)) from exc

    def indexed_file(file_id):
        item = store.file(file_id)
        if item is None:
            raise HTTPException(404, "색인된 파일을 찾을 수 없습니다.")
        return item

    @app.get("/api/files/{file_id}")
    def file_details(file_id: int):
        return indexed_file(file_id)

    def action_path(file_id):
        item = indexed_file(file_id)
        path = Path(item["path"])
        resolved = path.resolve()
        if (not path.is_file() or resolved != path or excluded(resolved)
                or not any(within(resolved, Path(root["path"])) for root in store.roots())):
            raise HTTPException(400, "파일이 이동되었거나 접근 가능한 검색 범위 밖에 있습니다.")
        return resolved

    @app.post("/api/files/{file_id}/reveal")
    def reveal(file_id: int):
        path = action_path(file_id)
        if os.name == "nt":
            subprocess.Popen(["explorer.exe", "/select,", str(path)])
        elif sys.platform == "darwin":
            subprocess.Popen(["open", "-R", str(path)])
        else:
            subprocess.Popen(["xdg-open", str(path.parent)])
        return {"ok": True}

    @app.post("/api/files/{file_id}/open")
    def open_file(file_id: int):
        path = action_path(file_id)
        safe_documents = {".pdf", ".docx", ".xlsx", ".pptx", ".odt", ".ods", ".odp", ".hwp", ".hwpx",
                          ".txt", ".md", ".csv", ".png", ".jpg", ".jpeg", ".gif", ".webp",
                          ".mp3", ".wav", ".mp4", ".mkv", ".bmp", ".tif", ".tiff"}
        if path.suffix.lower() not in safe_documents:
            raise HTTPException(400, "이 형식은 폴더에서 확인하세요. 실행 파일과 스크립트는 앱에서 실행하지 않습니다.")
        if os.name == "nt":
            os.startfile(str(path))
        else:
            subprocess.Popen(["open" if sys.platform == "darwin" else "xdg-open", str(path)])
        return {"ok": True}

    app.mount("/static", StaticFiles(directory=static), name="static")
    return app

