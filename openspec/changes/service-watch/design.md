## Context

A program joins the shared service with a hello line and then exchanges
length-prefixed MessagePack frames, strictly one request and one reply; the
service has no way to send a program anything unasked. Inside the service, each
index owns a debounced watcher whose subscriber API (`watch`, `unwatch`,
batches of created, modified, removed, renamed and rescan events, ignored files
filtered out) serves the in-process library. Changes under `.git` are kept from
subscribers and feed only the index's Git status worker.

## Decisions

### One connection per subscription

A subscription opens its own connection with a watch hello naming the scope,
pattern, ignore rules and whether repository state is wanted. The service
registers it as a session, so it holds a lease and follows the index when the
session's binding moves, subscribes once the watcher is ready, and replies ready;
events after that reply are guaranteed. The service then only writes event
frames, and the program closing the connection ends the subscription.

Multiplexing events onto the program session would need request identifiers, a
reader thread in the blocking session client and a rework of how a running call
detects that its program left. A long-poll request would hold one of the Node
package's few worker threads per subscription and add latency.

### Paths are relative to the scope on the wire

Index roots differ from session directories, so the service sends paths
relative to the subscription's scope and the C library joins them to the
instance's base path, giving programs the absolute paths the in-process watcher
gives.

### The core callback only hands events off

The core runs every subscriber's callback on one thread, so a blocked callback
would stall every subscriber of the index. The service's callback pushes into a
bounded queue per subscription, coalesced by path; on overflow it clears the
queue and queues one rescan. A writer task drains the queue to the connection.

### Repository state comes from the core

The core already decides which `.git` changes affect status. A subscription hook
beside the coverage hook fires with the repository root where the core marks the
Git status for a full rescan, and the service forwards it to subscriptions that
asked for repository state. A program-side watch of `.git` files would duplicate
that decision. In the C and Node interfaces it is an additional event kind, sent
only to subscriptions that requested it, so existing callers never see it.

### Reconnect belongs to the C library

When the connection drops because the service died, the C library reopens it
with backoff, starting the service as session opening does, and delivers a
rescan event, since changes in between are lost.

### Node callbacks

The Node package goes back to the process-wide threadsafe callback trampoline
upstream used for the in-process watcher, registered through the C library's
callback setter. Subscribing runs on a worker thread and resolves a Promise;
delivery runs on a native thread owned by the C library that does not keep the
process alive.
