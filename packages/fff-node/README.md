# fff - Fast File Finder

High-performance fuzzy file finder for Node.js, powered by Rust. Extremely fast live file, content, and directory search with a typo-resistant algorithm. As well as regex, plain-text, multi-occurrence and typo-resistant content search.

Comes with built-in git status support, frecency ranking, content indexing and many more! Designed for LLM agent tools that search through codebases or agentic RAG document search.

This is the Node package of the fork of fff at <https://gitlab.com/sakiori-oss/fff>. Its indexes live in the shared fff service of the current OS user, which `fff-mcp` agents of the same release use too, so a directory that an application and its agents search is indexed once.

## Installation

```bash
npm install @sakiori/fff-node
```

The native library and the `fff-mcp` service executable come in platform packages. The package depends on all of them, so every install downloads each one, and it loads the one of the running platform:

| Platform | Architecture          | Package                            |
| -------- | --------------------- | ---------------------------------- |
| macOS    | ARM64 (Apple Silicon) | `@sakiori/fff-bin-darwin-arm64`   |
| Linux    | x64 (glibc)           | `@sakiori/fff-bin-linux-x64-gnu`  |
| Linux    | x64 (musl)            | `@sakiori/fff-bin-linux-x64-musl` |

On any other platform, `FileFinder.create` fails with an error naming the supported ones. An application bundle needs only the platform package of its target.

A project that imports the upstream name `@ff-labs/fff-node` can keep its imports with a package-manager alias.

Requires Node.js 20.19 or later in the 20 line, or 22.12 and later. The package is an ES module; CommonJS `require` loads the same module, which relies on `require` of ES modules in those Node.js versions. In Electron, that takes a release whose bundled Node.js is one of them.

### Electron

Unpack the platform package's whole directory from the application archive (for example with electron-builder's `asarUnpack`). The package then loads the library and starts the service from `app.asar.unpacked`, and reports an error when only one of the two files is unpacked.

### Platforms without a prebuilt package

On a platform that has no `@sakiori/fff-bin-*` package (e.g. FreeBSD), build the C
library and the `fff-mcp` executable from source, in separate cargo invocations so
both report one build, and point `@sakiori/fff-node` at the library with `FFF_C_LIB`:

```bash
cargo build --release -p fff-mcp
cargo build --release -p fff-c
FFF_C_LIB=target/release/libfff_c.so node ./app.mjs
```

`FFF_C_LIB` is checked before the platform package and the local dev build, and
`fff-mcp` must sit beside the library. If either file does not exist, resolution
fails with an explicit error rather than silently falling back. `ffi-rs` has no
FreeBSD prebuilt either and must be built from source.

## Quick Start

Each `FileFinder` is a session on the shared service for one directory. The first one starts the service when none of this release runs; `FileFinder.create` fails with the reason when the service cannot be started or reached.

`FileFinder.create` and every method that asks the service return a Promise of a `Result` and run their native call on one of the package's worker threads, so a slow or unresponsive service blocks neither the event loop nor the thread pool that Node's file system and crypto APIs use. Calls on one instance may overlap; they reach the service one at a time, in call order. Idle workers do not keep the process alive.

```typescript
import { FileFinder } from "@sakiori/fff-node";

// Create an instance bound to a directory
const created = await FileFinder.create({ basePath: "/path/to/project" });
if (!created.ok) throw new Error(created.error);

const finder = created.value;

// Wait until the index is ready
await finder.waitForIndexReady(15_000);

// 1. Fuzzy file search (typo resistant)
const files = await finder.fileSearch("typescropt.ts", { pageSize: 10 });
if (files.ok) {
  for (const item of files.value.items) {
    console.log(item.relativePath, item.gitStatus);
  }
}

// 2. Glob filter — no fuzzy matching, 100% compatible with npm `glob`
const globbed = await finder.glob("src/**/*.ts");
if (globbed.ok) console.log(`${globbed.value.totalMatched} TypeScript files`);

// 3. Content search (live grep) with pagination
const grep = await finder.grep("TODO", { mode: "plain", pageSize: 20 });
if (grep.ok) {
  for (const m of grep.value.items) {
    console.log(`${m.relativePath}:${m.lineNumber}: ${m.lineContent}`);
  }
}

// 4. Directory search based on the query (typo resistant)
const dirs = await finder.directorySearch("components");
if (dirs.ok) console.log(dirs.value.items.map((d) => d.relativePath));

// End the session when you don't need it anymore
await finder.destroy();
```

A `FileFinder` that is never destroyed keeps its session, and the index it uses, until the process exits.

While the shared index scans, the `scannedFilesCount` of `getScanProgress` counts the files it has scanned so far, which can include files outside the instance's directory when the index covers a broader one. Once the index is ready it counts the instance's own files.

## Ranking without frecency

An index shared with agents may track frecency, which moves recently and frequently used files up. Pass `excludeFrecency: true` to `fileSearch`, `directorySearch`, `mixedSearch`, `glob`, `grep` or `multiGrep` to get the order an index without frecency gives: an empty query lists entries in path order, and grep searches the newest files first. Other clients of the index keep their ranking.

## Service namespace

Services live in a per-user namespace directory. `serviceDir` in `FileFinder.create`, or the `FFF_MCP_SERVICE_DIR` environment variable, selects another one, for example for tests. `FFF_MCP_IDLE_TIMEOUT_SECS` sets how long a service this package starts keeps running without clients; the default is one hour.

## Changes your program makes

The service's watcher picks up file changes after a short delay. A program that changes files itself and searches right after reports them, and the search sees them as soon as the report resolves:

