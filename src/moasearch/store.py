"""SQLite index. Each operation owns its connection; source files are never written."""

from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3

from .config import DEFAULT_SETTINGS, excluded, within


class Store:
    def __init__(self, data_dir: Path):
        self.data_dir = data_dir.resolve()
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.path = self.data_dir / "index.sqlite3"
        with self.connect() as db:
            db.executescript("""
                PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS roots (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL);
                CREATE TABLE IF NOT EXISTS files (
                    id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
                    path TEXT UNIQUE NOT NULL, name TEXT NOT NULL, extension TEXT NOT NULL,
                    size INTEGER NOT NULL, modified INTEGER NOT NULL, status TEXT NOT NULL,
                    error TEXT, model_id TEXT, reader_id TEXT NOT NULL DEFAULT 'legacy-v1');
                CREATE INDEX IF NOT EXISTS files_root ON files(root_id);
                CREATE TABLE IF NOT EXISTS chunks (
                    id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    text TEXT NOT NULL, location TEXT NOT NULL, vector BLOB, model_id TEXT);
                CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);
                CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                    text, content='chunks', content_rowid='id', tokenize='trigram');
                CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
                    INSERT INTO chunks_fts(rowid,text) VALUES(new.id,new.text);
                END;
                CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
                    INSERT INTO chunks_fts(chunks_fts,rowid,text) VALUES('delete',old.id,old.text);
                END;
            """)
            # Existing user indexes stay intact and acquire a version for the
            # reader that produced their current text. New readers can then
            # selectively refresh only formats whose extraction changed.
            file_columns = {row["name"] for row in db.execute("PRAGMA table_info(files)")}
            if "reader_id" not in file_columns:
                db.execute("ALTER TABLE files ADD COLUMN reader_id TEXT NOT NULL DEFAULT 'legacy-v1'")

    @contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=30)
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA foreign_keys=ON")
        try:
            with db:
                yield db
        finally:
            db.close()

    def settings(self):
        with self.connect() as db:
            return DEFAULT_SETTINGS | {r["key"]: json.loads(r["value"]) for r in db.execute("SELECT * FROM settings")}

    def update_settings(self, settings):
        unknown = settings.keys() - DEFAULT_SETTINGS.keys()
        if unknown:
            raise ValueError("알 수 없는 설정입니다: " + ", ".join(unknown))
        for key in ("embedding_model", "chat_model"):
            if key in settings and settings[key]:
                path = Path(settings[key]).expanduser().resolve()
                if path.suffix.lower() != ".gguf" or not path.is_file():
                    raise ValueError("존재하는 GGUF 모델 파일을 지정하세요.")
                settings[key] = str(path)
        bounds = {"n_gpu_layers": (-1, 200), "context_size": (512, 32768),
                  "scan_interval": (0, 86400), "max_file_mb": (1, 512)}
        for key, (low, high) in bounds.items():
            if key in settings and (type(settings[key]) is not int or not low <= settings[key] <= high):
                raise ValueError(f"{key}: {low}~{high} 범위의 정수가 필요합니다.")
        with self.connect() as db:
            db.executemany("INSERT OR REPLACE INTO settings VALUES (?,?)",
                           [(k, json.dumps(v, ensure_ascii=False)) for k, v in settings.items()])

    def roots(self):
        with self.connect() as db:
            return [dict(row) for row in db.execute("SELECT * FROM roots ORDER BY id")]

    def add_root(self, raw_path):
        path = Path(raw_path).expanduser().resolve()
        if not path.is_dir():
            raise ValueError("접근 가능한 폴더 경로를 입력하세요.")
        if excluded(path) or within(path, self.data_dir):
            raise ValueError("기본 제외 경로 또는 앱 데이터 폴더는 등록할 수 없습니다.")
        with self.connect() as db:
            for row in db.execute("SELECT path FROM roots"):
                other = Path(row["path"])
                if within(path, other) or within(other, path):
                    raise ValueError("이미 등록된 폴더와 범위가 겹칩니다.")
            cur = db.execute("INSERT INTO roots(path) VALUES (?)", (str(path),))
            return {"id": cur.lastrowid, "path": str(path)}

    def remove_root(self, root_id):
        with self.connect() as db:
            db.execute("DELETE FROM roots WHERE id=?", (root_id,))

    def counts(self):
        with self.connect() as db:
            return {"files": db.execute("SELECT count(*) FROM files").fetchone()[0],
                    "chunks": db.execute("SELECT count(*) FROM chunks").fetchone()[0],
                    "vectors": db.execute("SELECT count(*) FROM chunks WHERE vector IS NOT NULL").fetchone()[0]}

    def file_by_path(self, path):
        with self.connect() as db:
            row = db.execute("SELECT * FROM files WHERE path=?", (str(path),)).fetchone()
            return dict(row) if row else None

    def file(self, file_id):
        with self.connect() as db:
            row = db.execute("SELECT * FROM files WHERE id=?", (file_id,)).fetchone()
            if not row:
                return None
            return dict(row) | {"passages": [dict(r) for r in db.execute(
                "SELECT text,location FROM chunks WHERE file_id=? ORDER BY id LIMIT 100", (file_id,))]}

    def replace_file(self, root_id, path, stat, passages, vectors=None, model_id=None, error=None,
                     reader_id="legacy-v1"):
        with self.connect() as db:
            db.execute("""INSERT INTO files(root_id,path,name,extension,size,modified,status,error,model_id,reader_id)
                VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(path) DO UPDATE SET
                size=excluded.size,modified=excluded.modified,status=excluded.status,
                error=excluded.error,model_id=excluded.model_id,reader_id=excluded.reader_id""",
                (root_id, str(path), path.name, path.suffix.lower(), stat.st_size, stat.st_mtime_ns,
                 "error" if error else "content" if passages else "metadata", error, model_id, reader_id))
            file_id = db.execute("SELECT id FROM files WHERE path=?", (str(path),)).fetchone()[0]
            db.execute("DELETE FROM chunks WHERE file_id=?", (file_id,))
            db.executemany("INSERT INTO chunks(file_id,text,location,vector,model_id) VALUES (?,?,?,?,?)",
                           [(file_id, p.text, p.location, vectors[i] if vectors else None,
                             model_id if vectors else None) for i, p in enumerate(passages)])

    def prune(self, root_id, seen):
        with self.connect() as db:
            missing = [(r["id"],) for r in db.execute("SELECT id,path FROM files WHERE root_id=?", (root_id,))
                       if r["path"] not in seen]
            db.executemany("DELETE FROM files WHERE id=?", missing)

