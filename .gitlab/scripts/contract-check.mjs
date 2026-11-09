// Fails when the change the commits since the previous release mark is smaller than the change of
// @sakiori/fff-node's contract since then, for the commit a merge request lands or else HEAD.
//   node .gitlab/scripts/contract-check.mjs   (CUSTOM.md, npm versions)
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { landedTitle } from "./mr-title.mjs";

const REPORT = "packages/fff-node/etc/fff-node.api.md";
const MANIFEST = "packages/fff-node/package.json";
const FIELDS = ["exports", "engines", "os", "cpu"];
const RANK = { patch: 1, minor: 2, major: 3 };
const UPDATE_BRANCH = "git-intake/update";
const IDENTITY = {
  GIT_AUTHOR_NAME: "contract-check",
  GIT_AUTHOR_EMAIL: "contract-check@invalid",
  GIT_COMMITTER_NAME: "contract-check",
  GIT_COMMITTER_EMAIL: "contract-check@invalid",
};

class Failure extends Error {}

function fail(message) {
  throw new Failure(message);
}

// The declarations of an API report, each member prefixed with the declarations containing it.
export function declarations(report) {
  const block = /^```ts\n([\s\S]*?)^```$/m.exec(report.replace(/\r\n/g, "\n"));
  if (!block) fail("The API report holds no TypeScript block.");
  const lines = block[1].split("\n").filter((line) => line.trim() !== "" && !line.trim().startsWith("//"));
  const keys = new Set();
  for (const statement of statements(lines)) {
    if (/^(import|export \{\s*\};?$)/.test(statement[0].trim())) continue;
    expand(statement, "", keys);
  }
  return [...keys].sort();
}

export function compare(previous, next) {
  const before = new Set(declarations(previous.report));
  const after = new Set(declarations(next.report));
  return {
    removed: [...before].filter((key) => !after.has(key)),
    added: [...after].filter((key) => !before.has(key)),
    fields: FIELDS.filter(
      (field) => JSON.stringify(previous.manifest[field] ?? null) !== JSON.stringify(next.manifest[field] ?? null),
    ),
  };
}

export function needed(diff) {
  if (diff.removed.length > 0) return "major";
  return diff.added.length > 0 || diff.fields.length > 0 ? "minor" : null;
}

// How to mark a change in each way a commit reaches main: a squashed merge request, git intake's
// update, another merge request that lands unsquashed, and the tip that release-tag cuts.
const ADVICE = {
  squash: {
    major: "Mark the breaking change in the merge request: `!` before the title's colon (`feat!: …`) or a `BREAKING CHANGE: <what breaks>` footer at the end of its description.",
    minor: "Title the merge request as a feature: `feat: …`.",
  },
  update: {
    major: "Push a commit on top of the update's merges that marks the breaking change: `!` before its colon (`feat(node)!: …`) or a `BREAKING CHANGE: <what breaks>` footer.",
    minor: "Push a commit on top of the update's merges that marks the addition: `feat(node): …`.",
  },
  "merge-request": {
    major: "Push a commit to the merge request that marks the breaking change, `!` before its colon (`feat(node)!: …`) or a `BREAKING CHANGE: <what breaks>` footer, or tick Squash commits and mark it in the title or description.",
    minor: "Push a commit to the merge request that marks the addition, `feat(node): …`, or tick Squash commits and title the merge request `feat: …`.",
  },
  tip: {
    major: "release-tag cuts no release before main marks the change: land a merge request that marks it breaking, `!` before its title's colon (`feat!: …`) or a `BREAKING CHANGE: <what breaks>` footer in its description.",
    minor: "release-tag cuts no release before main marks the change: land a merge request titled as a feature, `feat: …`.",
  },
};

