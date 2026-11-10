"""Local application configuration and conservative scan defaults."""

import os
from pathlib import Path

DEFAULT_SETTINGS = {
    "embedding_model": "", "chat_model": "", "n_gpu_layers": 0,
    "context_size": 4096, "scan_interval": 300, "max_file_mb": 32,
}
EXCLUDED_DIRS = {
    ".git", ".svn", ".hg", ".ssh", ".aws", ".azure", ".gnupg", ".codex",
    "node_modules", ".venv", "venv", "__pycache__", "$recycle.bin",
    "system volume information", "windows", "program files", "program files (x86)",
    "programdata", "appdata", ".cache", "models",
}
SECRET_NAMES = {".env", "credentials", "credentials.json", "id_rsa", "id_ed25519", "ntuser.dat"}
SECRET_SUFFIXES = {".pem", ".key", ".pfx", ".p12", ".kdbx", ".gguf"}


def data_directory() -> Path:
    base = Path(os.environ.get("LOCALAPPDATA", Path.home() / ".local" / "share"))
    return Path(os.environ.get("MOASEARCH_DATA_DIR", base / "MoaSearch"))


def excluded(path: Path) -> bool:
    return (any(p.lower() in EXCLUDED_DIRS for p in path.parts)
            or path.name.lower() in SECRET_NAMES
            or path.name.lower().startswith(".env.")
            or path.suffix.lower() in SECRET_SUFFIXES)


def within(path: Path, parent: Path) -> bool:
    return path == parent or parent in path.parents

