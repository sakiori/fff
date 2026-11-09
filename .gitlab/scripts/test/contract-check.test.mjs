import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, beforeEach, describe, test } from "node:test";
import { compare, declarations, main, needed, verdict } from "../contract-check.mjs";

const root = mkdtempSync(join(tmpdir(), "contract-check-test-"));
after(() => rmSync(root, { recursive: true, force: true }));

const MANIFEST = { name: "@sakiori/fff-node", exports: { ".": "./dist/index.js" }, engines: { node: ">=22" }, os: ["linux"], cpu: ["x64"] };

describe("declarations", () => {
  test("keys members under their declarations and drops comments and imports", () => {
    const keys = declarations(
      report(`
import { Buffer } from 'node:buffer';

// @public
export interface Options {
    // (undocumented)
    limit?: number;
    nested: {
        depth: number;
    };
}

// @public
export type Location = {
    line: number;
} | {
    col: number;
};

// @public
export function open(path: string): Options;
`),
    );
    assert.deepEqual(keys, [
      "export function open(path: string): Options;",
      "export interface Options { }",
      "export interface Options { } > limit?: number;",
      "export interface Options { } > nested: { };",
      "export interface Options { } > nested: { }; > depth: number;",
      "export type Location = { line: number; } | { col: number; };",
    ]);
  });

  test("reads a report with CRLF line ends", () => {
    assert.deepEqual(declarations(report("export function a(): void;\n").replace(/\n/g, "\r\n")), ["export function a(): void;"]);
  });
});

describe("compare", () => {
  const before = contract("export interface A {\n    x: string;\n}\n\nexport interface B {\n}\n");

  test("an equal contract needs nothing", () => {
    assert.equal(needed(compare(before, contract("export interface A {\n    x: string;\n}\n\nexport interface B {\n}\n"))), null);
  });

  test("an added member needs a minor change", () => {
    const diff = compare(before, contract("export interface A {\n    x: string;\n    y?: number;\n}\n\nexport interface B {\n}\n"));
    assert.deepEqual(diff.added, ["export interface A { } > y?: number;"]);
    assert.deepEqual(diff.removed, []);
    assert.equal(needed(diff), "minor");
  });

  test("a member moved to another interface counts as removed", () => {
    const diff = compare(before, contract("export interface A {\n}\n\nexport interface B {\n    x: string;\n}\n"));
    assert.deepEqual(diff.removed, ["export interface A { } > x: string;"]);
    assert.equal(needed(diff), "major");
  });

  test("a changed member counts as removed", () => {
    const diff = compare(before, contract("export interface A {\n    x: number;\n}\n\nexport interface B {\n}\n"));
    assert.deepEqual(diff.removed, ["export interface A { } > x: string;"]);
    assert.equal(needed(diff), "major");
  });

  test("a changed manifest field needs a minor change", () => {
    const next = contract("export interface A {\n    x: string;\n}\n\nexport interface B {\n}\n", { engines: { node: ">=24" } });
    const diff = compare(before, next);
    assert.deepEqual(diff.fields, ["engines"]);
    assert.equal(needed(diff), "minor");
    assert.deepEqual(compare(before, contract(before.text, { name: "other", version: "9.9.9" })).fields, []);
  });
});