```typescript
await writeFile(join(root, "src/new.ts"), source);
await rm(join(root, "src/old.ts"));
await finder.notifyChanged(["src/new.ts", "src/old.ts"]);
```

Paths are absolute or relative to the instance's directory and must lie inside it (`outsideScope` otherwise). When the Promise resolves, every reported file that exists is in the index unless ignored, and every one that no longer exists is gone. Report files rather than directories: only an index with a watcher walks a new directory, and later. A changed `.gitignore` or `.ignore` triggers a rescan. No rescan runs otherwise. While the index is still scanning, the changes are applied when the scan finishes.

## Watching for changes

`watch` subscribes to the changes the service's watcher sees under the instance's directory, without a watcher of your own. Its Promise resolves once every later change is delivered, to a function that ends the subscription:

```typescript
const watched = await finder.watch("src/**", (events) => {
  for (const event of events) console.log(event.kind, event.path);
}, { ignore: ["src/generated"], repository: true });
if (watched.ok) watched.value(); // unsubscribe
```

The pattern and the `ignore` entries are globs, exact paths or directories relative to the instance's directory; without a pattern the whole directory is watched. Ignored files never produce events. Batches reach the callback on the event loop with absolute paths and the kinds `created`, `modified`, `removed`, `renamed` (with `from`) and `rescan`. A `rescan`, naming the instance's directory, replaces changes that could not be delivered: when the callback falls behind, after a rescan of the index, or after the connection to the service broke, which the subscription reopens by itself. Re-read whatever you derive from the directory when one arrives. With `repository: true`, a `repository` event names the work tree whenever the git state of a repository inside the instance's directory, or containing it, changes, as `git add`, a commit or a checkout do; files under `.git` never appear as file events. A callback that falls behind by more than 1024 events gets one `rescan` for what it missed. A `failed` event, carrying `error` and `code`, ends a subscription the service can no longer serve, as after its directory was removed. Once the unsubscribe function returns, or `destroy` was called, the callback is not called again. A ready subscription does not keep the process alive, and a pending one holds none of the package's workers. On an instance created with `disableWatch`, `watch` fails with `unavailable`.

## Timeouts

A service that does not answer makes `FileFinder.create` fail after 90 seconds and each call after 30 seconds, plus the time a call asks for itself, such as a grep's `timeBudgetMs`. `timeoutMs` in `FileFinder.create` replaces both bounds. A bound starts when one of the package's four native workers runs the call, so with more instances waiting on an unresponsive service a call can first wait for a free worker; `healthCheck` makes two calls and can take up to twice the bound. A call that times out, or finds the service gone, fails with the reason, and so do the calls already waiting behind it on the same instance; the instance's next call opens a new session, starting the service again if needed, within its own bound. A busy service cannot be told from a hung one, so give a grep that may take longer than the bound a `timeBudgetMs`. `destroy` always frees the instance, also after its service was lost.

## Unavailable methods

`trackQuery`, `getHistoricalQuery` and `refreshGitStatus` have no shared service equivalent and return an error naming them as unavailable over the shared service; the service keeps the index and git status current itself. `reindex` rescans the instance's own directory; create a new instance for another directory. `historyDbPath` is not supported.

## API Reference

Verify the latest API in [`./src/finder.ts`](./src/finder.ts), results and error codes in [`./src/errors.ts`](./src/errors.ts), result and option types in [`./src/fff-api.ts`](./src/fff-api.ts) and the Node-only options in [`./src/service-options.ts`](./src/service-options.ts). Every field and type is documented.

### Result Types

Methods return a `Result<T>`, those that ask the service in a Promise. A failure carries the error text, for people, and a stable `code`, for programs:

```typescript
type Result<T> = { ok: true; value: T } | { ok: false; error: string; code: ErrorCode };

import { ErrorCode } from "@sakiori/fff-node";

let page = await finder.grep("TODO", { cursor });
if (!page.ok && page.code === ErrorCode.staleCursor) {
  // The instance moved to another index: search again from the first page.
  page = await finder.grep("TODO");
}
```

| Code                  | Meaning                                                                                      |
| --------------------- | -------------------------------------------------------------------------------------------- |
| `lostService`         | The service did not answer within the call's bound, could not be reached or started, or the connection broke. Calls already waiting on the instance fail the same way; the next call opens a new session. |
| `staleCursor`         | The grep cursor belongs to an index the instance no longer uses; search again without it.    |
| `unavailable`         | The method or option has no equivalent over the shared service.                              |
| `scopeUnavailable`    | The directory is missing or inaccessible, or it is the home directory or filesystem root without the option that allows it. |
| `invalidRequest`      | The package or the service refused the request, such as an empty glob or a cursor from elsewhere. |
| `outsideScope`        | A path given to `notifyChanged` lies outside the instance's directory.                       |
| `retry`               | The index changed under the call, as when the instance moves to a broader one; retry the call. |
| `unsupportedPlatform` | The package has no native files for the running platform.                                    |
| `libraryUnavailable`  | The native files of the running platform are missing or cannot be loaded.                    |
| `destroyed`           | The `FileFinder` was destroyed.                                                              |
| `failed`              | Any other failure.                                                                           |

The text may change between releases; branch on the code.

## Building from Source

The library and the service executable must come from one build. The build uses zlob, which needs the Zig version that `mise.toml` pins (`mise install` provides it):

```bash
git clone https://gitlab.com/sakiori-oss/fff
cd fff

# Builds both and stages them for the package's development build
make prepare-node
```

## License

MIT
