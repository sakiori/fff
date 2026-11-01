// Subscribes to argv[2] in the namespace FFF_MCP_SERVICE_DIR names, prints "watching", then
// "heard" once a change of the absolute path argv[3] arrives, and destroys the instance.
import { FileFinder } from "../dist/index.js";

const [basePath, file, logFilePath] = process.argv.slice(2);
const created = await FileFinder.create({ basePath, logFilePath });
if (!created.ok) {
  console.error(created.error);
  process.exit(1);
}
let resolveHeard;
const heard = new Promise((resolve) => {
  resolveHeard = resolve;
});
const watched = await created.value.watch((events) => {
  if (events.some((event) => event.path === file)) resolveHeard();
});
if (!watched.ok) {
  console.error(watched.error);
  process.exit(1);
}
// A ready subscription does not keep the process alive.
const alive = setInterval(() => {}, 1 << 30);
console.log("watching");
await heard;
clearInterval(alive);
console.log("heard");
await created.value.destroy();
