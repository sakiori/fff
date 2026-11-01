## Why

Independent MCP sessions currently build independent indexes and watchers even
when they search the same files. A shared service should reuse that work while
preserving each client's search scope and session state.

## What changes

The normal executable becomes an automatic client of a per-user local service.
The service owns indexing, caches, and watchers. Each MCP connection has an
immutable scope, independent pagination, and its own lifecycle. Compatible
overlapping roots share an index; incompatible indexing policies remain separate.

## Capabilities

### New capabilities

- `shared-mcp-indexing`: automatic service discovery, scoped search, index reuse,
  and resource cleanup across independent clients.

### Modified capabilities

None.

## Impact

The MCP executable and internal search filtering change. Existing MCP tool names
and input schemas remain supported. The requested directory becomes the search
scope even when indexing uses a containing repository. The public SDK interfaces
remain compatible. Existing installed binaries are unaffected until deployment.
Search execution, scanning and watcher changes in the core crate apply to every
fff consumer: concurrent searches complete independently, and changes made
while an index scans are no longer lost.
