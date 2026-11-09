import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { landedTitle } from "../mr-title.mjs";

test("drops GitLab's draft prefixes, in any case and repeated", () => {
  assert.equal(landedTitle("Draft: feat: x"), "feat: x");
  assert.equal(landedTitle("draft:feat: x"), "feat: x");
  assert.equal(landedTitle("[Draft] fix(watch): y"), "fix(watch): y");
  assert.equal(landedTitle("(DRAFT) docs: z"), "docs: z");
  assert.equal(landedTitle("[draft]Draft: (Draft) feat!: w"), "feat!: w");
});

test("keeps a title without a prefix", () => {
  assert.equal(landedTitle("feat: draft: x"), "feat: draft: x");
  assert.equal(landedTitle("Drafting: x"), "Drafting: x");
  assert.equal(landedTitle("WIP: feat: x"), "WIP: feat: x");
});

test("prints the landed title", () => {
  const script = join(dirname(fileURLToPath(import.meta.url)), "..", "mr-title.mjs");
  assert.equal(execFileSync("node", [script, "Draft: feat: x"], { encoding: "utf8" }), "feat: x\n");
});
