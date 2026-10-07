import { createVectorIndex } from './vector-index.js';

let databaseModulePromise;

async function loadBetterSqlite3() {
  if (!databaseModulePromise) {
    databaseModulePromise = import('better-sqlite3');
  }

  try {
    const module = await databaseModulePromise;
    return module.default || module;
  } catch (error) {
    throw new Error(
      `Local ServiceNow docs search requires optional dependency better-sqlite3. ` +
      `Install optional dependencies or run npm install better-sqlite3 to enable SN-Docs-Sync and SN-Docs-Search. Original error: ${error.message}`
    );
  }
}

export async function getSqliteAvailability() {
  try {
    await loadBetterSqlite3();
    return { available: true };
  } catch (error) {
    return { available: false, reason: error.message };
  }
}

const SCHEMA = `
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
`;

export async function createDocsStore(dbPath, { Database = null, vectorConfig = null } = {}) {
  const DatabaseCtor = Database || await loadBetterSqlite3();
  const db = new DatabaseCtor(dbPath);
  let vectorIndex;
  try {
    vectorIndex = await createVectorIndex({
      ...(vectorConfig || {}),
      db
    });
  } catch (error) {
    db.close();
    throw error;
  }

  function hasTable(name) {
    return Boolean(db.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?").get(name));
  }

  // Caches written before branch provenance existed cannot prove which ref
  // their content came from; discard them instead of serving them. Rows are
  // deleted (not tables dropped) so AUTOINCREMENT ids are never reused by
  // any vector rows left behind.
  function discardLegacyCache() {
    if (!hasTable('documents')) return;
    const columns = db.prepare('PRAGMA table_info(documents)').all().map((column) => column.name);
    if (columns.includes('branch')) return;
    db.transaction(() => {
      if (hasTable('chunks_fts')) db.exec("INSERT INTO chunks_fts(chunks_fts) VALUES('delete-all')");
      if (hasTable('chunks')) db.exec('DELETE FROM chunks');
      db.exec('DELETE FROM documents');
      db.exec("ALTER TABLE documents ADD COLUMN branch TEXT NOT NULL DEFAULT ''");
      if (hasTable('families')) db.exec('DELETE FROM families');
      vectorIndex.clear();
    })();
  }

  function removeDocument(documentId) {
    const chunkIds = db.prepare('SELECT id FROM chunks WHERE document_id = ?').all(documentId).map((row) => row.id);
    db.prepare('DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE document_id = ?)').run(documentId);
    vectorIndex.removeChunks(chunkIds);
    db.prepare('DELETE FROM chunks WHERE document_id = ?').run(documentId);
  }

  function deleteDocument(documentId) {
    removeDocument(documentId);
    db.prepare('DELETE FROM documents WHERE id = ?').run(documentId);
  }

  return {
    initialize() {
      db.exec(`
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
      `);
      discardLegacyCache();
      db.exec(SCHEMA);
    },

    /**
     * Records the approved ref for a family before any document is written.
     * Content cached from any other ref is purged; returns whether it was.
     */
    beginFamilySync({ name, branch }) {
      return db.transaction(() => {
        const existing = db.prepare('SELECT branch FROM families WHERE name = ?').get(name);
        const stale = db.prepare('SELECT id FROM documents WHERE family = ? AND branch <> ?').all(name, branch);
        for (const row of stale) deleteDocument(row.id);
        db.prepare(`
          INSERT INTO families (name, branch, synced_at)
          VALUES (?, ?, NULL)
          ON CONFLICT(name) DO UPDATE SET
            branch = excluded.branch,
            synced_at = CASE WHEN families.branch = excluded.branch THEN families.synced_at ELSE NULL END
        `).run(name, branch);
        return { invalidated: stale.length > 0 || Boolean(existing && existing.branch !== branch) };
      })();
    },

    replaceDocument(document, chunks) {
      const tx = db.transaction(() => {
        const family = db.prepare('SELECT branch FROM families WHERE name = ?').get(document.family);
        if (!family || family.branch !== document.branch) {
          throw new Error(`Docs cache provenance mismatch for family ${document.family}`);
        }
        const existing = db.prepare('SELECT id FROM documents WHERE family = ? AND path = ?').get(document.family, document.path);
        if (existing) {
          removeDocument(existing.id);
          db.prepare('UPDATE documents SET branch = ?, sha = ?, title = ?, markdown = ? WHERE id = ?')
            .run(document.branch, document.sha, document.title, document.markdown, existing.id);
        } else {
          db.prepare('INSERT INTO documents (family, branch, path, sha, title, markdown) VALUES (?, ?, ?, ?, ?, ?)')
            .run(document.family, document.branch, document.path, document.sha, document.title, document.markdown);
        }

        const row = db.prepare('SELECT id FROM documents WHERE family = ? AND path = ?').get(document.family, document.path);
        const insertChunk = db.prepare(`
          INSERT INTO chunks (document_id, family, path, title, heading, start_line, end_line, body)
          VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        `);
        const insertFts = db.prepare('INSERT INTO chunks_fts (rowid, title, heading, body) VALUES (?, ?, ?, ?)');

        const indexedChunks = [];
        for (const chunk of chunks) {
          const result = insertChunk.run(
            row.id,
            chunk.family,
            chunk.path,
            chunk.title,
            chunk.heading,
            chunk.startLine,
            chunk.endLine,
            chunk.body
          );
          const indexedChunk = {
            ...chunk,
            id: Number(result.lastInsertRowid)
          };
          indexedChunks.push(indexedChunk);
          insertFts.run(indexedChunk.id, chunk.title, chunk.heading, chunk.body);
        }
        vectorIndex.indexChunks(indexedChunks);
      });

      tx();
    },

    /**
     * Marks a completed sync and removes documents no longer listed by the
     * family index. Returns the removed document paths.
     */
    completeFamilySync({ name, branch, syncedAt, keepPaths }) {
      return db.transaction(() => {
        const keep = new Set(keepPaths);
        const removed = [];
        for (const row of db.prepare('SELECT id, path FROM documents WHERE family = ?').all(name)) {
          if (keep.has(row.path)) continue;
          deleteDocument(row.id);
          removed.push(row.path);
        }
        db.prepare('UPDATE families SET synced_at = ? WHERE name = ? AND branch = ?').run(syncedAt, name, branch);
        return removed;
      })();
    },

    search({ query, family, limit = 10 }) {
      const ftsResults = () => db.prepare(`
        SELECT c.id, c.family, c.path, c.title, c.heading, c.start_line AS startLine,
               c.end_line AS endLine, snippet(chunks_fts, 2, '<mark>', '</mark>', '...', 20) AS snippet
        FROM chunks_fts
        JOIN chunks c ON c.id = chunks_fts.rowid
        WHERE chunks_fts MATCH ?
          AND (? IS NULL OR c.family = ?)
        ORDER BY rank
        LIMIT ?
      `).all(query, family || null, family || null, limit);

      if (!vectorIndex.available) {
        return ftsResults();
      }

      const vectorResults = vectorIndex.search({ query, family, limit });
      const seen = new Set(vectorResults.map((result) => result.id));
      const merged = [...vectorResults];
      for (const result of ftsResults()) {
        if (!seen.has(result.id)) {
          merged.push(result);
        }
      }
      return merged.slice(0, limit);
    },

    // Only content whose recorded ref still equals the family's approved ref.
    getDocument({ family, path }) {
      return db.prepare(`
        SELECT d.family, d.branch, d.path, d.title, d.markdown
        FROM documents d
        JOIN families f ON f.name = d.family AND f.branch = d.branch
        WHERE d.family = ? AND d.path = ?
      `).get(family, path);
    },

    status() {
      const families = db.prepare('SELECT name, branch, synced_at AS syncedAt FROM families ORDER BY name').all();
      return {
        dbPath,
        ftsAvailable: true,
        vectorAvailable: vectorIndex.available,
        vectorReason: vectorIndex.reason,
        families
      };
    },

    close() {
      db.close();
    }
  };
}
