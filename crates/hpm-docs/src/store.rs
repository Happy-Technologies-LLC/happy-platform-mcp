//! SQLite FTS5 docs store (port of `src/docs/sqlite-store.js`).
//!
//! The schema, table names and FTS5 external-content layout are identical to 6.2.0, so a
//! cache written by the Node server opens as is. The vector index is the one part that is
//! not portable: see [`crate::vector`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use crate::chunker::Chunk;
use crate::error::DocsError;
use crate::vector::{VectorConfig, VectorIndex};

const SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS families (
    name TEXT PRIMARY KEY,
    branch TEXT NOT NULL,
    synced_at TEXT
  );
  CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    family TEXT NOT NULL,
    branch TEXT NOT NULL,
    path TEXT NOT NULL,
    sha TEXT,
    title TEXT,
    markdown TEXT NOT NULL,
    UNIQUE(family, path)
  );
  CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL,
    family TEXT NOT NULL,
    path TEXT NOT NULL,
    title TEXT,
    heading TEXT,
    start_line INTEGER,
    end_line INTEGER,
    body TEXT NOT NULL,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE
  );
  CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    title,
    heading,
    body,
    content='chunks',
    content_rowid='id'
  );
";

/// A document about to replace its cached copy. `branch` must equal the family's approved ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRecord {
    pub family: String,
    pub branch: String,
    pub path: String,
    pub sha: Option<String>,
    pub title: String,
    pub markdown: String,
}

