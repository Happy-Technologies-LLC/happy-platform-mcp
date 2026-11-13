//! Local vector index (port of `src/docs/vector-index.js`).
//!
//! The JS index lived in a `sqlite-vec` `vec0` virtual table that cannot be opened without
//! that extension, so it is **not portable**. This index stores embeddings as `f32` BLOBs in
//! a plain table (`docs_chunk_embeddings`) and ranks by brute-force L2 distance (the `vec0`
//! default metric). Existing caches are upgraded transparently: on first use every chunk
//! without an embedding is embedded ("rebuilt on first run"); a leftover `vec0` table from
//! 6.x is never touched and stays inert.

use std::fmt;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::DocsError;
use crate::store::SearchResult;

pub const DEFAULT_VECTOR_DIMENSIONS: usize = 128;

const EMBEDDINGS_TABLE_SQL: &str = "
  CREATE TABLE IF NOT EXISTS docs_chunk_embeddings (
    chunk_id INTEGER PRIMARY KEY,
    embedding BLOB NOT NULL
  );
  CREATE TABLE IF NOT EXISTS docs_vector_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
  );
";
const SIGNATURE_KEY: &str = "embedding_signature";

pub type EmbedFn = Arc<dyn Fn(&str) -> Vec<f64> + Send + Sync>;

/// Vector settings (JS `{ enableVector, embeddingProvider, dimensions, embedText }`).
#[derive(Clone)]
pub struct VectorConfig {
    pub enable_vector: bool,
    pub embedding_provider: String,
    pub dimensions: usize,
    /// Overrides the provider's embedder (tests, future providers).
    pub embed_text: Option<EmbedFn>,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            enable_vector: false,
            embedding_provider: "none".to_owned(),
            dimensions: DEFAULT_VECTOR_DIMENSIONS,
            embed_text: None,
        }
    }
}

