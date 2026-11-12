//! Keyword and exact semantic search over the transactional SQLite index.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use regex::Regex;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};

use crate::engine::Embedder;
use crate::store::{Store, decode_vector};

const DEFAULT_VECTOR_CACHE_BYTES: usize = 128 * 1024 * 1024;
const QUERY_CACHE_SIZE: usize = 64;

#[derive(Debug)]
struct VectorRow {
    chunk_id: i64,
    file_id: i64,
    extension_id: u32,
    path_id: usize,
}

#[derive(Debug)]
struct VectorSnapshot {
    model_id: String,
    dimension: usize,
    revision: i64,
    rows: Vec<VectorRow>,
    vectors: Vec<f32>,
    extensions: Vec<String>,
    paths: Vec<PathBuf>,
}

impl VectorSnapshot {
    fn candidates(
        &self,
        query: &[f32],
        extension: Option<&str>,
        folder: Option<&Path>,
        file_limit: usize,
    ) -> Vec<(f32, i64)> {
        if self.dimension != query.len() {
            return Vec::new();
        }
        let path_allowed = folder.map(|folder| {
            self.paths
                .iter()
                .map(|path| folder_contains_path(folder, path))
                .collect::<Vec<_>>()
        });
        let mut best_by_file: HashMap<i64, (f32, i64)> = HashMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            if extension
                .is_some_and(|extension| self.extensions[row.extension_id as usize] != extension)
                || path_allowed
                    .as_ref()
                    .is_some_and(|allowed| !allowed[row.path_id])
            {
                continue;
            }
            let start = index * self.dimension;
            let score = dot(&self.vectors[start..start + self.dimension], query);
            best_by_file
                .entry(row.file_id)
                .and_modify(|best| {
                    if score > best.0 || (score == best.0 && row.chunk_id < best.1) {
                        *best = (score, row.chunk_id);
                    }
                })
                .or_insert((score, row.chunk_id));
        }
        let mut candidates: Vec<_> = best_by_file.into_values().collect();
        candidates.sort_by(compare_candidate_desc);
        candidates.truncate(file_limit);
        candidates
    }
}

#[derive(Default)]
struct VectorCache {
    snapshot: Option<Arc<VectorSnapshot>>,
}

impl VectorCache {
    fn get_or_build(
        &mut self,
        db: &Connection,
        model_id: &str,
        dimension: usize,
        revision: i64,
        byte_limit: usize,
    ) -> Result<Option<Arc<VectorSnapshot>>> {
        if self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.model_id == model_id
                && snapshot.dimension == dimension
                && snapshot.revision == revision
        }) {
            return Ok(self.snapshot.clone());
        }
        self.snapshot = None;
        if byte_limit == 0 || dimension == 0 {
            return Ok(None);
        }
        let count: i64 = db.query_row(
            "SELECT count(*) FROM chunks WHERE model_id=?1 AND vector IS NOT NULL",
            [model_id],
            |row| row.get(0),
        )?;
        let path_bytes: i64 = db.query_row(
            "SELECT COALESCE(sum(length(CAST(path AS BLOB)) + 64), 0) FROM files
             WHERE id IN (SELECT DISTINCT file_id FROM chunks
                          WHERE model_id=?1 AND vector IS NOT NULL)",
            [model_id],
            |row| row.get(0),
        )?;
        let row_bytes = dimension
            .saturating_mul(std::mem::size_of::<f32>())
            .saturating_add(std::mem::size_of::<VectorRow>())
            .saturating_add(32);
        let estimated = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .saturating_mul(row_bytes)
            .saturating_add(usize::try_from(path_bytes).unwrap_or(usize::MAX));
        if estimated > byte_limit {
            return Ok(None);
        }
        let count = usize::try_from(count).unwrap_or(0);
        let mut rows = Vec::new();
        let mut vectors = Vec::new();
        if rows.try_reserve_exact(count).is_err()
            || vectors
                .try_reserve_exact(count.saturating_mul(dimension))
                .is_err()
        {
            return Ok(None);
        }
        let mut extension_ids = HashMap::<String, u32>::new();
        let mut extensions = Vec::new();
        let mut path_ids = HashMap::<i64, usize>::new();
        let mut paths = Vec::new();
        let mut stmt = db.prepare(
            "SELECT c.id,c.file_id,f.extension,c.vector,f.path FROM chunks c
             JOIN files f ON f.id=c.file_id
             WHERE c.model_id=?1 AND c.vector IS NOT NULL ORDER BY c.file_id,c.id",
        )?;
        let mapped = stmt.query_map([model_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut bytes = 0usize;
        for item in mapped {
            let (chunk_id, file_id, extension, raw, path) = item?;
            let Some(mut vector) = decode_vector(&raw) else {
                continue;
            };
            if vector.len() != dimension || vector.iter().any(|v| !v.is_finite()) {
                continue;
            }
            normalize(&mut vector);
            let path_bytes = path.len();
            let extension_id = if let Some(id) = extension_ids.get(&extension) {
                *id
            } else {
                let id = extensions.len() as u32;
                extension_ids.insert(extension.clone(), id);
                extensions.push(extension);
                id
            };
            let (path_id, is_new_path) = if let Some(id) = path_ids.get(&file_id) {
                (*id, false)
            } else {
                let id = paths.len();
                paths.push(PathBuf::from(path));
                path_ids.insert(file_id, id);
                (id, true)
            };
            bytes = bytes.saturating_add(
                vector.len() * std::mem::size_of::<f32>() + std::mem::size_of::<VectorRow>(),
            );
            if is_new_path {
                bytes = bytes.saturating_add(path_bytes + 64);
            }
            rows.push(VectorRow {
                chunk_id,
                file_id,
                extension_id,
                path_id,
            });
            vectors.extend_from_slice(&vector);
        }
        if bytes > byte_limit {
            return Ok(None);
        }
        let snapshot = Arc::new(VectorSnapshot {
            model_id: model_id.to_string(),
            dimension,
            revision,
            rows,
            vectors,
            extensions,
            paths,
        });
        self.snapshot = Some(snapshot.clone());
        Ok(Some(snapshot))
    }
}

