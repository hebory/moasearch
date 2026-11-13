//! Bundled resources. WebView2 resolves this virtual origin without a network listener.
use anyhow::{Context, Result};
use std::path::Path;

pub const SCHEME: &str = "moasearch";
pub const ORIGIN: &str = "https://moasearch.app";
pub const HOME: &str = "moasearch://app/";
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub fn is_app_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    ((url.origin().ascii_serialization() == ORIGIN)
        || (url.scheme() == SCHEME && url.host_str() == Some("app")))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

pub fn is_document(value: &str) -> bool {
    is_app_url(value)
        && reqwest::Url::parse(value)
            .is_ok_and(|url| matches!(url.path(), "/" | "/index.html") && url.query().is_none())
}

pub fn window_url(folder: Option<&Path>) -> Result<String> {
    let mut url = reqwest::Url::parse(HOME)?;
    if let Some(folder) = folder {
        let folder = folder
            .to_str()
            .context("폴더 경로를 UTF-8로 읽을 수 없습니다.")?;
        let encoded =
            reqwest::Url::parse_with_params("https://unused.invalid", [("folder", folder)])?;
        url.set_fragment(encoded.query());
    }
    Ok(url.into())
}

pub fn resource(path: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match path {
        "/" | "/index.html" => (
            "text/html; charset=utf-8",
            include_bytes!("../static/index.html"),
        ),
        "/static/app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../static/app.js"),
        ),
        "/static/bridge.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../static/bridge.js"),
        ),
        "/static/style.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../static/style.css"),
        ),
        "/static/icon.png" => ("image/png", include_bytes!("../../icon.png")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_bundled_app_documents_can_send_commands() {
        assert!(is_document(&format!("{ORIGIN}/#folder=test")));
        assert!(is_document(HOME));
        for url in [
            "https://moasearch.app.evil/",
            "https://evil@moasearch.app/",
            "http://moasearch.app/",
            "https://moasearch.app:8888/",
            "file:///index.html",
            "https://moasearch.app/static/app.js",
            "https://moasearch.app/?redirect=evil",
        ] {
            assert!(!is_document(url), "{url}");
        }
        assert!(resource("/static/bridge.js").is_some());
        for path in [
            "/api/status",
            "/../Cargo.toml",
            "/static/../../index.sqlite3",
        ] {
            assert!(resource(path).is_none());
        }
    }
    #[test]
    fn folder_fragment_preserves_unicode_and_special_characters() -> Result<()> {
        for folder in [
            r"D:\",
            r"D:\한글 폴더\100%_계약 + # &=",
            r"\\server\share\문서",
        ] {
            let url = reqwest::Url::parse(&window_url(Some(Path::new(folder)))?)?;
            assert_eq!(url.query(), None);
            let params = reqwest::Url::parse(&format!(
                "https://unused.invalid/?{}",
                url.fragment().unwrap()
            ))?;
            let pairs: Vec<_> = params.query_pairs().collect();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0], ("folder".into(), folder.into()));
        }
        Ok(())
    }
}
