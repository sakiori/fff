// Subscribes to argv[2] in the namespace FFF_MCP_SERVICE_DIR names and returns, keeping the
// subscription: the process must still exit.
import { FileFinder } from "../dist/index.js";

const [basePath, logFilePath] = process.argv.slice(2);
const created = await FileFinder.create({ basePath, logFilePath });
if (!created.ok) {
  console.error(created.error);
  process.exit(1);
}
const watched = await created.value.watch(() => {});
if (!watched.ok) {
  console.error(watched.error);
  process.exit(1);
}
console.log("watching");
