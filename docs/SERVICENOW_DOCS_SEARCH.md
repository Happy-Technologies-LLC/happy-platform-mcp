# ServiceNow Docs Search

Happy MCP can search and retrieve the official ServiceNowDocs markdown repository without depending on QMD or any user-local index.

## Modes

- **Live GitHub mode:** zero setup for family discovery and direct document fetches from `ServiceNow/ServiceNowDocs`.
- **Local sync mode:** optional SQLite FTS5 index for fast local search and offline use. Disabled by default.
- **Vector mode:** optional semantic search using sqlite-vec with deterministic local embeddings. It is disabled by default and requires local indexing.

## Configuration

```bash
HAPPY_DOCS_ENABLE_LOCAL_INDEX=false
HAPPY_DOCS_CACHE_DIR=~/.happy-platform-mcp/docs/servicenow
HAPPY_DOCS_ENABLE_VECTOR=false
HAPPY_DOCS_EMBEDDING_PROVIDER=none  # use local to enable deterministic local embeddings
HAPPY_MCP_DOCS_ONLY=false
```

The same system properties can live in `config/servicenow-instances.json`:

```json
{
  "docs": {
    "localIndexEnabled": false,
    "cacheDir": "~/.happy-platform-mcp/docs/servicenow",
    "enableVector": false,
    "embeddingProvider": "none"
  },
  "instances": []
}
```

`better-sqlite3` and `sqlite-vec` are optional npm dependencies. Live GitHub docs tools do not require them. Local sync/search requires `better-sqlite3` and `localIndexEnabled=true`. Vector search additionally requires `enableVector=true` and `embeddingProvider=local`; when sqlite-vec is unavailable, status reports the reason and FTS search continues to work.

Set `HAPPY_MCP_DOCS_ONLY=true` to expose only `SN-Docs-*` tools without ServiceNow credentials. The stdio server also falls back to docs-only mode when neither a ServiceNow config file nor ServiceNow environment credentials are present.

Docs requests never carry credentials. `GITHUB_TOKEN` and `docs.githubToken` are no longer read: the ServiceNowDocs repository is public, and an ambient GitHub token must not be sent with documentation requests. Remove those settings from docs-only deployments; private or authenticated docs retrieval is not supported.

## Source boundary

- All requests go to `https://raw.githubusercontent.com/ServiceNow/ServiceNowDocs/` with redirects rejected. There is no configurable base URL.
- Approved families and their branches come only from `main/llms.txt` in that repository (the upstream family-to-branch mapping, for example `"australia" : "australia"`). `SN-Docs-Families` shows that list. `SN-Docs-Get` and `SN-Docs-Sync` reject any family that is not listed, including the `australia` default if upstream stops listing it.
- `SN-Docs-Sync` `branch` is optional and only accepted when it equals the approved branch for the family.
- Document paths must be relative `.md` paths made of letters, digits and `._~+=,@()-` segments, at most 32 segments and 1024 characters. Requests are rejected before any cache lookup or network call if a path contains `..`/`.` segments, empty segments, percent-encoding, backslashes, control characters, a query or fragment, a scheme, or a leading `/`.
- During sync, links in a family `llms.txt` are accepted only when they are relative paths that meet the same rules, or raw links to the same repository and the same approved branch. Links to another repository, branch or origin are not fetched. The sync result counts them in `linksRejected` and lists up to 50 in `rejectedLinks`.

## Limits

| Limit | Value | When exceeded |
| --- | --- | --- |
| `main/llms.txt` or a family `llms.txt` | 1 MiB | Request fails |
| One markdown document | 8 MiB | Sync skips the document and reports why; `SN-Docs-Get` fails |
| One line in any response | 16 KiB | Same as one markdown document |
| Documents listed by a family index | 1000 | Sync fails before anything is written |
| Bytes downloaded by one sync | 64 MiB | Sync stops with `DOCS_SYNC_BUDGET` |
| One request | 30 seconds | Request fails with `DOCS_TIMEOUT` |
| One sync | 10 minutes | Sync stops with `DOCS_SYNC_DEADLINE` |

Size limits are checked against `Content-Length` and again while the response streams, before anything is written to disk, SQLite or the chunker. Content is never silently truncated. HTTP errors report only the status code, never the response body.

## Cache provenance

Each cached document records the branch it was fetched from, and `SN-Docs-Get` serves only cached documents whose branch still equals the family's recorded approved branch. If the approved branch for a family changes, the next sync purges that family's cached documents and files before downloading. A completed sync also removes documents that are no longer listed in the family index. Caches created before this change have no branch provenance, so they are discarded on first open and must be re-synced.

The cache directory holds the SQLite index (`index.sqlite` and its `-wal`/`-shm` files) and a `files/<family>/` tree of downloaded markdown. Keeping family files under `files/` means no family name can overwrite or delete the index. Older versions wrote markdown to `<cacheDir>/<family>/`; those directories are no longer used and can be deleted. Cache write failures are reported as `DOCS_CACHE_WRITE` with the docs-relative path and error code only, without local filesystem paths.

## Tools

- `SN-Docs-Families` - List available ServiceNowDocs families/releases.
- `SN-Docs-Status` - Show local cache, FTS, and vector status.
- `SN-Docs-Sync` - Download and index a docs family locally.
- `SN-Docs-Search` - Search the locally synced SQLite FTS index.
- `SN-Docs-Get` - Retrieve a markdown document from local cache or GitHub.

## Sync Example

```javascript
SN-Docs-Families({})
// Enable local indexing first: docs.localIndexEnabled=true or HAPPY_DOCS_ENABLE_LOCAL_INDEX=true
SN-Docs-Sync({ "family": "australia" })
SN-Docs-Search({ "query": "create a Flow Designer action", "family": "australia" })
SN-Docs-Get({ "family": "australia", "path": "markdown/application-development/index.md" })
```

## Notes

- Docs sync does not use ServiceNow instance credentials.
- Docs tools default to the `australia` family because the upstream ServiceNowDocs repository does not currently expose a `latest` branch.
- Docs sync skips documents that fail to download or exceed a per-document limit, reports `documentsSkipped` (with up to 50 reasons in `skippedDocuments`), and continues as long as at least one document syncs. Budget, deadline and link-count failures stop the whole sync.
- Search is local-first once a family has been synced.
- If a family is not synced, `SN-Docs-Search` returns a setup hint instead of failing hard.
- `SN-Docs-Get` falls back to GitHub raw markdown when a document is not in the local cache, and works without local indexing.
