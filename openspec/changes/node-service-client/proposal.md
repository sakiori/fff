## Why

Applications that embed fff through the Node package build their own index in
their own process. An application that also runs coding agents then indexes the
same directory twice: once in-process for its own search, and once in the shared
service the agents' `fff-mcp` clients use. Each index carries its own scan,
watcher and caches.

The shared service already owns indexes for every MCP client of the same OS
user. Programs should be able to use those indexes as well, so that one
directory is indexed once for agents and applications alike.

## What changes

The shared service gains a structured client mode beside MCP. A program opens a
session for a directory and calls file, directory, mixed and glob searches,
content grep with cursors, readiness waits and rescans, and receives structured
results instead of text written for agents.

The service client moves out of the `fff-mcp` executable into a library that
both `fff-mcp` and the C library use. The Node package's `FileFinder` connects
through it to the shared service, starting the service when none runs, and keeps
its existing method signatures. The platform packages carry the service
executable beside the native library.

Two service rules change so that applications and agents actually meet on one
index:

- Index reuse follows what a request needs rather than exact option equality.
  Options that decide which files an index holds must match; capabilities such
  as content indexing and watching may exceed the request; cache settings belong
  to the index, and whether frecency counts in ranking is chosen per request.
- A service is identified by a build identity compiled into the executable,
  not by the bytes of the file, so a copy re-signed inside an application meets
  the same release installed elsewhere.

The fork's release pipeline publishes the Node package and its platform
packages to npmjs.org as `@sakiori/fff-node` and `@sakiori/fff-bin-<platform>`
for macOS on arm64 and x64 and Linux on x64 and arm64 with glibc and musl. The
platform packages are optional dependencies that declare their platform, so an
install brings only the running platform's one.

## Capabilities

### New capabilities

- `service-clients`: structured program sessions on the shared service and the
  Node package that uses them.

### Modified capabilities

- `shared-mcp-indexing`: build identity and capability-based index reuse.

## Impact

- `fff-mcp`: the protocol version increases; services and clients of different
  builds keep running side by side as before. Index reuse rules change for MCP
  clients too.
- `fff-c`: new functions create and use service-backed instances. The existing
  in-process functions remain for other bindings.
- `@sakiori/fff-node`, the fork's Node package: `FileFinder` always uses the
  shared service. Methods without a service equivalent (`watch`, query history
  and tracking) fail with an explicit error. Applications ship the service
  executable from the platform package; Electron applications need no patches
  to native-library lookup.
- Consumers pin a fork release and may alias it to the upstream package name. A
  consumer and an agent installation share a service only when they run the
  same release.
- Windows keeps the existing service code but gets no published package.