// Null when the commits' change covers the contract's, otherwise why not and how to mark it.
export function verdict({ previous, bump, diff, mode }) {
  const need = needed(diff);
  if ((RANK[bump] ?? 0) >= (RANK[need] ?? 0)) return null;
  const asked = bump ? `a ${bump} change` : "no release";
  const lines = [];
  if (diff.removed.length > 0) {
    lines.push(`The contract of @sakiori/fff-node lacks declarations of ${previous}'s, removed or changed:`);
    lines.push(...diff.removed.map((key) => `  - ${key}`));
  }
  if (diff.added.length > 0) {
    lines.push(`The contract of @sakiori/fff-node adds declarations to ${previous}'s:`);
    lines.push(...diff.added.map((key) => `  + ${key}`));
  }
  if (diff.fields.length > 0) lines.push(`The manifest's ${diff.fields.join(", ")} differ from ${previous}'s.`);
  lines.push(
    need === "major"
      ? `That needs a major change, but the commits since ${previous} ask for ${asked}.`
      : `That needs at least a minor change, but the commits since ${previous} ask for ${asked}.`,
  );
  lines.push(ADVICE[mode][need]);
  return lines.join("\n");
}

export async function main({ env = process.env, cwd = process.cwd(), log = console.log } = {}) {
  const git = (args, options = {}) =>
    execFileSync("git", args, { cwd, encoding: "utf8", stdio: ["pipe", "pipe", "pipe"], ...options }).trim();
  const has = (spec) => {
    try {
      git(["cat-file", "-e", spec]);
      return true;
    } catch {
      return false;
    }
  };

  const squash = env.CI_MERGE_REQUEST_SQUASH_ON_MERGE === "true";
  const mode = squash
    ? "squash"
    : !env.CI_MERGE_REQUEST_IID
      ? "tip"
      : env.CI_MERGE_REQUEST_SOURCE_BRANCH_NAME === UPDATE_BRANCH
        ? "update"
        : "merge-request";
  let commit;
  if (squash) {
    const title = landedTitle(env.CI_MERGE_REQUEST_TITLE ?? "");
    if (!title) fail("CI_MERGE_REQUEST_TITLE is not set; a squashed merge request lands with its title.");
    const description = (await mergeRequestDescription(env)).replace(/\r\n/g, "\n").trim();
    const target = `origin/${env.CI_MERGE_REQUEST_TARGET_BRANCH_NAME || "main"}`;
    if (!has(`${target}^{commit}`)) fail(`${target} is not in the clone; fetch the merge request's target branch.`);
    const base = git(["merge-base", "HEAD", target]);
    commit = git(["commit-tree", "HEAD^{tree}", "-p", base, "-F", "-"], {
      input: description ? `${title}\n\n${description}\n` : `${title}\n`,
      env: { ...env, ...IDENTITY },
    });
  } else {
    commit = git(["rev-parse", "HEAD^{commit}"]);
  }

  const { previous, bump } = intake(commit, git, env);
  if (previous === null) {
    log("No release precedes the landed commit; there is no contract to compare with.");
    return;
  }
  if (!has(`${previous}^{commit}`)) {
    fail(`The release ${previous} is not in the clone; fetch the release tags (git fetch origin '+refs/tags/v*:refs/tags/v*').`);
  }
  if (!has(`${previous}:${REPORT}`)) {
    log(`${previous} has no API report; there is no contract to compare with.`);
    return;
  }
  if (!has(`${commit}:${REPORT}`)) fail(`The landed commit has no ${REPORT}; run make node-api.`);
  const contract = (rev) => ({
    report: git(["show", `${rev}:${REPORT}`]),
    manifest: has(`${rev}:${MANIFEST}`) ? JSON.parse(git(["show", `${rev}:${MANIFEST}`])) : {},
  });
  const diff = compare(contract(previous), contract(commit));
  const problem = verdict({ previous, bump, diff, mode });
  if (problem) fail(problem);
  const need = needed(diff);
  log(
    need
      ? `The contract changed since ${previous} by a ${need} change; the commits ask for a ${bump} change, which covers it.`
      : `The contract is the one ${previous} released.`,
  );
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  try {
    await main();
  } catch (error) {
    console.error(error instanceof Failure ? error.message : error);
    process.exitCode = 1;
  }
}

