//! Local-only defaults and shared filesystem boundaries.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub fn defaults() -> Value {
    json!({"embedding_model":"", "chat_model":"", "n_gpu_layers":0,
        "context_size":4096, "scan_interval":300, "max_file_mb":32})
}

pub fn data_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("MOASEARCH_DATA_DIR") {
        return path.into();
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/share")
        });
    base.join("MoaSearch")
}

pub fn canonical(path: &Path) -> Result<PathBuf> {
    let result = path
        .canonicalize()
        .with_context(|| format!("경로에 접근할 수 없습니다: {}", path.display()))?;
    // Match existing Python DB paths and Windows shell/WinRT path syntax.
    #[cfg(windows)]
    {
        let value = result.to_string_lossy();
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{unc}")));
        }
        if let Some(normal) = value.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(normal));
        }
    }
    Ok(result)
}

pub fn within(path: &Path, parent: &Path) -> bool {
    #[cfg(windows)]
    {
        Path::new(&path.to_string_lossy().to_lowercase())
            .starts_with(parent.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.starts_with(parent)
    }
}

pub fn excluded(path: &Path) -> bool {
    const DIRS: &[&str] = &[
        ".git",
        ".svn",
        ".hg",
        ".ssh",
        ".aws",
        ".azure",
        ".gnupg",
        ".codex",
        "node_modules",
        ".venv",
        "venv",
        "__pycache__",
        "$recycle.bin",
        "system volume information",
        "windows",
        "program files",
        "program files (x86)",
        "programdata",
        "appdata",
        ".cache",
        "models",
        "target",
    ];
    if path
        .components()
        .any(|c| DIRS.contains(&c.as_os_str().to_string_lossy().to_lowercase().as_str()))
    {
        return true;
    }
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if [
        ".env",
        "credentials",
        "credentials.json",
        "id_rsa",
        "id_ed25519",
        "ntuser.dat",
    ]
    .contains(&name.as_str())
        || name.starts_with(".env.")
    {
        return true;
    }
    ["pem", "key", "pfx", "p12", "kdbx", "gguf"].contains(
        &path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .as_str(),
    )
}

pub fn safe_metadata(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions, symlinks, offline and recall-on-access cloud placeholders.
        if metadata.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return false;
        }
    }
    true
}

pub fn modified_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|t| t.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundaries_and_secrets() {
        assert!(excluded(Path::new("a/.ssh/id_rsa")));
        assert!(excluded(Path::new("a/target/debug/app.exe")));
        assert!(excluded(Path::new("a/.env.local")));
        assert!(!excluded(Path::new("a/회의.txt")));
        assert!(!within(Path::new("a/root-other/x"), Path::new("a/root")));
        assert!(within(Path::new("a/root/문서.txt"), Path::new("a/root")));
    }
}
