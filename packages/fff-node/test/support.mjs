import { execFileSync } from "node:child_process";
import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findServiceExecutable } from "../dist/index.js";

// Tests never reach the per-user namespace or state: defaults resolve below a
// temporary state directory, and services the tests start exit soon when orphaned.
const stateRoot = realpathSync(mkdtempSync(join(tmpdir(), "fff-node-state-")));
process.env.XDG_STATE_HOME = stateRoot;
process.env.FFF_MCP_IDLE_TIMEOUT_SECS = "60";
delete process.env.FFF_MCP_SERVICE_DIR;

/**
 * A service namespace of its own, with its log beside it. `cleanup` stops its
 * services and removes it, so a service that refused to stop exits as well.
 */
export function disposableNamespace() {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "fff-ns-")));
  const dir = join(root, "svc");
  const log = join(root, "service.log");
  const cli = (flag) =>
    execFileSync(
      findServiceExecutable(),
      [flag, "--service-dir", dir, "--log-file", join(root, "cli.log")],
      { encoding: "utf8" },
    );
  return {
    root,
    dir,
    log,
    /** Options that keep a FileFinder in this namespace. */
    options: { serviceDir: dir, logFilePath: log },
    status: () => JSON.parse(cli("--service-status")),
    cleanup() {
      try {
        cli("--service-stop");
      } catch {
        // Removing the namespace below ends a service that still has clients.
      }
      rmSync(root, { recursive: true, force: true });
    },
  };
}

export function removeStateRoot() {
  rmSync(stateRoot, { recursive: true, force: true });
}
