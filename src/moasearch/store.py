"""SQLite index with bounded batch writes and transactional search revisions."""

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
            self._prepare_search_metadata(db)

    @staticmethod
    def _prepare_search_metadata(db):
        paths_exist = db.execute("SELECT 1 FROM sqlite_master WHERE name='files_fts'").fetchone()
        schema = """
            CREATE INDEX IF NOT EXISTS chunks_model ON chunks(model_id,file_id) WHERE vector IS NOT NULL;
            CREATE INDEX IF NOT EXISTS chunks_file_text ON chunks(file_id,text,location);
            CREATE INDEX IF NOT EXISTS files_modified ON files(modified DESC);
            CREATE INDEX IF NOT EXISTS files_extension ON files(extension,modified DESC);
            CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
                path, content='files', content_rowid='id', tokenize='trigram');
            CREATE TABLE IF NOT EXISTS index_meta (
                id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL,
                files INTEGER NOT NULL, chunks INTEGER NOT NULL, vectors INTEGER NOT NULL);
            INSERT INTO index_meta SELECT 1,0,
                (SELECT count(*) FROM files), (SELECT count(*) FROM chunks),
                (SELECT count(*) FROM chunks WHERE vector IS NOT NULL)
                WHERE NOT EXISTS (SELECT 1 FROM index_meta WHERE id=1);
        """
        # Keep counters and invalidation in the same transaction as the actual
        # rows, including writes from a second app/CLI process and FK cascades.
        schema += """
            CREATE TRIGGER IF NOT EXISTS files_search_ai AFTER INSERT ON files BEGIN
                INSERT INTO files_fts(rowid,path) VALUES(new.id,new.path);
                UPDATE index_meta SET revision=revision+1,files=files+1 WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS files_search_ad AFTER DELETE ON files BEGIN
                INSERT INTO files_fts(files_fts,rowid,path) VALUES('delete',old.id,old.path);
                UPDATE index_meta SET revision=revision+1,files=files-1 WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS files_search_au AFTER UPDATE ON files BEGIN
                UPDATE index_meta SET revision=revision+1 WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS files_path_au AFTER UPDATE OF path ON files
            WHEN old.path != new.path BEGIN
                INSERT INTO files_fts(files_fts,rowid,path) VALUES('delete',old.id,old.path);
                INSERT INTO files_fts(rowid,path) VALUES(new.id,new.path);
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_meta_ai AFTER INSERT ON chunks BEGIN
                UPDATE index_meta SET revision=revision+1,chunks=chunks+1,
                    vectors=vectors+(new.vector IS NOT NULL) WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_meta_ad AFTER DELETE ON chunks BEGIN
                UPDATE index_meta SET revision=revision+1,chunks=chunks-1,
                    vectors=vectors-(old.vector IS NOT NULL) WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_meta_au AFTER UPDATE ON chunks BEGIN
                UPDATE index_meta SET revision=revision+1,
                    vectors=vectors+(new.vector IS NOT NULL)-(old.vector IS NOT NULL) WHERE id=1;
            END;
        """
        if not paths_exist:
            schema += "INSERT INTO files_fts(files_fts) VALUES('rebuild');\n"
        # Migration itself is atomic: another indexing process cannot change
        # rows between the initial counts and installation of the triggers.
        db.executescript("BEGIN IMMEDIATE;\n" + schema + "COMMIT;")

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
        return {key: value for key, value in self.index_state().items() if key != "revision"}

    def index_state(self, db=None):
        if db is not None:
            return dict(db.execute("SELECT revision,files,chunks,vectors FROM index_meta WHERE id=1").fetchone())
        with self.connect() as db:
            return self.index_state(db)

    def file_by_path(self, path, db=None):
        if db is not None:
            row = db.execute("SELECT * FROM files WHERE path=?", (str(path),)).fetchone()
            return dict(row) if row else None
        with self.connect() as db:
            return self.file_by_path(path, db)

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
            self._replace_file(db, root_id, path, stat, passages, vectors, model_id, error, reader_id)

    def replace_files(self, records, db=None):
        """Atomically publish completed files; never hold a transaction during OCR."""
        if db is None:
            with self.connect() as connection:
                self.replace_files(records, connection)
            return
        with db:
            for record in records:
                self._replace_file(db, **record)

    @staticmethod
    def _replace_file(db, root_id, path, stat, passages, vectors=None, model_id=None, error=None,
                      reader_id="legacy-v1"):
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

    def prune(self, root_id, seen, db=None):
        if db is None:
            with self.connect() as connection:
                self.prune(root_id, seen, connection)
            return
        with db:
            missing = [(r["id"],) for r in db.execute("SELECT id,path FROM files WHERE root_id=?", (root_id,))
                       if r["path"] not in seen]
            db.executemany("DELETE FROM files WHERE id=?", missing)

