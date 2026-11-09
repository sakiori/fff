// The title a merge request lands with: GitLab drops its draft prefixes when it is marked ready.
//   node .gitlab/scripts/mr-title.mjs <title>   (prints the title without them)
import { pathToFileURL } from "node:url";

// GitLab's draft prefixes, which it strips one after another, at most 20 times.
const DRAFT = /^(?:\[draft\]|\(draft\)|draft:)+\s*/i;

export function landedTitle(title) {
  for (let i = 0; i < 20; i++) {
    const next = title.replace(DRAFT, "");
    if (next === title) break;
    title = next;
  }
  return title;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  if (process.argv.length !== 3) {
    console.error("usage: mr-title.mjs <title>");
    process.exit(1);
  }
  console.log(landedTitle(process.argv[2]));
}
