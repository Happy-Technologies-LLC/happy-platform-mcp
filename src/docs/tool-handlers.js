import fs from 'fs/promises';
import path from 'path';
import { assertDocsFamilyName, getDocsConfig, normalizeDocsDocumentPath } from './config.js';
import { createServiceNowDocsClient } from './github-client.js';
import { createDocsStore, getSqliteAvailability } from './sqlite-store.js';
import { syncDocsFamily } from './sync.js';
import { createVectorIndex } from './vector-index.js';

export const DEFAULT_DOCS_FAMILY = 'australia';

function jsonContent(payload) {
  return {
    content: [{
      type: 'text',
      text: JSON.stringify(payload, null, 2)
    }]
  };
}

async function withStore(config, operation) {
  await fs.mkdir(config.cacheDir, { recursive: true });
  const store = await createDocsStore(path.join(config.cacheDir, 'index.sqlite'), {
    vectorConfig: config
  });
  try {
    store.initialize();
    return await operation(store);
  } finally {
    store.close();
  }
}

export async function handleDocsTool(name, args = {}, deps = {}) {
  const config = deps.config || getDocsConfig();
  const client = deps.client || createServiceNowDocsClient();

  switch (name) {
    case 'SN-Docs-Families': {
      const families = await client.listFamilies();
      return jsonContent({ families });
    }

    case 'SN-Docs-Status': {
      const sqlite = await getSqliteAvailability();
      const vector = await createVectorIndex(config);
      if (!config.localIndexEnabled || !sqlite.available) {
        return jsonContent({
          cacheDir: config.cacheDir,
          localIndexEnabled: config.localIndexEnabled,
          ftsAvailable: false,
          sqliteAvailable: sqlite.available,
          sqliteReason: sqlite.reason || (config.localIndexEnabled ? undefined : 'Local docs index disabled'),
          vectorAvailable: vector.available,
          vectorReason: vector.reason,
          families: []
        });
      }

      return withStore(config, (store) => jsonContent({
        cacheDir: config.cacheDir,
        localIndexEnabled: config.localIndexEnabled,
        sqliteAvailable: sqlite.available,
        sqliteReason: sqlite.reason,
        ...store.status(),
        vectorAvailable: vector.available,
        vectorReason: vector.reason
      }));
    }

    case 'SN-Docs-Sync': {
      if (!config.localIndexEnabled) {
        return jsonContent({
          synced: false,
          message: 'Local ServiceNow docs indexing is disabled. Set docs.localIndexEnabled=true in config/servicenow-instances.json or HAPPY_DOCS_ENABLE_LOCAL_INDEX=true to enable SN-Docs-Sync.'
        });
      }

      const result = await syncDocsFamily({
        family: args.family || DEFAULT_DOCS_FAMILY,
        branch: args.branch,
        cacheDir: config.cacheDir,
        client,
        vectorConfig: config
      });
      return jsonContent(result);
    }

    case 'SN-Docs-Search': {
      if (!config.localIndexEnabled) {
        return jsonContent({
          query: args.query,
          family: args.family || DEFAULT_DOCS_FAMILY,
          results: [],
          message: 'Local ServiceNow docs search is disabled. Use SN-Docs-Get for direct GitHub retrieval, or set docs.localIndexEnabled=true / HAPPY_DOCS_ENABLE_LOCAL_INDEX=true and run SN-Docs-Sync.'
        });
      }

      const family = args.family || DEFAULT_DOCS_FAMILY;
      return withStore(config, (store) => {
        const results = store.search({
          query: args.query,
          family,
          limit: args.limit || 10
        });
        return jsonContent({
          query: args.query,
          family,
          results,
          message: results.length > 0
            ? 'Found locally indexed ServiceNow documentation results.'
            : 'No local docs results found. If this family has not been synced, run SN-Docs-Sync first.'
        });
      });
    }

    case 'SN-Docs-Get': {
      // Validate before any cache or network use.
      const family = assertDocsFamilyName(args.family || DEFAULT_DOCS_FAMILY);
      const documentPath = normalizeDocsDocumentPath(args.path);

      if (config.localIndexEnabled) {
        const document = await withStore(config, (store) => store.getDocument({ family, path: documentPath }));
        if (document) {
          return jsonContent({ source: 'local-cache', document });
        }
      }

      const { branch } = await client.resolveFamily(family);
      const markdown = await client.getMarkdown(family, documentPath);
      return jsonContent({
        source: 'github',
        document: {
          family,
          branch,
          path: documentPath,
          markdown
        }
      });
    }

    default:
      throw new Error(`Unknown docs tool: ${name}`);
  }
}