/// Canonicalize an existing directory selected for a folder-scoped search.
pub fn canonical_folder(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.to_str().is_some_and(|value| value.trim().is_empty()) {
        return Err(anyhow!("검색 폴더를 지정하세요."));
    }
    let folder = crate::config::canonical(path)?;
    if !folder.is_dir() {
        return Err(anyhow!("검색 범위는 접근 가능한 폴더여야 합니다."));
    }
    if folder.to_str().is_none() {
        return Err(anyhow!("검색 폴더 경로는 유니코드 문자열이어야 합니다."));
    }
    Ok(folder)
}

/// Check path containment on complete path components, accounting for Windows case.
pub fn folder_contains_path(folder: &Path, candidate: &Path) -> bool {
    #[cfg(windows)]
    {
        let normalize = |path: &Path| {
            path.to_string_lossy()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        let folder = normalize(folder);
        let candidate = normalize(candidate);
        candidate == folder
            || candidate
                .strip_prefix(&folder)
                .is_some_and(|suffix| suffix.starts_with('\\'))
    }
    #[cfg(not(windows))]
    {
        candidate == folder || candidate.starts_with(folder)
    }
}

pub(crate) fn scope_like(folder: &Path) -> String {
    let mut prefix = folder.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        prefix = prefix.replace('/', "\\");
    }
    let separator = std::path::MAIN_SEPARATOR;
    while prefix.ends_with(separator) && prefix.len() > 1 {
        // Keep the separator in drive roots (for example, C:\\).
        if cfg!(windows) && prefix.len() == 3 && prefix.as_bytes()[1] == b':' {
            break;
        }
        prefix.pop();
    }
    let boundary = if prefix.ends_with(separator) {
        String::new()
    } else {
        separator.to_string()
    };
    let literal_prefix = prefix + &boundary;
    format!(
        "{}%",
        literal_prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

#[derive(Default)]
struct QueryCache {
    values: Vec<((String, String), Vec<f32>)>,
}

impl QueryCache {
    fn get(&mut self, model_id: &str, query: &str) -> Option<Vec<f32>> {
        let index = self
            .values
            .iter()
            .position(|((model, text), _)| model == model_id && text == query)?;
        let item = self.values.remove(index);
        let vector = item.1.clone();
        self.values.insert(0, item);
        Some(vector)
    }

    fn insert(&mut self, model_id: String, query: String, vector: Vec<f32>) {
        self.values
            .retain(|((model, text), _)| model != &model_id || text != &query);
        self.values.insert(0, ((model_id, query), vector));
        self.values.truncate(QUERY_CACHE_SIZE);
    }
}

pub struct Search {
    store: Arc<Store>,
    engine: Arc<dyn Embedder>,
    vectors: Mutex<VectorCache>,
    query_vectors: Mutex<QueryCache>,
    vector_cache_bytes: usize,
}

impl Search {
    pub fn new(store: Arc<Store>, engine: Arc<dyn Embedder>) -> Self {
        Self::with_cache_limit(store, engine, DEFAULT_VECTOR_CACHE_BYTES)
    }

    pub fn with_cache_limit(
        store: Arc<Store>,
        engine: Arc<dyn Embedder>,
        vector_cache_bytes: usize,
    ) -> Self {
        Self {
            store,
            engine,
            vectors: Mutex::new(VectorCache::default()),
            query_vectors: Mutex::new(QueryCache::default()),
            vector_cache_bytes,
        }
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
        extension: Option<&str>,
        mode: &str,
    ) -> Result<Value> {
        self.search_in(query, limit, extension, mode, None)
    }

    pub fn search_in(
        &self,
        query: &str,
        limit: usize,
        extension: Option<&str>,
        mode: &str,
        folder: Option<&Path>,
    ) -> Result<Value> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 2000 {
            return Err(anyhow!("검색어는 1~2,000자로 입력하세요."));
        }
        let limit = limit.clamp(1, 100);
        let folder = folder.map(canonical_folder).transpose()?;
        let folder = folder.as_deref();
        let terms = terms(query);
        let extension = extension
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!(".{}", value.trim().trim_start_matches('.').to_lowercase()));
        let mut hits = HashMap::<i64, Hit>::new();
        let mut warnings = Vec::<String>::new();
        let mut next_hit_order = 0usize;
        {
            let db = self.store.connect()?;
            keyword_search(
                &db,
                &terms,
                extension.as_deref(),
                folder,
                &mut hits,
                &mut next_hit_order,
            )?;
        }

        let mut actual_mode = "keyword";
        if mode == "hybrid" {
            if let Some(model_id) = self.engine.model_id() {
                match self.query_vector(&model_id, query) {
                    Ok(query_vector) => {
                        match self.semantic_search(
                            &model_id,
                            &query_vector,
                            extension.as_deref(),
                            folder,
                            limit,
                            &mut hits,
                            &mut next_hit_order,
                        ) {
                            Ok((has_candidates, _)) => {
                                actual_mode = "hybrid";
                                if !has_candidates {
                                    warnings.push(
                                        "현재 모델의 의미 색인이 없습니다. 폴더를 다시 색인하세요."
                                            .to_string(),
                                    );
                                }
                            }
                            Err(error) => warnings.push(format!(
                                "의미 검색을 사용할 수 없어 키워드로 검색했습니다: {error}"
                            )),
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "의미 검색을 사용할 수 없어 키워드로 검색했습니다: {error}"
                    )),
                }
            } else {
                warnings.push(
                    "내장 임베딩 모델을 준비하면 의미 검색을 사용할 수 있습니다.".to_string(),
                );
            }
        }
        let mut sorted: Vec<_> = hits.into_values().collect();
        sorted.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.first_seen.cmp(&b.first_seen))
        });
        sorted.truncate(limit);
        let output: Vec<Value> = sorted
            .into_iter()
            .map(|hit| {
                let text = hit.text.unwrap_or_default();
                let mut item = hit.file;
                item["score"] = json!(hit.score);
                item["match_type"] = json!(hit.match_type);
                item["snippet"] = json!(preview(&text, &terms, 360));
                item.as_object_mut().unwrap().remove("model_id");
                item
            })
            .collect();
        Ok(json!({"results":output,"mode":actual_mode,"warnings":warnings}))
    }

    pub fn answer(&self, query: &str, limit: usize) -> Result<Value> {
        self.answer_in(query, limit, None)
    }

    pub fn answer_in(&self, query: &str, limit: usize, folder: Option<&Path>) -> Result<Value> {
        let result = self.search_in(query, limit.clamp(1, 8), None, "hybrid", folder)?;
        let sources: Vec<Value> = result["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                item["snippet"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            })
            .cloned()
            .collect();
        let warnings = result["warnings"].clone();
        if sources.is_empty() {
            return Ok(
                json!({"answer":"답변에 근거로 사용할 문서 본문을 찾지 못했습니다.","sources":[],"warnings":warnings}),
            );
        }
        let mut context = String::new();
        for (index, source) in sources.iter().enumerate() {
            context.push_str(&format!(
                "[{}] {} ({}, {})\n{}\n\n",
                index + 1,
                source["name"].as_str().unwrap_or(""),
                source["path"].as_str().unwrap_or(""),
                source["location"].as_str().unwrap_or(""),
                source["snippet"].as_str().unwrap_or(""),
            ));
        }
        let answer = self.engine.answer(query, &context)?;
        Ok(json!({"answer":answer,"sources":sources,"warnings":warnings}))
    }

    fn query_vector(&self, model_id: &str, query: &str) -> Result<Vec<f32>> {
        let mut cache = self.query_vectors.lock().unwrap();
        if let Some(vector) = cache.get(model_id, query) {
            return Ok(vector);
        }
        let mut vectors = self.engine.embed(&[query.to_string()], true)?;
        let mut vector = vectors
            .drain(..)
            .next()
            .ok_or_else(|| anyhow!("검색 임베딩이 비어 있습니다."))?;
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            return Err(anyhow!("검색 임베딩이 비어 있거나 올바르지 않습니다."));
        }
        normalize(&mut vector);
        cache.insert(model_id.to_string(), query.to_string(), vector.clone());
        Ok(vector)
    }

    fn semantic_search(
        &self,
        model_id: &str,
        query: &[f32],
        extension: Option<&str>,
        folder: Option<&Path>,
        limit: usize,
        hits: &mut HashMap<i64, Hit>,
        next_hit_order: &mut usize,
    ) -> Result<(bool, i64)> {
        let mut db = self.store.connect()?;
        let tx = db.transaction()?;
        let state = Store::index_state_on(&tx)?;
        let revision = state["revision"]
            .as_i64()
            .ok_or_else(|| anyhow!("색인 버전이 올바르지 않습니다."))?;
        let candidate_limit = (limit * 3).max(60);
        let snapshot = {
            let mut cache = self.vectors.lock().unwrap();
            cache.get_or_build(
                &tx,
                model_id,
                query.len(),
                revision,
                self.vector_cache_bytes,
            )?
        };
        let ranked = match snapshot {
            Some(snapshot) => snapshot.candidates(query, extension, folder, candidate_limit),
            None => streaming_candidates(&tx, model_id, query, extension, folder, candidate_limit)?,
        };
        let has_candidates = !ranked.is_empty();
        let placeholders = std::iter::repeat("?")
            .take(ranked.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut found = HashMap::<i64, Value>::new();
        if !ranked.is_empty() {
            let ids: Vec<i64> = ranked.iter().map(|(_, id)| *id).collect();
            let sql = format!(
                "SELECT f.*,c.id AS result_chunk_id,c.text,c.location FROM chunks c
                 JOIN files f ON f.id=c.file_id WHERE c.id IN ({placeholders})"
            );
            let mut statement = tx.prepare(&sql)?;
            let mapped = statement.query_map(params_from_iter(ids.iter()), |row| {
                let mut item = crate::store::row_json(row)?;
                item["result_chunk_id"] = json!(row.get::<_, i64>(11)?);
                item["text"] = json!(row.get::<_, String>(12)?);
                item["location"] = json!(row.get::<_, String>(13)?);
                Ok(item)
            })?;
            for row in mapped {
                let item = row?;
                found.insert(item["result_chunk_id"].as_i64().unwrap_or_default(), item);
            }
        }
        for (rank, (_, chunk_id)) in ranked.iter().enumerate() {
            if let Some(mut item) = found.remove(chunk_id) {
                item.as_object_mut().unwrap().remove("result_chunk_id");
                let text = item["text"].as_str().unwrap_or_default().to_string();
                item.as_object_mut().unwrap().remove("text");
                add_hit(hits, item, Some(text), rank + 1, "semantic", next_hit_order);
            }
        }
        tx.commit()?;
        Ok((has_candidates, revision))
    }
}