/// One search hit. JSON keys match 6.2.0 (`startLine`, `endLine`, `vectorDistance`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: i64,
    pub family: String,
    pub path: String,
    pub title: Option<String>,
    pub heading: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_distance: Option<f64>,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredDocument {
    pub family: String,
    pub branch: String,
    pub path: String,
    pub title: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyStatus {
    pub name: String,
    pub branch: String,
    pub synced_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStatus {
    pub db_path: String,
    pub fts_available: bool,
    pub vector_available: bool,
    pub vector_reason: Option<String>,
    pub families: Vec<FamilyStatus>,
}

pub struct DocsStore {
    conn: Connection,
    db_path: PathBuf,
    vector: VectorIndex,
}

fn has_table(conn: &Connection, name: &str) -> Result<bool, DocsError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn remove_document_chunks(
    conn: &Connection,
    vector: &VectorIndex,
    document_id: i64,
) -> Result<(), DocsError> {
    let chunk_ids: Vec<i64> = conn
        .prepare_cached("SELECT id FROM chunks WHERE document_id = ?1")?
        .query_map([document_id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    conn.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE document_id = ?1)",
        [document_id],
    )?;
    vector.remove_chunks(conn, &chunk_ids)?;
    conn.execute("DELETE FROM chunks WHERE document_id = ?1", [document_id])?;
    Ok(())
}

fn delete_document(
    conn: &Connection,
    vector: &VectorIndex,
    document_id: i64,
) -> Result<(), DocsError> {
    remove_document_chunks(conn, vector, document_id)?;
    conn.execute("DELETE FROM documents WHERE id = ?1", [document_id])?;
    Ok(())
}

impl DocsStore {
    /// Opens (creating if needed) the database at `db_path`; call [`Self::initialize`] next.
    /// Dropping the store closes the connection.
    pub fn open(db_path: &Path, vector_config: &VectorConfig) -> Result<Self, DocsError> {
        Ok(Self {
            conn: Connection::open(db_path)?,
            db_path: db_path.to_owned(),
            vector: VectorIndex::new(vector_config, true),
        })
    }

    /// Sets pragmas, discards a pre-provenance cache, creates the schema and, when vector
    /// search is on, reconciles the embeddings with the chunks (first-run rebuild).
    pub fn initialize(&mut self) -> Result<(), DocsError> {
        self.conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;",
        )?;
        self.vector.ensure_tables(&self.conn);
        self.discard_legacy_cache()?;
        self.conn.execute_batch(SCHEMA)?;
        self.vector.reconcile(&mut self.conn);
        Ok(())
    }

    /// Caches written before branch provenance existed cannot prove which ref their content
    /// came from; discard them instead of serving them. Rows are deleted (not tables dropped)
    /// so AUTOINCREMENT ids are never reused.
    fn discard_legacy_cache(&mut self) -> Result<(), DocsError> {
        if !has_table(&self.conn, "documents")? {
            return Ok(());
        }
        let has_branch = self
            .conn
            .prepare("PRAGMA table_info(documents)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "branch");
        if has_branch {
            return Ok(());
        }
        let has_fts = has_table(&self.conn, "chunks_fts")?;
        let has_chunks = has_table(&self.conn, "chunks")?;
        let has_families = has_table(&self.conn, "families")?;
        let tx = self.conn.transaction()?;
        if has_fts {
            tx.execute_batch("INSERT INTO chunks_fts(chunks_fts) VALUES('delete-all')")?;
        }
        if has_chunks {
            tx.execute_batch("DELETE FROM chunks")?;
        }
        tx.execute_batch("DELETE FROM documents")?;
        tx.execute_batch("ALTER TABLE documents ADD COLUMN branch TEXT NOT NULL DEFAULT ''")?;
        if has_families {
            tx.execute_batch("DELETE FROM families")?;
        }
        self.vector.clear(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Records the approved ref for a family before any document is written. Content cached
    /// from any other ref is purged; returns whether anything was invalidated.
    pub fn begin_family_sync(&mut self, name: &str, branch: &str) -> Result<bool, DocsError> {
        let tx = self.conn.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT branch FROM families WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let stale: Vec<i64> = tx
            .prepare("SELECT id FROM documents WHERE family = ?1 AND branch <> ?2")?
            .query_map(params![name, branch], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in &stale {
            delete_document(&tx, &self.vector, *id)?;
        }
        tx.execute(
            "INSERT INTO families (name, branch, synced_at)
             VALUES (?1, ?2, NULL)
             ON CONFLICT(name) DO UPDATE SET
               branch = excluded.branch,
               synced_at = CASE WHEN families.branch = excluded.branch THEN families.synced_at ELSE NULL END",
            params![name, branch],
        )?;
        tx.commit()?;
        Ok(!stale.is_empty() || existing.is_some_and(|previous| previous != branch))
    }

    /// Replaces a document and its chunks atomically.
    pub fn replace_document(
        &mut self,
        document: &DocumentRecord,
        chunks: &[Chunk],
    ) -> Result<(), DocsError> {
        let tx = self.conn.transaction()?;
        replace_document_in(&tx, &self.vector, document, chunks)?;
        tx.commit()?;
        Ok(())
    }

    /// Marks a completed sync and removes documents no longer listed by the family index.
    /// Returns the removed document paths.
    pub fn complete_family_sync(
        &mut self,
        name: &str,
        branch: &str,
        synced_at: &str,
        keep_paths: &HashSet<String>,
    ) -> Result<Vec<String>, DocsError> {
        let tx = self.conn.transaction()?;
        let rows: Vec<(i64, String)> = tx
            .prepare("SELECT id, path FROM documents WHERE family = ?1")?
            .query_map([name], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut removed = Vec::new();
        for (id, path) in rows {
            if keep_paths.contains(&path) {
                continue;
            }
            delete_document(&tx, &self.vector, id)?;
            removed.push(path);
        }
        tx.execute(
            "UPDATE families SET synced_at = ?1 WHERE name = ?2 AND branch = ?3",
            params![synced_at, name, branch],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// FTS5 (and, when enabled, vector) search. Vector hits come first, then FTS hits not
    /// already present. `query` is an FTS5 MATCH expression, passed through as in 6.2.0.
    pub fn search(
        &self,
        query: &str,
        family: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DocsError> {
        let family = family.filter(|name| !name.is_empty());
        if !self.vector.available() {
            return self.fts_results(query, family, limit);
        }
        // A failing vector query must not take text search down with it.
        let Ok(mut merged) = self.vector.search(&self.conn, query, family, limit) else {
            return self.fts_results(query, family, limit);
        };
        let seen: HashSet<i64> = merged.iter().map(|result| result.id).collect();
        merged.extend(
            self.fts_results(query, family, limit)?
                .into_iter()
                .filter(|result| !seen.contains(&result.id)),
        );
        merged.truncate(limit);
        Ok(merged)
    }

    fn fts_results(
        &self,
        query: &str,
        family: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DocsError> {
        let mut statement = self.conn.prepare_cached(
            "SELECT c.id, c.family, c.path, c.title, c.heading, c.start_line AS startLine,
                    c.end_line AS endLine, snippet(chunks_fts, 2, '<mark>', '</mark>', '...', 20) AS snippet
             FROM chunks_fts
             JOIN chunks c ON c.id = chunks_fts.rowid
             WHERE chunks_fts MATCH ?1
               AND (?2 IS NULL OR c.family = ?2)
             ORDER BY rank
             LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![query, family, limit], |row| {
            Ok(SearchResult {
                id: row.get(0)?,
                family: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                heading: row.get(4)?,
                start_line: row.get(5)?,
                end_line: row.get(6)?,
                vector_distance: None,
                snippet: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Only content whose recorded ref still equals the family's approved ref.
    pub fn get_document(
        &self,
        family: &str,
        path: &str,
    ) -> Result<Option<StoredDocument>, DocsError> {
        Ok(self
            .conn
            .query_row(
                "SELECT d.family, d.branch, d.path, d.title, d.markdown
                 FROM documents d
                 JOIN families f ON f.name = d.family AND f.branch = d.branch
                 WHERE d.family = ?1 AND d.path = ?2",
                params![family, path],
                |row| {
                    Ok(StoredDocument {
                        family: row.get(0)?,
                        branch: row.get(1)?,
                        path: row.get(2)?,
                        title: row.get(3)?,
                        markdown: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn status(&self) -> Result<StoreStatus, DocsError> {
        let families = self
            .conn
            .prepare("SELECT name, branch, synced_at FROM families ORDER BY name")?
            .query_map([], |row| {
                Ok(FamilyStatus {
                    name: row.get(0)?,
                    branch: row.get(1)?,
                    synced_at: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(StoreStatus {
            db_path: self.db_path.to_string_lossy().into_owned(),
            fts_available: true,
            vector_available: self.vector.available(),
            vector_reason: self.vector.reason().map(str::to_owned),
            families,
        })
    }

    /// Closes the connection, reporting failures that a plain drop would swallow.
    pub fn close(self) -> Result<(), DocsError> {
        self.conn.close().map_err(|(_, error)| error.into())
    }
}

fn replace_document_in(
    tx: &Transaction<'_>,
    vector: &VectorIndex,
    document: &DocumentRecord,
    chunks: &[Chunk],
) -> Result<(), DocsError> {
    let approved: Option<String> = tx
        .query_row(
            "SELECT branch FROM families WHERE name = ?1",
            [&document.family],
            |row| row.get(0),
        )
        .optional()?;
    if approved.as_deref() != Some(document.branch.as_str()) {
        return Err(DocsError::plain(format!(
            "Docs cache provenance mismatch for family {}",
            document.family
        )));
    }

    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM documents WHERE family = ?1 AND path = ?2",
            params![document.family, document.path],
            |row| row.get(0),
        )
        .optional()?;
    let document_id = if let Some(id) = existing {
        remove_document_chunks(tx, vector, id)?;
        tx.execute(
            "UPDATE documents SET branch = ?1, sha = ?2, title = ?3, markdown = ?4 WHERE id = ?5",
            params![
                document.branch,
                document.sha,
                document.title,
                document.markdown,
                id
            ],
        )?;
        id
    } else {
        tx.execute(
            "INSERT INTO documents (family, branch, path, sha, title, markdown) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![document.family, document.branch, document.path, document.sha, document.title, document.markdown],
        )?;
        tx.last_insert_rowid()
    };

    let mut insert_chunk = tx.prepare_cached(
        "INSERT INTO chunks (document_id, family, path, title, heading, start_line, end_line, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let mut insert_fts = tx.prepare_cached(
        "INSERT INTO chunks_fts (rowid, title, heading, body) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut indexed: Vec<(i64, &str, &str, &str)> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        insert_chunk.execute(params![
            document_id,
            chunk.family,
            chunk.path,
            chunk.title,
            chunk.heading,
            chunk.start_line,
            chunk.end_line,
            chunk.body
        ])?;
        let id = tx.last_insert_rowid();
        insert_fts.execute(params![id, chunk.title, chunk.heading, chunk.body])?;
        indexed.push((id, &chunk.title, &chunk.heading, &chunk.body));
    }
    vector.index_chunks(tx, &indexed)
}

// Ported from tests/docs-sqlite-store.test.js. The JS cases that exercised the sqlite-vec
// loader (`closes the database when vector initialization throws`) have no Rust equivalent:
// the connection is owned by the store, so every failure path closes it on drop, and there
// is no extension to load.
#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn chunk(path: &str, body: &str) -> Chunk {
        Chunk {
            family: "australia".to_owned(),
            path: path.to_owned(),
            title: "Flow Designer".to_owned(),
            heading: "Flow Designer".to_owned(),
            start_line: 1,
            end_line: 3,
            body: body.to_owned(),
        }
    }

    fn record(path: &str, branch: &str, markdown: &str) -> DocumentRecord {
        DocumentRecord {
            family: "australia".to_owned(),
            branch: branch.to_owned(),
            path: path.to_owned(),
            sha: Some("abc".to_owned()),
            title: "Flow Designer".to_owned(),
            markdown: markdown.to_owned(),
        }
    }

    fn open(dir: &TempDir, name: &str, vector: &VectorConfig) -> DocsStore {
        let mut store = DocsStore::open(&dir.path().join(name), vector).unwrap();
        store.initialize().unwrap();
        store
    }

    fn local_vector() -> VectorConfig {
        VectorConfig {
            enable_vector: true,
            embedding_provider: "local".to_owned(),
            ..VectorConfig::default()
        }
    }

    #[test]
    fn indexes_and_searches_chunks_with_fts5() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir, "index.sqlite", &VectorConfig::default());
        store.begin_family_sync("australia", "australia").unwrap();
        let markdown = "# Flow Designer\n\nCreate actions.";
        store
            .replace_document(
                &record("foo.md", "australia", markdown),
                &[chunk("foo.md", markdown)],
            )
            .unwrap();

        let results = store
            .search("create actions", Some("australia"), 5)
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].family, "australia");
        assert_eq!(results[0].path, "foo.md");
        assert_eq!(results[0].title.as_deref(), Some("Flow Designer"));
        assert_eq!(results[0].vector_distance, None);
        let json = serde_json::to_value(&results[0]).unwrap();
        assert!(json.get("startLine").is_some() && json.get("vectorDistance").is_none());
        store.close().unwrap();
    }

    #[test]
    fn refuses_documents_whose_ref_differs_from_the_family_approved_ref() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir, "index.sqlite", &VectorConfig::default());
        store.begin_family_sync("australia", "australia").unwrap();

        let error = store
            .replace_document(&record("x.md", "zurich", "# X"), &[])
            .unwrap_err();

        assert!(error.message().contains("provenance mismatch"));
        assert!(store.get_document("australia", "x.md").unwrap().is_none());
    }

    #[test]
    fn reports_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir, "index.sqlite", &VectorConfig::default());
        let status = store.status().unwrap();
        assert!(status.fts_available);
        assert!(!status.vector_available);
        assert_eq!(
            status.vector_reason.as_deref(),
            Some("Vector search disabled")
        );
        assert!(status.families.is_empty());
    }

    #[test]
    fn indexes_and_searches_chunks_with_the_local_vector_index_when_vector_mode_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir, "vector.sqlite", &local_vector());
        assert!(store.status().unwrap().vector_available);
        store.begin_family_sync("australia", "australia").unwrap();
        let markdown = "# Flow Designer\n\nCreate reusable flow actions.";
        store
            .replace_document(
                &record("vector.md", "australia", markdown),
                &[chunk("vector.md", markdown)],
            )
            .unwrap();

        let results = store
            .search("reusable flow actions", Some("australia"), 5)
            .unwrap();

        assert_eq!(results[0].family, "australia");
        assert_eq!(results[0].path, "vector.md");
        assert_eq!(results[0].title.as_deref(), Some("Flow Designer"));
        assert!(results[0].vector_distance.is_some());
    }

    #[test]
    fn a_failed_vector_rebuild_disables_vector_search_with_a_reason_and_keeps_text_search() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = open(&dir, "index.sqlite", &VectorConfig::default());
            store.begin_family_sync("australia", "australia").unwrap();
            let markdown = "# Flow Designer\n\nCreate actions.";
            store
                .replace_document(
                    &record("a.md", "australia", markdown),
                    &[chunk("a.md", markdown)],
                )
                .unwrap();
        }
        let broken = VectorConfig {
            enable_vector: true,
            embedding_provider: "custom".to_owned(),
            embed_text: Some(std::sync::Arc::new(|_| vec![0.0; 3])),
            ..VectorConfig::default()
        };

        let store = open(&dir, "index.sqlite", &broken);

        let status = store.status().unwrap();
        assert!(!status.vector_available);
        assert!(
            status.vector_reason.unwrap().starts_with(
                "Vector index rebuild failed: Embedding has 3 dimensions; expected 128"
            )
        );
        assert_eq!(
            store.search("actions", Some("australia"), 5).unwrap().len(),
            1
        );
    }

    #[test]
    fn opening_an_up_to_date_vector_cache_does_not_need_the_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = open(&dir, "index.sqlite", &local_vector());
            store.begin_family_sync("australia", "australia").unwrap();
            let markdown = "# Flow Designer\n\nCreate actions.";
            store
                .replace_document(
                    &record("a.md", "australia", markdown),
                    &[chunk("a.md", markdown)],
                )
                .unwrap();
        }
        // Another process holds the write lock (for instance a sync on the same cache).
        let writer = Connection::open(dir.path().join("index.sqlite")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let store = open(&dir, "index.sqlite", &local_vector());

        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(store.status().unwrap().vector_available);
        assert_eq!(
            store.search("actions", Some("australia"), 5).unwrap().len(),
            1
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn begin_family_sync_purges_content_from_another_ref_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir, "index.sqlite", &local_vector());
        assert!(!store.begin_family_sync("australia", "australia").unwrap());
        store
            .replace_document(
                &record("a.md", "australia", "# A"),
                &[chunk("a.md", "alpha")],
            )
            .unwrap();
        assert!(!store.begin_family_sync("australia", "australia").unwrap());
        assert!(store.begin_family_sync("australia", "australia-2").unwrap());

        assert!(
            store
                .search("alpha", Some("australia"), 5)
                .unwrap()
                .is_empty()
        );
        assert!(store.get_document("australia", "a.md").unwrap().is_none());
    }

    // ---- compatibility with a database written by the 6.2.0 Node server ----

    const JS_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/js-6.2.0-docs-index.sqlite");

    fn js_fixture(vector: &VectorConfig) -> (TempDir, DocsStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.sqlite"), JS_FIXTURE).unwrap();
        let store = open(&dir, "index.sqlite", vector);
        (dir, store)
    }

    #[test]
    fn opens_a_database_created_by_the_js_store_and_serves_its_content() {
        let (_dir, store) = js_fixture(&VectorConfig::default());

        let status = store.status().unwrap();
        assert_eq!(
            status.families,
            [FamilyStatus {
                name: "australia".to_owned(),
                branch: "australia".to_owned(),
                synced_at: Some("2026-01-02T03:04:05.678Z".to_owned()),
            }]
        );
        let document = store
            .get_document("australia", "platform/flow-designer.md")
            .unwrap()
            .unwrap();
        assert_eq!(document.title.as_deref(), Some("Flow Designer"));
        assert_eq!(document.branch, "australia");
        let hits = store.search("triggers", Some("australia"), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].heading.as_deref(), Some("Triggers"));
        assert_eq!(hits[0].path, "platform/flow-designer.md");
        assert!(hits[0].snippet.contains("<mark>"));
    }

    #[test]
    fn can_resync_and_delete_in_a_database_created_by_the_js_store() {
        let (_dir, mut store) = js_fixture(&VectorConfig::default());

        assert!(!store.begin_family_sync("australia", "australia").unwrap());
        let markdown = "# New Page\n\nFresh replacement text.";
        store
            .replace_document(
                &record("platform/new.md", "australia", markdown),
                &[chunk("platform/new.md", markdown)],
            )
            .unwrap();
        let keep: HashSet<String> = [
            "platform/flow-designer.md".to_owned(),
            "platform/new.md".to_owned(),
        ]
        .into();
        let removed = store
            .complete_family_sync("australia", "australia", "2026-02-03T04:05:06.789Z", &keep)
            .unwrap();

        assert_eq!(removed, ["platform/old.md"]);
        assert!(
            store
                .get_document("australia", "platform/old.md")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .search("legacy", Some("australia"), 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search("fresh", Some("australia"), 5).unwrap().len(),
            1
        );
        assert_eq!(
            store.status().unwrap().families[0].synced_at.as_deref(),
            Some("2026-02-03T04:05:06.789Z")
        );
    }

    #[test]
    fn the_js_fixture_carries_a_sqlite_vec_table_that_the_rust_store_never_touches() {
        let (dir, _store) = js_fixture(&VectorConfig::default());
        let raw = Connection::open(dir.path().join("index.sqlite")).unwrap();
        let vec0: String = raw
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'docs_chunk_vectors'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(vec0.to_lowercase().contains("vec0"));
    }

    #[test]
    fn rebuilds_the_vector_index_on_first_run_over_a_js_database() {
        let (_dir, mut store) = js_fixture(&local_vector());

        assert!(store.status().unwrap().vector_available);
        let results = store
            .search("reusable flow actions", Some("australia"), 5)
            .unwrap();
        assert_eq!(results[0].path, "platform/flow-designer.md");
        assert!(results[0].vector_distance.is_some());

        // Resync over the 6.x database: replace a document, add one, drop another.
        assert!(!store.begin_family_sync("australia", "australia").unwrap());
        let markdown = "# Flow Designer\n\nRewritten reusable flow actions.";
        store
            .replace_document(
                &record("platform/flow-designer.md", "australia", markdown),
                &[chunk("platform/flow-designer.md", markdown)],
            )
            .unwrap();
        let keep: HashSet<String> = ["platform/flow-designer.md".to_owned()].into();
        let removed = store
            .complete_family_sync("australia", "australia", "2026-02-03T04:05:06.789Z", &keep)
            .unwrap();

        assert_eq!(removed, ["platform/old.md"]);
        let after = store
            .search("legacy workflows", Some("australia"), 5)
            .unwrap();
        assert!(after.iter().all(|hit| hit.path != "platform/old.md"));
        let rewritten = store.search("rewritten", Some("australia"), 5).unwrap();
        assert_eq!(rewritten[0].path, "platform/flow-designer.md");
        assert!(rewritten[0].vector_distance.is_some());
        store.close().unwrap();
    }
}