describe("verdict", () => {
  const added = { removed: [], added: ["export function more(): void;"], fields: [] };
  const removed = { removed: ["export function gone(): void;"], added: [], fields: [] };

  test("a change the commits cover passes", () => {
    assert.equal(verdict({ previous: "v1.2.0", bump: "minor", diff: added, mode: "squash" }), null);
    assert.equal(verdict({ previous: "v1.2.0", bump: "major", diff: removed, mode: "squash" }), null);
    assert.equal(verdict({ previous: "v1.2.0", bump: null, diff: { removed: [], added: [], fields: [] }, mode: "squash" }), null);
  });

  test("an addition under a fix asks for feat", () => {
    const message = verdict({ previous: "v1.2.0", bump: "patch", diff: added, mode: "squash" });
    assert.match(message, /adds declarations to v1\.2\.0's:\n {2}\+ export function more\(\): void;/);
    assert.match(message, /needs at least a minor change, but the commits since v1\.2\.0 ask for a patch change/);
    assert.match(message, /`feat: …`/);
  });

  test("an addition that asks for no release says so", () => {
    assert.match(verdict({ previous: "v1.2.0", bump: null, diff: added, mode: "squash" }), /ask for no release/);
  });

  test("a removal without a breaking change asks for ! or BREAKING CHANGE", () => {
    const message = verdict({ previous: "v1.2.0", bump: "minor", diff: removed, mode: "squash" });
    assert.match(message, /lacks declarations of v1\.2\.0's, removed or changed:\n {2}- export function gone\(\): void;/);
    assert.match(message, /needs a major change/);
    assert.match(message, /`feat!: …`.*`BREAKING CHANGE: <what breaks>` footer at the end of its description/);
  });

  test("an update asks for a commit on top of its merges", () => {
    assert.match(verdict({ previous: "v1.2.0", bump: "patch", diff: added, mode: "update" }), /commit on top of the update's merges .*`feat\(node\): …`/);
    assert.match(verdict({ previous: "v1.2.0", bump: "minor", diff: removed, mode: "update" }), /commit on top of the update's merges .*`feat\(node\)!: …`/);
  });

  test("another unsquashed merge request asks for a marking commit or squashing", () => {
    const message = verdict({ previous: "v1.2.0", bump: "patch", diff: added, mode: "merge-request" });
    assert.match(message, /Push a commit to the merge request .*`feat\(node\): …`, or tick Squash commits/);
    assert.doesNotMatch(message, /update's merges/);
  });

  test("the tip release-tag would cut names the merge request to land", () => {
    assert.match(verdict({ previous: "v1.2.0", bump: "minor", diff: removed, mode: "tip" }), /release-tag cuts no release before main marks the change/);
  });
});

describe("main", () => {
  let dir;
  let base;
  let calls;

  // A repository whose v1.0.0 has a report with one function, and a branch off main.
  beforeEach(() => {
    dir = mkdtempSync(join(root, "repo-"));
    git("init", "--quiet", "--initial-branch=main");
    write("README", "dated\n");
    commit("chore: start");
    git("tag", "v0.11.1-sakiori.20261101.1");
    write("packages/fff-node/etc/fff-node.api.md", report("export function a(): void;\n"));
    write("packages/fff-node/package.json", JSON.stringify(MANIFEST));
    commit("feat: report");
    git("tag", "v1.0.0");
    write("README", "main\n");
    commit("docs: main moves on");
    base = git("rev-parse", "HEAD");
    git("update-ref", "refs/remotes/origin/main", base);
    git("switch", "--quiet", "-c", "feature");
    calls = join(dir, "..", `${dir.split("/").pop()}.calls`);
  });

  function git(...args) {
    return execFileSync("git", args, { cwd: dir, encoding: "utf8", env: { ...process.env, ...IDENTITY } }).trim();
  }
  function write(path, text) {
    mkdirSync(join(dir, path, ".."), { recursive: true });
    writeFileSync(join(dir, path), text);
  }
  function commit(message) {
    git("add", "-A");
    git("commit", "--quiet", "-m", message);
  }

  // Runs the check with a git-intake that answers `answer` and records the commit it ran on.
  async function check(answer, extra = {}) {
    const bin = join(dir, "..", `${dir.split("/").pop()}.bin`);
    mkdirSync(bin, { recursive: true });
    writeFileSync(
      join(bin, "git-intake"),
      `#!/bin/sh\n{ echo "$*"; git rev-parse HEAD; git log -1 --format='%P%n%B'; } > "${calls}"\nprintf '%s\\n' '${typeof answer === "string" ? answer : JSON.stringify(answer)}'\n`,
    );
    chmodSync(join(bin, "git-intake"), 0o755);
    const lines = [];
    // The pipeline's own CI_ variables would change how the check lands the commit.
    const outside = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith("CI_")));
    const env = { ...outside, ...IDENTITY, PATH: `${bin}:${process.env.PATH}`, CI_MERGE_REQUEST_TARGET_BRANCH_NAME: "main", ...extra };
    await main({ env, cwd: dir, log: (line) => lines.push(line) });
    return lines.join("\n");
  }

  const squash = (title, description = "") => ({
    CI_MERGE_REQUEST_SQUASH_ON_MERGE: "true",
    CI_MERGE_REQUEST_TITLE: title,
    CI_MERGE_REQUEST_DESCRIPTION: description,
  });

  test("a squashed merge request lands its title and description on the merge base", async () => {
    write("packages/fff-node/etc/fff-node.api.md", report("export function a(): void;\n\nexport function b(): void;\n"));
    commit("wip");
    write("notes", "more\n");
    commit("wip again");
    const head = git("rev-parse", "HEAD");
    git("update-ref", "refs/remotes/origin/main", git("commit-tree", `${base}^{tree}`, "-p", base, "-m", "fix: elsewhere"));
    await assert.rejects(check({ previous: "v1.0.0", bump: "patch" }, squash("fix: b", "Adds b.\r\n")), /`feat: …`/);
    const [args, landed, parent, ...message] = readFileSync(calls, "utf8").trim().split("\n");
    assert.equal(args, "release version --json");
    assert.notEqual(landed, head);
    assert.equal(parent, base);
    assert.equal(message.join("\n"), "fix: b\n\nAdds b.");
    assert.equal(execFileSync("git", ["rev-parse", `${landed}^{tree}`], { cwd: dir, encoding: "utf8" }).trim(), git("rev-parse", "HEAD^{tree}"));
    assert.equal(git("worktree", "list").split("\n").length, 1);
  });

  test("a change the commits cover passes", async () => {
    write("packages/fff-node/etc/fff-node.api.md", report("export function a(): void;\n\nexport function b(): void;\n"));
    commit("feat: b");
    assert.match(await check({ previous: "v1.0.0", bump: "minor" }, squash("feat: b")), /by a minor change; the commits ask for a minor change/);
  });

  test("a merge request that lands unsquashed lands its head", async () => {
    write("packages/fff-node/etc/fff-node.api.md", report("export function b(): void;\n"));
    commit("sync(fff): merge");
    const update = { CI_MERGE_REQUEST_IID: "7", CI_MERGE_REQUEST_SOURCE_BRANCH_NAME: "git-intake/update" };
    await assert.rejects(check({ previous: "v1.0.0", bump: "minor" }, update), /lacks declarations[\s\S]*commit on top of the update's merges/);
    assert.equal(readFileSync(calls, "utf8").split("\n")[1], git("rev-parse", "HEAD"));
    const other = { CI_MERGE_REQUEST_IID: "8", CI_MERGE_REQUEST_SOURCE_BRANCH_NAME: "fix/thing" };
    await assert.rejects(check({ previous: "v1.0.0", bump: "minor" }, other), /Push a commit to the merge request/);
    await assert.rejects(check({ previous: "v1.0.0", bump: "minor" }), /release-tag cuts no release/);
  });

  test("a draft prefix is not part of the landed title", async () => {
    await check({ previous: null, bump: "minor" }, squash("Draft: feat: b"));
    assert.equal(readFileSync(calls, "utf8").trim().split("\n").slice(3).join("\n"), "feat: b");
  });

  test("output of git intake that is no JSON is refused", async () => {
    await assert.rejects(check("not json"), /git intake release version --json wrote no JSON object: not json/);
  });

  test("a declaration added and removed again before a release needs nothing", async () => {
    write("packages/fff-node/etc/fff-node.api.md", report("export function a(): void;\n\nexport function b(): void;\n"));
    commit("feat: b");
    write("packages/fff-node/etc/fff-node.api.md", report("export function a(): void;\n"));
    commit("fix: no b after all");
    assert.match(await check({ previous: "v1.0.0", bump: "patch" }, squash("fix: no b after all")), /is the one v1\.0\.0 released/);
  });

  test("no previous release, or one without a report, compares nothing", async () => {
    assert.match(await check({ previous: null, bump: "patch" }), /No release precedes/);
    write("packages/fff-node/etc/fff-node.api.md", report("export function b(): void;\n"));
    commit("fix: b");
    assert.match(await check({ previous: "v0.11.1-sakiori.20261101.1", bump: "patch" }), /has no API report/);
  });

  test("a git intake without the previous release and change is refused", async () => {
    await assert.rejects(check({ version: "1.0.1-rehearsal.20261109" }), /needs git intake 0\.5\.3 or later/);
  });

  test("a previous release missing from the clone is refused", async () => {
    await assert.rejects(check({ previous: "v1.1.0", bump: "patch" }), /fetch the release tags/);
  });
});

const IDENTITY = {
  GIT_AUTHOR_NAME: "test",
  GIT_AUTHOR_EMAIL: "test@invalid",
  GIT_COMMITTER_NAME: "test",
  GIT_COMMITTER_EMAIL: "test@invalid",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
};

function report(body) {
  return `## API Report File for "@sakiori/fff-node"\n\n\`\`\`ts\n\n${body}\n\`\`\`\n`;
}

function contract(body, manifest = {}) {
  return { text: body, report: report(body), manifest: { ...MANIFEST, ...manifest } };
}
