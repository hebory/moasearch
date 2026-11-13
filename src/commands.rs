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

fn default_limit() -> usize {
    30
}
fn default_mode() -> String {
    "hybrid".into()
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
            ["hybrid", "keyword"].contains(&self.mode.as_str()),
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
    Answer(SearchInput),
    File { id: i64 },
    OpenFile { id: i64 },
    RevealFile { id: i64 },
    DownloadModel { kind: String },
    Quit {},
}
