"""Local launcher, diagnostics, indexing and model provisioning."""

import argparse
import json
from pathlib import Path
import sys
import threading
import time
import webbrowser

from .config import data_directory
from .store import Store


def emit(value):
    print(json.dumps(value, ensure_ascii=False, indent=2))


def main(argv=None):
    parser = argparse.ArgumentParser(description="모아검색 — 내장 로컬 AI 파일 검색")
    parser.add_argument("--data-dir", type=Path, default=data_directory(), help="색인·설정·모델 저장 폴더")
    commands = parser.add_subparsers(dest="command")
    serve = commands.add_parser("serve", help="로컬 검색 화면 실행")
    serve.add_argument("--port", type=int, default=8765)
    serve.add_argument("--no-browser", action="store_true")
    commands.add_parser("doctor", help="내장 엔진과 모델 준비 상태 확인")
    index = commands.add_parser("index", help="폴더를 추가하고 증분 색인")
    index.add_argument("paths", nargs="*")
    search = commands.add_parser("search", help="명령줄에서 검색")
    search.add_argument("query")
    search.add_argument("--keyword", action="store_true")
    models = commands.add_parser("models", help="내장 모델 관리")
    model_cmd = models.add_subparsers(dest="model_command", required=True)
    model_cmd.add_parser("list", help="기본 모델 목록")
    download = model_cmd.add_parser("download", help="내장 GGUF 또는 이미지 OCR 모델 다운로드 및 설정")
    download.add_argument("kind", choices=["embedding", "chat", "ocr"])
    args = parser.parse_args(argv)
    try:
        if args.command in (None, "serve"):
            import uvicorn
            from .app import create_app
            port = getattr(args, "port", 8765)
            if not 1024 <= port <= 65535:
                parser.error("포트는 1024~65535 범위여야 합니다.")
            app = create_app(args.data_dir)
            if not getattr(args, "no_browser", False):
                timer = threading.Timer(1.2, webbrowser.open, args=[f"http://127.0.0.1:{port}"])
                timer.daemon = True
                timer.start()
            uvicorn.run(app, host="127.0.0.1", port=port, access_log=False)
            return 0
        store = Store(args.data_dir)
        if args.command == "models":
            from .models import catalog, download_model
            if args.model_command == "list":
                emit(catalog())
            else:
                last = [0]
                def progress(received, total):
                    if received - last[0] >= 32 * 1024 * 1024 or received == total:
                        print(f"다운로드 {received // 1024 // 1024} / {total // 1024 // 1024} MB", flush=True)
                        last[0] = received
                model = download_model(args.kind, store.data_dir / "models", progress)
                if args.kind != "ocr":
                    store.update_settings({f"{args.kind}_model": str(model)})
                print(f"모델 준비 완료: {model}")
            return 0
        from .engine import EmbeddedEngine
        engine = EmbeddedEngine(store)
        try:
            if args.command == "doctor":
                from .ocr import status as ocr_status
                emit({"data_dir": str(store.data_dir), "engine": engine.status(),
                      "ocr": ocr_status(store.data_dir / "models"),
                      "settings": store.settings(), "counts": store.counts()})
            elif args.command == "search":
                from .search import Search
                emit(Search(store, engine).search(args.query, mode="keyword" if args.keyword else "hybrid"))
            elif args.command == "index":
                from .indexer import Indexer
                existing = {root["path"] for root in store.roots()}
                for path in args.paths:
                    if str(Path(path).expanduser().resolve()) not in existing:
                        store.add_root(path)
                indexer = Indexer(store, engine)
                indexer.start()
                try:
                    while indexer.status()["running"]:
                        time.sleep(.2)
                except KeyboardInterrupt:
                    indexer.cancel()
                    while indexer.status()["running"]:
                        time.sleep(.2)
                emit(indexer.status())
                return 1 if indexer.status()["errors"] else 0
        finally:
            engine.close()
        return 0
    except (ValueError, RuntimeError, OSError) as exc:
        print(f"오류: {exc}", file=sys.stderr)
        return 1