struct Hit {
    file: Value,
    text: Option<String>,
    score: f64,
    match_type: String,
    first_seen: usize,
}

fn keyword_search(
    db: &Connection,
    terms: &[String],
    extension: Option<&str>,
    folder: Option<&Path>,
    hits: &mut HashMap<i64, Hit>,
    next_hit_order: &mut usize,
) -> Result<()> {
    if terms.is_empty() {
        return Ok(());
    }
    let ext_sql = if extension.is_some() {
        " AND f.extension=?"
    } else {
        ""
    };
    let folder_sql = if folder.is_some() {
        " AND f.path LIKE ? ESCAPE '\\' COLLATE NOCASE"
    } else {
        ""
    };
    let mut path_parts = Vec::new();
    let mut path_params = Vec::<String>::new();
    let long_path_terms: Vec<_> = terms
        .iter()
        .filter(|term| term.chars().count() >= 3)
        .collect();
    let short_path_terms: Vec<_> = terms
        .iter()
        .filter(|term| term.chars().count() < 3)
        .collect();
    if !long_path_terms.is_empty() {
        let expression = fts_expression(long_path_terms.iter().map(|s| s.as_str()));
        let likes = std::iter::repeat("f.path LIKE ? ESCAPE '\\'")
            .take(long_path_terms.len())
            .collect::<Vec<_>>()
            .join(" OR ");
        path_parts.push(format!(
            "(f.id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?) AND ({likes}))"
        ));
        path_params.push(expression);
        path_params.extend(long_path_terms.iter().map(|term| like(term)));
    }
    if !short_path_terms.is_empty() {
        let likes = std::iter::repeat("f.path LIKE ? ESCAPE '\\'")
            .take(short_path_terms.len())
            .collect::<Vec<_>>()
            .join(" OR ");
        path_parts.push(format!("({likes})"));
        path_params.extend(short_path_terms.iter().map(|term| like(term)));
    }
    let path_clause = path_parts.join(" OR ");
    let path_query = format!(
        "SELECT f.*, '' AS text, '파일명·경로' AS location FROM files f WHERE ({path_clause}){ext_sql}{folder_sql} ORDER BY f.modified DESC LIMIT 300"
    );
    let path_extension = extension.map(str::to_string);
    let mut path_values: Vec<&dyn rusqlite::ToSql> = path_params.iter().map(|s| s as _).collect();
    if let Some(extension) = path_extension.as_ref() {
        path_values.push(extension);
    }
    let folder_pattern = folder.map(scope_like);
    if let Some(pattern) = folder_pattern.as_ref() {
        path_values.push(pattern);
    }
    let mut path_stmt = db.prepare(&path_query)?;
    let mut path_rows = path_stmt
        .query_map(params_from_iter(path_values), |row| {
            let mut file = crate::store::row_json(row)?;
            file["text"] = json!(row.get::<_, String>(11)?);
            file["location"] = json!(row.get::<_, String>(12)?);
            Ok(file)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    path_rows.retain(|row| {
        folder.is_none_or(|folder| {
            row["path"]
                .as_str()
                .is_some_and(|path| folder_contains_path(folder, Path::new(path)))
        })
    });
    path_rows.sort_by(|a, b| {
        name_match_count(b, terms)
            .cmp(&name_match_count(a, terms))
            .then_with(|| b["modified"].as_i64().cmp(&a["modified"].as_i64()))
    });
    for (rank, row) in path_rows.into_iter().enumerate() {
        let text = row["text"].as_str().unwrap_or_default().to_string();
        add_hit(hits, row, Some(text), rank + 1, "filename", next_hit_order);
    }

    let long_terms: Vec<_> = terms
        .iter()
        .filter(|term| term.chars().count() >= 3)
        .collect();
    let mut content_rows = Vec::new();
    if !long_terms.is_empty() {
        let expression = fts_expression(long_terms.iter().map(|s| s.as_str()));
        let sql = format!(
            "SELECT f.*,c.text,c.location FROM chunks_fts
             JOIN chunks c ON c.id=chunks_fts.rowid JOIN files f ON f.id=c.file_id
             WHERE chunks_fts MATCH ?{ext_sql}{folder_sql} ORDER BY bm25(chunks_fts) LIMIT 400"
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&expression];
        let fts_extension = extension.map(str::to_string);
        if let Some(extension) = fts_extension.as_ref() {
            values.push(extension);
        }
        if let Some(pattern) = folder_pattern.as_ref() {
            values.push(pattern);
        }
        let mut stmt = db.prepare(&sql)?;
        content_rows.extend(
            stmt.query_map(params_from_iter(values), joined_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    let short_terms: Vec<_> = terms
        .iter()
        .filter(|term| term.chars().count() < 3)
        .collect();
    if !short_terms.is_empty() {
        let clause = std::iter::repeat("c.text LIKE ? ESCAPE '\\'")
            .take(short_terms.len())
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT f.*,c.text,c.location FROM chunks c JOIN files f ON f.id=c.file_id
             WHERE ({clause}){ext_sql}{folder_sql} ORDER BY f.modified DESC LIMIT 200"
        );
        let patterns: Vec<String> = short_terms.iter().map(|term| like(term)).collect();
        let mut values: Vec<&dyn rusqlite::ToSql> = patterns.iter().map(|s| s as _).collect();
        let short_extension = extension.map(str::to_string);
        if let Some(extension) = short_extension.as_ref() {
            values.push(extension);
        }
        if let Some(pattern) = folder_pattern.as_ref() {
            values.push(pattern);
        }
        let mut stmt = db.prepare(&sql)?;
        content_rows.extend(
            stmt.query_map(params_from_iter(values), joined_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    content_rows.retain(|row| {
        folder.is_none_or(|folder| {
            row["path"]
                .as_str()
                .is_some_and(|path| folder_contains_path(folder, Path::new(path)))
        })
    });
    let mut seen = HashSet::new();
    for row in content_rows {
        let id = row["id"].as_i64().unwrap_or_default();
        if seen.insert(id) {
            let text = row["text"].as_str().unwrap_or_default().to_string();
            add_hit(hits, row, Some(text), seen.len(), "keyword", next_hit_order);
        }
    }
    Ok(())
}

fn joined_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut item = crate::store::row_json(row)?;
    item["text"] = json!(row.get::<_, String>(11)?);
    item["location"] = json!(row.get::<_, String>(12)?);
    Ok(item)
}

fn add_hit(
    hits: &mut HashMap<i64, Hit>,
    file: Value,
    text: Option<String>,
    rank: usize,
    source: &str,
    next_hit_order: &mut usize,
) {
    let id = file["id"].as_i64().unwrap_or_default();
    let contribution = 1.0 / (40.0 + rank as f64);
    if let Some(existing) = hits.get_mut(&id) {
        existing.score += contribution;
        if !existing.match_type.split('+').any(|part| part == source) {
            existing.match_type.push('+');
            existing.match_type.push_str(source);
        }
        if text.as_ref().is_some_and(|value| !value.is_empty())
            && (source == "keyword" || existing.text.as_deref().unwrap_or_default().is_empty())
        {
            existing.text = text;
            existing.file["location"] = file["location"].clone();
        }
    } else {
        hits.insert(
            id,
            Hit {
                file,
                text,
                score: contribution,
                match_type: source.to_string(),
                first_seen: {
                    let order = *next_hit_order;
                    *next_hit_order += 1;
                    order
                },
            },
        );
    }
}

#[derive(Clone, Copy)]
struct Candidate(f32, i64);

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.1.cmp(&other.1))
    }
}

fn streaming_candidates(
    db: &Connection,
    model_id: &str,
    query: &[f32],
    extension: Option<&str>,
    folder: Option<&Path>,
    file_limit: usize,
) -> Result<Vec<(f32, i64)>> {
    let mut sql = String::from(
        "SELECT c.id,c.file_id,c.vector,f.path FROM chunks c JOIN files f ON f.id=c.file_id
         WHERE c.model_id=?1 AND c.vector IS NOT NULL",
    );
    if extension.is_some() {
        sql.push_str(" AND f.extension=?2");
    }
    if folder.is_some() {
        sql.push_str(if extension.is_some() {
            " AND f.path LIKE ?3 ESCAPE '\\' COLLATE NOCASE"
        } else {
            " AND f.path LIKE ?2 ESCAPE '\\' COLLATE NOCASE"
        });
    }
    sql.push_str(" ORDER BY c.file_id,c.id");
    let mut stmt = db.prepare(&sql)?;
    let folder_pattern = folder.map(scope_like);
    let mut sql_values: Vec<&dyn rusqlite::ToSql> = vec![&model_id];
    let extension_value = extension.map(str::to_string);
    if let Some(extension) = extension_value.as_ref() {
        sql_values.push(extension);
    }
    if let Some(pattern) = folder_pattern.as_ref() {
        sql_values.push(pattern);
    }
    let mut rows = stmt.query(params_from_iter(sql_values))?;
    let mut top = BinaryHeap::<std::cmp::Reverse<Candidate>>::new();
    let mut current_file = None;
    let mut current_file_in_scope = true;
    let mut current_best: Option<Candidate> = None;
    while let Some(row) = rows.next()? {
        let chunk_id: i64 = row.get(0)?;
        let file_id: i64 = row.get(1)?;
        let raw: Vec<u8> = row.get(2)?;
        let path: String = row.get(3)?;
        if current_file.is_some_and(|previous| previous != file_id) {
            retain(&mut top, current_best.take(), file_limit);
            current_file_in_scope =
                folder.is_none_or(|folder| folder_contains_path(folder, Path::new(&path)));
        } else if current_file.is_none() {
            current_file_in_scope =
                folder.is_none_or(|folder| folder_contains_path(folder, Path::new(&path)));
        }
        current_file = Some(file_id);
        if !current_file_in_scope {
            current_best = None;
            continue;
        }
        let Some(mut vector) = decode_vector(&raw) else {
            continue;
        };
        if vector.len() != query.len() || vector.iter().any(|v| !v.is_finite()) {
            continue;
        }
        normalize(&mut vector);
        let candidate = Candidate(dot(&vector, query), chunk_id);
        if current_best.is_none_or(|best| {
            candidate.0 > best.0 || (candidate.0 == best.0 && candidate.1 < best.1)
        }) {
            current_best = Some(candidate);
        }
    }
    retain(&mut top, current_best, file_limit);
    let mut ranked: Vec<_> = top
        .into_iter()
        .map(|std::cmp::Reverse(Candidate(score, id))| (score, id))
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(ranked)
}

fn retain(
    top: &mut BinaryHeap<std::cmp::Reverse<Candidate>>,
    current: Option<Candidate>,
    limit: usize,
) {
    let Some(candidate) = current else { return };
    if top.len() < limit {
        top.push(std::cmp::Reverse(candidate));
    } else if top.peek().is_some_and(|worst| candidate > worst.0) {
        top.pop();
        top.push(std::cmp::Reverse(candidate));
    }
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn normalize(values: &mut [f32]) {
    let norm = values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in values {
            *value = (*value as f64 / norm) as f32;
        }
    }
}

fn compare_candidate_desc(a: &(f32, i64), b: &(f32, i64)) -> Ordering {
    b.0.partial_cmp(&a.0)
        .unwrap_or(Ordering::Equal)
        .then_with(|| b.1.cmp(&a.1))
}

fn terms(query: &str) -> Vec<String> {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let regex = REGEX.get_or_init(|| Regex::new(r"[\w.-]+").unwrap());
    let query = query.to_lowercase();
    let mut seen = HashSet::new();
    regex
        .find_iter(&query)
        .map(|m| m.as_str().to_string())
        .filter(|term| seen.insert(term.clone()))
        .take(24)
        .collect()
}

fn fts_expression<'a>(terms: impl Iterator<Item = &'a str>) -> String {
    terms
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn like(value: &str) -> String {
    format!(
        "%{}%",
        value
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

#[cfg(test)]
mod scope_path_tests {
    use super::*;

    #[test]
    fn scope_pattern_escapes_like_wildcards_and_keeps_root_separator() {
        #[cfg(windows)]
        {
            assert_eq!(
                scope_like(Path::new(r"C:\문서%_자료\")),
                r"C:\\문서\%\_자료\\%"
            );
            assert_eq!(scope_like(Path::new(r"C:\")), r"C:\\%");
        }
        #[cfg(not(windows))]
        assert_eq!(
            scope_like(Path::new("/tmp/자료%_폴더")),
            "/tmp/자료\\%\\_폴더/%"
        );
    }

    #[test]
    fn containment_observes_component_boundaries_and_platform_case_rules() {
        #[cfg(windows)]
        {
            assert!(folder_contains_path(
                Path::new(r"C:\Users\Me\문서"),
                Path::new(r"c:\users\me\문서\하위\파일.txt")
            ));
            assert!(!folder_contains_path(
                Path::new(r"C:\Users\Me\문서"),
                Path::new(r"C:\Users\Me\문서2\파일.txt")
            ));
            assert!(!folder_contains_path(
                Path::new(r"\\server\share\docs"),
                Path::new(r"\\server\share\docs-other\file.txt")
            ));
        }
        #[cfg(not(windows))]
        {
            assert!(folder_contains_path(
                Path::new("/tmp/docs"),
                Path::new("/tmp/docs/sub/file.txt")
            ));
            assert!(!folder_contains_path(
                Path::new("/tmp/docs"),
                Path::new("/tmp/docs2/file.txt")
            ));
        }
    }
}

fn name_match_count(file: &Value, terms: &[String]) -> usize {
    let name = file["name"].as_str().unwrap_or_default().to_lowercase();
    terms
        .iter()
        .filter(|term| name.contains(term.as_str()))
        .count()
}

fn preview(text: &str, terms: &[String], limit: usize) -> String {
    let folded = text.to_lowercase();
    let start_char = terms
        .iter()
        .filter_map(|term| {
            folded
                .find(term)
                .map(|index| folded[..index].chars().count())
        })
        .min()
        .unwrap_or(0)
        .saturating_sub(70);
    let excerpt: String = text.chars().skip(start_char).take(limit).collect();
    let prefix = if start_char > 0 { "…" } else { "" };
    let has_more = text.chars().count() > start_char + limit;
    format!("{prefix}{excerpt}{}", if has_more { "…" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extract::Passage, store::FileRecord};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct FixtureEngine {
        model: Mutex<String>,
        embeddings: AtomicUsize,
    }

    impl FixtureEngine {
        fn new(model: &str) -> Self {
            Self {
                model: Mutex::new(model.to_string()),
                embeddings: AtomicUsize::new(0),
            }
        }
    }

    impl Embedder for FixtureEngine {
        fn model_id(&self) -> Option<String> {
            Some(self.model.lock().unwrap().clone())
        }
        fn embed(&self, texts: &[String], _query: bool) -> Result<Vec<Vec<f32>>> {
            self.embeddings.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
        fn answer(&self, _question: &str, _context: &str) -> Result<String> {
            Ok("fixture".into())
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<Store>, PathBuf, i64) {
        let base = PathBuf::from(".moasearch-tests");
        std::fs::create_dir_all(&base).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("search-")
            .tempdir_in(base)
            .unwrap();
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let store = Arc::new(Store::open(temp.path().join("data")).unwrap());
        let root_id = store.add_root(library.to_str().unwrap()).unwrap()["id"]
            .as_i64()
            .unwrap();
        (temp, store, library, root_id)
    }

    fn add_document(
        store: &Store,
        library: &std::path::Path,
        root_id: i64,
        name: &str,
        vectors: Vec<Vec<f32>>,
        model_id: &str,
    ) {
        let path = library.join(name);
        std::fs::write(&path, "fixture text").unwrap();
        let passages = vectors
            .iter()
            .enumerate()
            .map(|(i, _)| Passage {
                text: format!("fixture passage {i}"),
                location: "본문".into(),
            })
            .collect();
        let record = FileRecord {
            root_id,
            path: crate::config::canonical(&path).unwrap(),
            size: 12,
            modified: 1,
            passages,
            vectors: Some(vectors),
            model_id: Some(model_id.into()),
            error: None,
            reader_id: "test".into(),
        };
        store.replace_files(&[record]).unwrap();
    }

    #[test]
    fn korean_terms_fts_quoting_and_preview_are_stable() {
        assert_eq!(
            terms("임대차 보증금 반환"),
            vec!["임대차", "보증금", "반환"]
        );
        assert_eq!(
            fts_expression(["a\"b", "보증금"].into_iter()),
            "\"a\"\"b\" OR \"보증금\""
        );
        assert_eq!(preview("앞쪽 ", &[], 3), "앞쪽 ");
        assert!(
            preview(&("x".repeat(200) + "보증금 반환"), &["보증금".into()], 20).starts_with('…')
        );
    }

    #[test]
    fn vector_cache_keeps_best_chunk_and_extension_filter() {
        let snapshot = VectorSnapshot {
            model_id: "google".into(),
            dimension: 2,
            revision: 1,
            vectors: vec![1.0, 0.0, 0.0, 1.0, 0.8, 0.6],
            extensions: vec![".txt".into(), ".pdf".into()],
            paths: vec![PathBuf::from("a.txt"), PathBuf::from("b.pdf")],
            rows: vec![
                VectorRow {
                    chunk_id: 1,
                    file_id: 10,
                    extension_id: 0,
                    path_id: 0,
                },
                VectorRow {
                    chunk_id: 2,
                    file_id: 10,
                    extension_id: 0,
                    path_id: 0,
                },
                VectorRow {
                    chunk_id: 3,
                    file_id: 20,
                    extension_id: 1,
                    path_id: 1,
                },
            ],
        };
        assert_eq!(
            snapshot.candidates(&[1.0, 0.0], None, None, 10),
            vec![(1.0, 1), (0.8, 3)]
        );
        assert_eq!(
            snapshot.candidates(&[1.0, 0.0], Some(".pdf"), None, 10),
            vec![(0.8, 3)]
        );
    }

    #[test]
    fn cached_and_bounded_fallback_match_negative_scores_and_extension_filter() {
        let (_temp, store, library, root_id) = fixture();
        add_document(
            &store,
            &library,
            root_id,
            "negative.txt",
            vec![vec![-1.0, 0.0], vec![-0.2, -0.98]],
            "google-a",
        );
        add_document(
            &store,
            &library,
            root_id,
            "less-negative.md",
            vec![vec![-0.8, -0.6]],
            "google-a",
        );
        add_document(
            &store,
            &library,
            root_id,
            "different.pdf",
            vec![vec![-0.98, -0.2]],
            "google-a",
        );
        let engine = Arc::new(FixtureEngine::new("google-a"));
        let cached = Search::with_cache_limit(store.clone(), engine.clone(), 1_000_000);
        let fallback = Search::with_cache_limit(store.clone(), engine, 0);
        let cached_result = cached
            .search("unmatched semantic intent", 10, None, "hybrid")
            .unwrap();
        let fallback_result = fallback
            .search("unmatched semantic intent", 10, None, "hybrid")
            .unwrap();
        let names = |result: &Value| {
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&cached_result),
            vec!["negative.txt", "less-negative.md", "different.pdf"]
        );
        assert_eq!(names(&fallback_result), names(&cached_result));
        let filtered = fallback
            .search("another semantic intent", 10, Some("txt"), "hybrid")
            .unwrap();
        assert_eq!(names(&filtered), vec!["negative.txt"]);
        assert!(fallback.vectors.lock().unwrap().snapshot.is_none());
    }

    #[test]
    fn scoped_search_filters_keyword_semantic_cache_fallback_and_answer_sources() {
        let (_temp, store, library, root_id) = fixture();
        let scope = library.join("scope%_dir");
        std::fs::create_dir_all(scope.join("nested")).unwrap();
        std::fs::create_dir_all(library.join("scope%_dir2")).unwrap();
        add_document(
            &store,
            &library,
            root_id,
            "scope%_dir/nested/needle-child.txt",
            vec![vec![1.0, 0.0]],
            "google-a",
        );
        add_document(
            &store,
            &library,
            root_id,
            "scope%_dir2/needle-sibling.txt",
            vec![vec![1.0, 0.0]],
            "google-a",
        );
        add_document(
            &store,
            &library,
            root_id,
            "outside.txt",
            vec![vec![1.0, 0.0]],
            "google-a",
        );
        let cached = Search::with_cache_limit(
            store.clone(),
            Arc::new(FixtureEngine::new("google-a")),
            1_000_000,
        );
        let fallback =
            Search::with_cache_limit(store.clone(), Arc::new(FixtureEngine::new("google-a")), 0);
        for search in [&cached, &fallback] {
            let keyword = search
                .search_in("needle", 10, None, "keyword", Some(&scope))
                .unwrap();
            assert_eq!(keyword["results"].as_array().unwrap().len(), 1);
            assert_eq!(keyword["results"][0]["name"], "needle-child.txt");

            // This query has no matching text or filename, exercising only
            // semantic retrieval before the per-file candidate limit.
            let semantic = search
                .search_in(
                    "unmatched semantic intent",
                    10,
                    None,
                    "hybrid",
                    Some(&scope),
                )
                .unwrap();
            assert_eq!(semantic["results"].as_array().unwrap().len(), 1);
            assert_eq!(semantic["results"][0]["name"], "needle-child.txt");

            let answer = search
                .answer_in("unmatched semantic intent", 5, Some(&scope))
                .unwrap();
            assert_eq!(answer["sources"].as_array().unwrap().len(), 1);
            assert_eq!(answer["sources"][0]["name"], "needle-child.txt");
        }
        // Reusing a global vector snapshot for another scope must still filter
        // before ranking, so its results cannot leak across scope changes.
        let sibling = cached
            .search_in(
                "unmatched semantic intent",
                10,
                None,
                "hybrid",
                Some(&library.join("scope%_dir2")),
            )
            .unwrap();
        assert_eq!(sibling["results"].as_array().unwrap().len(), 1);
        assert_eq!(sibling["results"][0]["name"], "needle-sibling.txt");
    }

    #[test]
    fn zero_cosine_ties_keep_higher_chunk_order_in_cache_and_fallback() {
        assert_eq!(
            Candidate(-0.0, 1).cmp(&Candidate(0.0, 1)),
            Ordering::Equal,
            "Rust ordering must match Python float equality for signed zero"
        );
        let (_temp, store, library, root_id) = fixture();
        add_document(
            &store,
            &library,
            root_id,
            "zero-a.txt",
            vec![vec![0.0, 1.0]],
            "google-a",
        );
        add_document(
            &store,
            &library,
            root_id,
            "zero-b.txt",
            vec![vec![0.0, -1.0]],
            "google-a",
        );
        let engine: Arc<dyn Embedder> = Arc::new(FixtureEngine::new("google-a"));
        let cached = Search::with_cache_limit(store.clone(), engine.clone(), 1_000_000);
        let fallback = Search::with_cache_limit(store, engine, 0);
        // Keep the query clear of both fixture filenames and passage text so
        // this test exercises semantic tie ordering only.
        let query = "zeta omega absent";
        let cached_names: Vec<_> = cached.search(query, 10, None, "hybrid").unwrap()["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect();
        let fallback_names: Vec<_> = fallback.search(query, 10, None, "hybrid").unwrap()["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(cached_names, vec!["zero-b.txt", "zero-a.txt"]);
        assert_eq!(fallback_names, cached_names);
    }

    #[test]
    fn query_embeddings_are_cached_by_model_and_vectors_invalidate_on_revision() {
        let (_temp, store, library, root_id) = fixture();
        add_document(
            &store,
            &library,
            root_id,
            "a.txt",
            vec![vec![1.0, 0.0]],
            "google-a",
        );
        let engine = Arc::new(FixtureEngine::new("google-a"));
        let search = Search::new(store.clone(), engine.clone());
        search
            .search("same natural language query", 10, None, "hybrid")
            .unwrap();
        let old_revision = search
            .vectors
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .revision;
        search
            .search("same natural language query", 10, None, "hybrid")
            .unwrap();
        assert_eq!(engine.embeddings.load(AtomicOrdering::SeqCst), 1);

        add_document(
            &store,
            &library,
            root_id,
            "b.txt",
            vec![vec![0.9, 0.1]],
            "google-a",
        );
        search
            .search("same natural language query", 10, None, "hybrid")
            .unwrap();
        assert!(
            search
                .vectors
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .revision
                > old_revision
        );
        assert_eq!(engine.embeddings.load(AtomicOrdering::SeqCst), 1);

        *engine.model.lock().unwrap() = "google-b".into();
        search
            .search("same natural language query", 10, None, "hybrid")
            .unwrap();
        assert_eq!(engine.embeddings.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(
            search
                .vectors
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .model_id,
            "google-b"
        );
    }

    #[test]
    fn exact_ties_choose_lowest_chunk_then_highest_file_id() {
        let (_temp, store, library, root_id) = fixture();
        for index in 0..65 {
            add_document(
                &store,
                &library,
                root_id,
                &format!("tie-{index:02}.txt"),
                vec![vec![1.0, 0.0]],
                "google-a",
            );
        }
        let engine = Arc::new(FixtureEngine::new("google-a"));
        let search = Search::new(store, engine);
        let result = search
            .search("query words absent from fixtures", 1, None, "hybrid")
            .unwrap();
        assert_eq!(result["results"][0]["name"], "tie-64.txt");
    }

    #[test]
    fn equal_rrf_scores_preserve_first_keyword_result_order() {
        let (_temp, store, library, root_id) = fixture();
        let a = library.join("needle-a.txt");
        let b = library.join("needle-b.txt");
        std::fs::write(&a, "needle filler").unwrap();
        std::fs::write(&b, "needle repeated").unwrap();
        store
            .replace_files(&[
                FileRecord {
                    root_id,
                    path: a,
                    size: 13,
                    modified: 20,
                    passages: vec![Passage {
                        text: "needle filler".into(),
                        location: "본문".into(),
                    }],
                    vectors: None,
                    model_id: None,
                    error: None,
                    reader_id: "test".into(),
                },
                FileRecord {
                    root_id,
                    path: b,
                    size: 15,
                    modified: 10,
                    passages: vec![Passage {
                        text: "needle needle needle needle needle needle needle needle".into(),
                        location: "본문".into(),
                    }],
                    vectors: None,
                    model_id: None,
                    error: None,
                    reader_id: "test".into(),
                },
            ])
            .unwrap();

        let search = Search::new(store, Arc::new(FixtureEngine::new("google-a")));
        let result = search.search("needle", 10, None, "keyword").unwrap();
        assert_eq!(result["results"][0]["name"], "needle-a.txt");
        assert_eq!(result["results"][1]["name"], "needle-b.txt");
        assert_eq!(
            result["results"][0]["score"], result["results"][1]["score"],
            "the filename/content rank contributions are reversed but equal"
        );
    }
}
