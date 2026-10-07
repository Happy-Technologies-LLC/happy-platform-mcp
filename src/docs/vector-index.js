const DEFAULT_VECTOR_DIMENSIONS = 128;

function disabledVector(reason) {
  return {
    available: false,
    reason,
    async indexChunks() {},
    removeChunks() {},
    clear() {},
    search() {
      return [];
    }
  };
}

function hashToken(token) {
  let hash = 2166136261;
  for (let i = 0; i < token.length; i += 1) {
    hash ^= token.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

export function createLocalEmbedding(text, dimensions = DEFAULT_VECTOR_DIMENSIONS) {
  const vector = new Array(dimensions).fill(0);
  const tokens = String(text || '').toLowerCase().match(/[a-z0-9_]+/g) || [];

  for (const token of tokens) {
    const hash = hashToken(token);
    const index = hash % dimensions;
    vector[index] += (hash & 1) === 0 ? 1 : -1;
  }

  const magnitude = Math.hypot(...vector) || 1;
  return vector.map((value) => Number((value / magnitude).toFixed(6)));
}

export function serializeVector(vector) {
  return JSON.stringify(vector);
}

async function defaultLoadSqliteVec() {
  try {
    const module = await import('sqlite-vec');
    return module.default || module;
  } catch {
    return null;
  }
}

function embeddingFunctionForProvider(embeddingProvider, dimensions) {
  if (embeddingProvider === 'local') {
    return (text) => createLocalEmbedding(text, dimensions);
  }

  return null;
}

export async function createVectorIndex({
  enableVector,
  embeddingProvider,
  db,
  dimensions = DEFAULT_VECTOR_DIMENSIONS,
  loadSqliteVec = defaultLoadSqliteVec,
  embedText = null
} = {}) {
  if (!enableVector) {
    return disabledVector('Vector search disabled');
  }

  if (!embeddingProvider || embeddingProvider === 'none') {
    return disabledVector('Vector search requires an embedding provider');
  }

  if (!db) {
    return disabledVector('Vector search requires an initialized SQLite database');
  }

  const embed = embedText || embeddingFunctionForProvider(embeddingProvider, dimensions);
  if (!embed) {
    return disabledVector(`Unsupported embedding provider: ${embeddingProvider}`);
  }

  const sqliteVec = await loadSqliteVec();
  if (!sqliteVec?.load) {
    return disabledVector('sqlite-vec integration is not available; install optional dependency sqlite-vec');
  }

  try {
    sqliteVec.load(db);
    db.exec(`
      CREATE VIRTUAL TABLE IF NOT EXISTS docs_chunk_vectors
      USING vec0(embedding float[${dimensions}]);
    `);
  } catch (error) {
    return disabledVector(`sqlite-vec initialization failed: ${error.message}`);
  }

  return {
    available: true,
    reason: null,

    indexChunks(chunks) {
      const deleteVector = db.prepare('DELETE FROM docs_chunk_vectors WHERE _rowid_ = ?');
      const insertVector = db.prepare('INSERT INTO docs_chunk_vectors(_rowid_, embedding) VALUES (?, ?)');

      for (const chunk of chunks) {
        const body = [chunk.title, chunk.heading, chunk.body].filter(Boolean).join('\n\n');
        deleteVector.run(chunk.id);
        insertVector.run(chunk.id, serializeVector(embed(body)));
      }
    },

    removeChunks(chunkIds) {
      const deleteVector = db.prepare('DELETE FROM docs_chunk_vectors WHERE _rowid_ = ?');
      for (const id of chunkIds) deleteVector.run(id);
    },

    clear() {
      db.exec('DELETE FROM docs_chunk_vectors');
    },

    search({ query, family, limit = 10 }) {
      return db.prepare(`
        SELECT c.id, c.family, c.path, c.title, c.heading, c.start_line AS startLine,
               c.end_line AS endLine, v.distance AS vectorDistance,
               substr(c.body, 1, 240) AS snippet
        FROM docs_chunk_vectors v
        JOIN chunks c ON c.id = v.rowid
        WHERE v.embedding MATCH ?
          AND v.k = ?
          AND (? IS NULL OR c.family = ?)
        ORDER BY v.distance
      `).all(serializeVector(embed(query)), limit, family || null, family || null);
    }
  };
}