impl fmt::Debug for VectorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorConfig")
            .field("enable_vector", &self.enable_vector)
            .field("embedding_provider", &self.embedding_provider)
            .field("dimensions", &self.dimensions)
            .field("embed_text", &self.embed_text.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl From<&crate::config::DocsConfig> for VectorConfig {
    fn from(config: &crate::config::DocsConfig) -> Self {
        Self {
            enable_vector: config.enable_vector,
            embedding_provider: config.embedding_provider.clone(),
            ..Self::default()
        }
    }
}

/// FNV-1a over the token's bytes (tokens are ASCII, so bytes equal the JS UTF-16 units).
fn hash_token(token: &str) -> u32 {
    token.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

/// Deterministic hashed bag-of-tokens embedding, L2-normalized and rounded to 6 decimals.
#[must_use]
pub fn create_local_embedding(text: &str, dimensions: usize) -> Vec<f64> {
    let mut vector = vec![0.0_f64; dimensions];
    if dimensions == 0 {
        return vector;
    }
    let lowered = text.to_lowercase();
    for token in lowered
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
        .filter(|token| !token.is_empty())
    {
        let hash = hash_token(token);
        let index = usize::try_from(hash).unwrap_or(0) % dimensions;
        vector[index] += if hash & 1 == 0 { 1.0 } else { -1.0 };
    }
    let magnitude = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
    let magnitude = if magnitude == 0.0 { 1.0 } else { magnitude };
    for value in &mut vector {
        *value = round6(*value / magnitude);
    }
    vector
}

fn round6(value: f64) -> f64 {
    format!("{value:.6}").parse().unwrap_or(value)
}

/// `[0.1,0.2]`-style JSON text of a vector (JS `serializeVector`), for diagnostics.
#[must_use]
pub fn serialize_vector(vector: &[f64]) -> String {
    serde_json::to_string(vector).unwrap_or_default()
}

fn encode_embedding(vector: &[f64]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|&value| (value as f32).to_le_bytes())
        .collect()
}

fn l2_distance(query: &[f32], blob: &[u8]) -> Option<f64> {
    if blob.len() != query.len() * 4 {
        return None;
    }
    let sum: f64 = blob
        .as_chunks::<4>()
        .0
        .iter()
        .zip(query)
        .map(|(&raw, &q)| {
            let stored = f32::from_le_bytes(raw);
            let delta = f64::from(stored) - f64::from(q);
            delta * delta
        })
        .sum();
    Some(sum.sqrt())
}

struct Embedder {
    embed: EmbedFn,
    dimensions: usize,
    signature: String,
}

/// State of the vector index. Holds no connection; the owning store passes its own.
pub struct VectorIndex {
    embedder: Option<Embedder>,
    reason: Option<String>,
}

/// Text embedded for a chunk: title, heading and body, blank parts omitted.
fn embedding_text(title: Option<&str>, heading: Option<&str>, body: &str) -> String {
    [title, heading, Some(body)]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl VectorIndex {
    fn disabled(reason: impl Into<String>) -> Self {
        Self {
            embedder: None,
            reason: Some(reason.into()),
        }
    }

    /// Resolves the configuration (JS `createVectorIndex` before touching the database).
    /// `has_db` is false when only availability is being reported, e.g. status without a
    /// store.
    #[must_use]
    pub fn new(config: &VectorConfig, has_db: bool) -> Self {
        if !config.enable_vector {
            return Self::disabled("Vector search disabled");
        }
        let provider = config.embedding_provider.as_str();
        if provider.is_empty() || provider == "none" {
            return Self::disabled("Vector search requires an embedding provider");
        }
        if !has_db {
            return Self::disabled("Vector search requires an initialized SQLite database");
        }
        let dimensions = config.dimensions;
        let embed: EmbedFn = match (&config.embed_text, provider) {
            (Some(custom), _) => Arc::clone(custom),
            (None, "local") => Arc::new(move |text| create_local_embedding(text, dimensions)),
            _ => return Self::disabled(format!("Unsupported embedding provider: {provider}")),
        };
        Self {
            embedder: Some(Embedder {
                embed,
                dimensions,
                signature: format!("{provider}:{dimensions}"),
            }),
            reason: None,
        }
    }

    #[must_use]
    pub const fn available(&self) -> bool {
        self.embedder.is_some()
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Creates the embedding tables. A failure disables the index (reported through
    /// [`Self::reason`]) instead of failing the store, as the JS `sqlite-vec` init did.
    pub fn ensure_tables(&mut self, conn: &Connection) {
        if self.embedder.is_none() {
            return;
        }
        if let Err(error) = conn.execute_batch(EMBEDDINGS_TABLE_SQL) {
            self.embedder = None;
            self.reason = Some(format!("Vector index initialization failed: {error}"));
        }
    }

    /// Brings the embeddings in line with the chunks: drops everything when the embedding
    /// signature changed, removes orphans and embeds every chunk that has no embedding yet.
    /// This is the one-time rebuild of an index that came from the JS server.
    pub fn reconcile(&self, conn: &mut Connection) -> Result<(), DocsError> {
        let Some(embedder) = &self.embedder else {
            return Ok(());
        };
        let tx = conn.transaction()?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM docs_vector_meta WHERE key = ?1",
                [SIGNATURE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if stored.as_deref() != Some(embedder.signature.as_str()) {
            tx.execute("DELETE FROM docs_chunk_embeddings", [])?;
        }
        tx.execute(
            "DELETE FROM docs_chunk_embeddings WHERE chunk_id NOT IN (SELECT id FROM chunks)",
            [],
        )?;

        let missing: Vec<(i64, Option<String>, Option<String>, String)> = {
            let mut statement = tx.prepare(
                "SELECT id, title, heading, body FROM chunks
                 WHERE id NOT IN (SELECT chunk_id FROM docs_chunk_embeddings)",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<_, _>>()?
        };
        {
            let mut insert =
                tx.prepare("INSERT OR REPLACE INTO docs_chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)")?;
            for (id, title, heading, body) in &missing {
                let vector = self.embed(
                    embedder,
                    &embedding_text(title.as_deref(), heading.as_deref(), body),
                )?;
                insert.execute(params![id, encode_embedding(&vector)])?;
            }
        }
        tx.execute(
            "INSERT INTO docs_vector_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SIGNATURE_KEY, embedder.signature],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn embed(&self, embedder: &Embedder, text: &str) -> Result<Vec<f64>, DocsError> {
        let vector = (embedder.embed)(text);
        if vector.len() == embedder.dimensions {
            Ok(vector)
        } else {
            Err(DocsError::plain(format!(
                "Embedding has {} dimensions; expected {}",
                vector.len(),
                embedder.dimensions
            )))
        }
    }

    /// Embeds and stores freshly inserted chunks `(id, title, heading, body)`.
    pub fn index_chunks(
        &self,
        conn: &Connection,
        chunks: &[(i64, &str, &str, &str)],
    ) -> Result<(), DocsError> {
        let Some(embedder) = &self.embedder else {
            return Ok(());
        };
        let mut insert = conn.prepare_cached(
            "INSERT OR REPLACE INTO docs_chunk_embeddings (chunk_id, embedding) VALUES (?1, ?2)",
        )?;
        for &(id, title, heading, body) in chunks {
            let vector = self.embed(embedder, &embedding_text(Some(title), Some(heading), body))?;
            insert.execute(params![id, encode_embedding(&vector)])?;
        }
        Ok(())
    }

    pub fn remove_chunks(&self, conn: &Connection, chunk_ids: &[i64]) -> Result<(), DocsError> {
        if self.embedder.is_none() {
            return Ok(());
        }
        let mut delete =
            conn.prepare_cached("DELETE FROM docs_chunk_embeddings WHERE chunk_id = ?1")?;
        for id in chunk_ids {
            delete.execute([id])?;
        }
        Ok(())
    }

    pub fn clear(&self, conn: &Connection) -> Result<(), DocsError> {
        if self.embedder.is_some() {
            conn.execute("DELETE FROM docs_chunk_embeddings", [])?;
        }
        Ok(())
    }

    /// Nearest chunks by L2 distance, filtered by family before ranking.
    pub fn search(
        &self,
        conn: &Connection,
        query: &str,
        family: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, DocsError> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_possible_truncation)]
        let needle: Vec<f32> = self
            .embed(embedder, query)?
            .iter()
            .map(|&v| v as f32)
            .collect();

        let mut ranked: Vec<(f64, i64)> = Vec::new();
        {
            let mut statement = conn.prepare_cached(
                "SELECT e.chunk_id, e.embedding
                 FROM docs_chunk_embeddings e JOIN chunks c ON c.id = e.chunk_id
                 WHERE (?1 IS NULL OR c.family = ?1)",
            )?;
            let mut rows = statement.query([family])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let blob = row
                    .get_ref(1)?
                    .as_blob()
                    .map_err(|e| DocsError::plain(e.to_string()))?;
                if let Some(distance) = l2_distance(&needle, blob) {
                    ranked.push((distance, id));
                }
            }
        }
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        ranked.truncate(limit);

        let mut by_id = conn.prepare_cached(
            "SELECT c.id, c.family, c.path, c.title, c.heading, c.start_line, c.end_line, substr(c.body, 1, 240)
             FROM chunks c WHERE c.id = ?1",
        )?;
        let mut results = Vec::with_capacity(ranked.len());
        for (distance, id) in ranked {
            results.push(by_id.query_row([id], |row| {
                Ok(SearchResult {
                    id: row.get(0)?,
                    family: row.get(1)?,
                    path: row.get(2)?,
                    title: row.get(3)?,
                    heading: row.get(4)?,
                    start_line: row.get(5)?,
                    end_line: row.get(6)?,
                    vector_distance: Some(distance),
                    snippet: row.get(7)?,
                })
            })?);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/docs-vector-index.test.js. The two sqlite-vec loader cases are
    // replaced: there is no extension to load, so availability is decided by configuration
    // and table creation (see `creates_the_embedding_tables_when_configured`).

    fn enabled(provider: &str) -> VectorConfig {
        VectorConfig {
            enable_vector: true,
            embedding_provider: provider.to_owned(),
            ..VectorConfig::default()
        }
    }

    #[test]
    fn is_disabled_when_vector_config_is_false() {
        let index = VectorIndex::new(&VectorConfig::default(), false);
        assert!(!index.available());
        assert_eq!(index.reason(), Some("Vector search disabled"));
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            index
                .search(&conn, "anything", None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reports_missing_embedding_provider_as_unavailable() {
        let index = VectorIndex::new(&enabled("none"), true);
        assert!(!index.available());
        assert!(index.reason().unwrap().contains("embedding provider"));
    }

    #[test]
    fn reports_missing_database_and_unsupported_provider() {
        assert_eq!(
            VectorIndex::new(&enabled("local"), false).reason(),
            Some("Vector search requires an initialized SQLite database")
        );
        assert_eq!(
            VectorIndex::new(&enabled("openai"), true).reason(),
            Some("Unsupported embedding provider: openai")
        );
    }

    #[test]
    fn creates_deterministic_local_embeddings() {
        assert_eq!(
            create_local_embedding("Flow Designer action", 128),
            create_local_embedding("Flow Designer action", 128)
        );
        assert_eq!(
            create_local_embedding("Flow Designer action", 128).len(),
            128
        );
    }

    #[test]
    fn local_embeddings_are_normalized_and_match_the_js_algorithm() {
        let vector = create_local_embedding("a", 128);
        // FNV-1a("a") = 0xE40C292C: even hash => +1 at index 0xE40C292C % 128 = 44.
        let nonzero: Vec<(usize, f64)> = vector
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, v)| *v != 0.0)
            .collect();
        assert_eq!(nonzero, [(44, 1.0)]);
        assert!(create_local_embedding("", 8).iter().all(|v| *v == 0.0));
        let mixed = create_local_embedding("Hello, hello_World 42", 128);
        let norm = mixed.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3);
    }

    #[test]
    fn serializes_vectors_as_json_arrays() {
        assert_eq!(serialize_vector(&[0.1, 0.2, 0.3]), "[0.1,0.2,0.3]");
    }

    #[test]
    fn embedding_blobs_round_trip_and_rank_by_l2() {
        let blob = encode_embedding(&[0.5, -0.25]);
        assert_eq!(blob.len(), 8);
        assert!((l2_distance(&[0.5, -0.25], &blob).unwrap()).abs() < 1e-9);
        assert!((l2_distance(&[0.5, 0.75], &blob).unwrap() - 1.0).abs() < 1e-9);
        assert!(l2_distance(&[0.5], &blob).is_none());
    }

    #[test]
    fn creates_the_embedding_tables_when_configured() {
        let conn = Connection::open_in_memory().unwrap();
        let mut index = VectorIndex::new(&enabled("local"), true);
        assert!(index.available());
        index.ensure_tables(&conn);
        assert!(index.available());
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('docs_chunk_embeddings', 'docs_vector_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn table_creation_failure_disables_the_index_with_a_reason() {
        let conn = Connection::open_in_memory().unwrap();
        // A read-only connection makes CREATE TABLE fail.
        conn.execute_batch("PRAGMA query_only = ON").unwrap();
        let mut index = VectorIndex::new(&enabled("local"), true);
        index.ensure_tables(&conn);
        assert!(!index.available());
        assert!(
            index
                .reason()
                .unwrap()
                .starts_with("Vector index initialization failed:")
        );
    }
}