// git intake's previous release and next change for the commit, run in a worktree checked out at it.
function intake(commit, git, env) {
  const dir = mkdtempSync(join(tmpdir(), "contract-check-"));
  const tree = join(dir, "tree");
  try {
    removeStaleTrees(git);
    git(["worktree", "add", "--detach", "--quiet", tree, commit]);
    // Without a tag, git intake versions the checked-out commit.
    const { CI_COMMIT_TAG: _, ...untagged } = env;
    let output;
    try {
      output = execFileSync("git", ["intake", "release", "version", "--json"], {
        cwd: tree,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "inherit"],
        env: untagged,
      });
    } catch {
      fail("git intake release version failed on the landed commit.");
    }
    let answer;
    try {
      answer = JSON.parse(output);
    } catch {}
    if (typeof answer !== "object" || answer === null) {
      fail(`git intake release version --json wrote no JSON object: ${output.trim().slice(0, 500)}`);
    }
    if (!("previous" in answer) || !("bump" in answer)) {
      fail("git intake release version --json reports no previous release or change; the check needs git intake 0.5.3 or later.");
    }
    if (answer.previous !== null && typeof answer.previous !== "string") fail(`git intake reports the previous release ${JSON.stringify(answer.previous)}.`);
    if (answer.bump !== null && !(answer.bump in RANK)) fail(`git intake reports the change ${JSON.stringify(answer.bump)}.`);
    return answer;
  } finally {
    try {
      git(["worktree", "remove", "--force", tree]);
    } catch {}
    rmSync(dir, { recursive: true, force: true });
  }
}

// The merge request's description; GitLab truncates the variable, so a long one comes from the API.
async function mergeRequestDescription(env) {
  if (env.CI_MERGE_REQUEST_DESCRIPTION_IS_TRUNCATED !== "true") return env.CI_MERGE_REQUEST_DESCRIPTION ?? "";
  const url = `${env.CI_API_V4_URL}/projects/${env.CI_PROJECT_ID}/merge_requests/${env.CI_MERGE_REQUEST_IID}`;
  const response = await fetch(url, { headers: { accept: "application/json" } });
  if (!response.ok) fail(`The description is truncated, and GET ${url} answered ${response.status}.`);
  return (await response.json()).description ?? "";
}

// Groups lines into statements that end where their brackets balance.
function statements(lines) {
  const out = [];
  let current = [];
  let depth = 0;
  for (const line of lines) {
    current.push(line);
    for (const change of brackets(line)) depth += change;
    if (depth <= 0) {
      out.push(current);
      current = [];
      depth = 0;
    }
  }
  if (current.length > 0) out.push(current);
  return out;
}

// A block `<header> {` … `}<rest>` that its first brace encloses is a container: its own key, and
// its statements keyed under it. Anything else, a union of object types among them, is one key.
function expand(lines, prefix, keys) {
  const first = lines[0].trim();
  const last = lines.at(-1).trim();
  if (lines.length > 1 && first.endsWith("{") && last.startsWith("}") && encloses(lines)) {
    const key = `${prefix}${normalize(first)} ${normalize(last)}`;
    keys.add(key);
    for (const statement of statements(lines.slice(1, -1))) expand(statement, `${key} > `, keys);
    return;
  }
  keys.add(prefix + normalize(lines.join(" ")));
}

function encloses(lines) {
  let depth = 0;
  for (const [index, line] of lines.entries()) {
    for (const change of brackets(line)) {
      depth += change;
      if (depth === 0 && index < lines.length - 1) return false;
    }
  }
  return true;
}

// +1 or -1 for each bracket outside string literals.
function brackets(line) {
  const changes = [];
  let quote = null;
  for (let i = 0; i < line.length; i++) {
    const char = line[i];
    if (quote) {
      if (char === "\\") i++;
      else if (char === quote) quote = null;
    } else if (char === '"' || char === "'" || char === "`") quote = char;
    else if ("{([".includes(char)) changes.push(1);
    else if ("})]".includes(char)) changes.push(-1);
  }
  return changes;
}

function normalize(text) {
  return text.replace(/\s+/g, " ").trim();
}

// Drops the registrations of this check's own trees whose job was killed, and no other.
function removeStaleTrees(git) {
  for (const line of git(["worktree", "list", "--porcelain"]).split("\n")) {
    if (!line.startsWith("worktree ")) continue;
    const path = line.slice("worktree ".length);
    if (basename(path) === "tree" && basename(dirname(path)).startsWith("contract-check-") && !existsSync(path)) {
      git(["worktree", "remove", "--force", path]);
    }
  }
}
