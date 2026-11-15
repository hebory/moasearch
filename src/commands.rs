//! Typed command boundary shared by the desktop view and application service.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use anyhow::Result;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub folder: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListFilesInput {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub folder: Option<PathBuf>,
    #[serde(default)]
    pub cursor: Option<ListCursor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListCursor {
    pub name: String,
    pub path: PathBuf,
    pub id: i64,
    pub revision: i64,
}

fn default_list_limit() -> usize {
    200
}

fn default_limit() -> usize {
    30
}
fn default_mode() -> String {
    "hybrid".into()
}

impl ListFilesInput {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=500).contains(&self.limit),
            "파일 목록 개수는 1~500개여야 합니다."
        );
        if let Some(extension) = self.extension.as_deref() {
            let normalized = extension.trim().trim_start_matches('.');
            anyhow::ensure!(
                extension.trim().is_empty()
                    || (!normalized.is_empty()
                        && normalized.len() <= 30
                        && normalized.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                "확장자 형식이 올바르지 않습니다."
            );
        }
        if let Some(folder) = &self.folder {
            let path = folder.to_string_lossy();
            anyhow::ensure!(
                !path.trim().is_empty() && path.chars().count() <= 32767 && !path.contains('\0'),
                "폴더 경로가 올바르지 않습니다."
            );
        }
        if let Some(cursor) = &self.cursor {
            let path = cursor.path.to_string_lossy();
            anyhow::ensure!(
                !cursor.name.is_empty()
                    && cursor.name.chars().count() <= 255
                    && !path.is_empty()
                    && path.chars().count() <= 32767
                    && !path.contains('\0')
                    && cursor.id > 0
                    && cursor.revision >= 0,
                "파일 목록 cursor가 올바르지 않습니다."
            );
        }
        Ok(())
    }
}

impl SearchInput {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.query.trim().is_empty() && self.query.chars().count() <= 2000,
            "검색어는 1~2000자여야 합니다."
        );
        anyhow::ensure!(
            (1..=100).contains(&self.limit),
            "검색 결과 수는 1~100개여야 합니다."
        );
        anyhow::ensure!(
            ["hybrid", "keyword", "instant"].contains(&self.mode.as_str()),
            "알 수 없는 검색 방식입니다."
        );
        anyhow::ensure!(
            self.extension.as_ref().is_none_or(|s| s.len() <= 30),
            "확장자가 너무 깁니다."
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    Status {},
    Scope { path: PathBuf },
    AddRoot { path: PathBuf },
    RemoveRoot { id: i64 },
    StartIndex {},
    CancelIndex {},
    UpdateSettings { settings: Value },
    Search(SearchInput),
    ListFiles(ListFilesInput),
    Answer(SearchInput),
    File { id: i64 },
    OpenFile { id: i64 },
    RevealFile { id: i64 },
    DownloadModel { kind: String },
    Quit {},
}
