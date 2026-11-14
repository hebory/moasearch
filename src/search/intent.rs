//! Explicit file-kind query handling kept separate from semantic ranking.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, params_from_iter};
use serde_json::Value;

use super::{folder_contains_path, scope_like};

const SOURCE_CODE_EXTENSIONS: &[&str] = &[
    ".rs", ".py", ".js", ".jsx", ".ts", ".tsx", ".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp",
    ".hxx", ".java", ".cs", ".go", ".sh", ".bash", ".zsh", ".ps1", ".psm1", ".psd1", ".bat",
    ".cmd", ".html", ".htm", ".css", ".scss", ".sass", ".less", ".vue", ".svelte", ".php", ".rb",
    ".swift", ".kt", ".kts", ".scala", ".lua", ".sql", ".r", ".dart", ".ex", ".exs", ".erl",
    ".hrl", ".hs", ".lhs", ".clj", ".cljs", ".cljc", ".fs", ".fsx", ".vb", ".m", ".mm", ".pl",
    ".pm", ".groovy", ".gradle", ".mk",
];

/// Recognize only the exact supported source-code aliases after case and
/// whitespace normalization. Longer natural-language queries stay semantic.
pub(super) fn is_source_code_query(query: &str) -> bool {
    let normalized: String = query
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "소스코드" | "sourcecode" | "source-code"
    )
}

/// Return indexed source-code files, preserving the regular extension and
/// folder constraints used by search. The first stored passage is included as
/// the result preview when one exists.
pub(super) fn source_code_files(
    db: &Connection,
    extension: Option<&str>,
    folder: Option<&Path>,
) -> Result<Vec<Value>> {
    let mut sql = format!(
        "SELECT f.*,
                COALESCE((SELECT c.text FROM chunks c WHERE c.file_id=f.id ORDER BY c.id LIMIT 1), '') AS text,
                COALESCE((SELECT c.location FROM chunks c WHERE c.file_id=f.id ORDER BY c.id LIMIT 1), '') AS location
         FROM files f
         WHERE f.extension IN ({})",
        std::iter::repeat("?")
            .take(SOURCE_CODE_EXTENSIONS.len())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut values: Vec<String> = SOURCE_CODE_EXTENSIONS
        .iter()
        .map(|extension| (*extension).to_string())
        .collect();

    if let Some(extension) = extension.filter(|value| !value.trim().is_empty()) {
        sql.push_str(" AND f.extension=?");
        values.push(format!(
            ".{}",
            extension.trim().trim_start_matches('.').to_lowercase()
        ));
    }
    if let Some(folder) = folder {
        sql.push_str(" AND f.path LIKE ? ESCAPE '\\' COLLATE NOCASE");
        values.push(scope_like(folder));
    }
    sql.push_str(" ORDER BY f.modified DESC,f.id DESC LIMIT 300");

    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| {
        let mut file = crate::store::row_json(row)?;
        file["text"] = row.get::<_, String>(11)?.into();
        file["location"] = row.get::<_, String>(12)?.into();
        Ok(file)
    })?;
    let mut files = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    files.retain(|file| {
        folder.is_none_or(|folder| {
            file["path"]
                .as_str()
                .is_some_and(|path| folder_contains_path(folder, Path::new(path)))
        })
    });
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE files(
                id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL, path TEXT NOT NULL,
                name TEXT NOT NULL, extension TEXT NOT NULL, size INTEGER NOT NULL,
                modified INTEGER NOT NULL, status TEXT NOT NULL, error TEXT,
                model_id TEXT, reader_id TEXT
             );
             CREATE TABLE chunks(
                id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, text TEXT NOT NULL,
                location TEXT NOT NULL
             );",
        )
        .unwrap();
        db
    }

    fn insert_file(db: &Connection, id: i64, path: &Path, modified: i64) {
        let path_text = path.to_string_lossy();
        let name = path.file_name().unwrap().to_string_lossy();
        let extension = path.extension().unwrap().to_string_lossy().to_lowercase();
        db.execute(
            "INSERT INTO files(id,root_id,path,name,extension,size,modified,status,error,model_id,reader_id)
             VALUES(?1,1,?2,?3,?4,10,?5,'content',NULL,NULL,'test')",
            params![id, path_text, name, format!(".{extension}"), modified],
        )
        .unwrap();
        db.execute(
            "INSERT INTO chunks(id,file_id,text,location) VALUES(?1,?1,'첫 번째 내용','본문')",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn source_code_alias_requires_an_exact_normalized_phrase() {
        for alias in ["소스코드", " 소스 코드 ", "source code", "SOURCE   CODE"] {
            assert!(is_source_code_query(alias), "alias: {alias}");
        }
        for other in ["소스코드 파일", "내 소스 코드", "source code files", "code"] {
            assert!(!is_source_code_query(other), "not an exact alias: {other}");
        }
    }

    #[test]
    fn source_code_file_types_and_extension_filter_are_respected() {
        let db = database();
        let root = Path::new("C:/fixture");
        for (id, name, modified) in [
            (1, "a.rs", 1),
            (2, "b.py", 3),
            (3, "c.vue", 2),
            (4, "notes.txt", 9),
            (5, "report.hwp", 8),
            (6, "manual.pdf", 7),
            (7, "data.json", 6),
        ] {
            insert_file(&db, id, &root.join(name), modified);
        }

        let files = source_code_files(&db, None, None).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["b.py", "c.vue", "a.rs"]);
        assert_eq!(files[0]["text"], "첫 번째 내용");
        assert_eq!(files[0]["location"], "본문");

        let filtered = source_code_files(&db, Some("PY"), None).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["name"], "b.py");
        assert!(
            source_code_files(&db, Some("pdf"), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn scoped_source_code_search_excludes_sibling_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        let scope = temp.path().join("code");
        let nested = scope.join("nested");
        let sibling = temp.path().join("code2");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let db = database();
        insert_file(&db, 1, &scope.join("main.rs"), 1);
        insert_file(&db, 2, &nested.join("module.py"), 2);
        insert_file(&db, 3, &sibling.join("excluded.rs"), 3);

        let files = source_code_files(&db, None, Some(&scope)).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["module.py", "main.rs"]);
    }
}
