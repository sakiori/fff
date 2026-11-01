// Holds a FileFinder for argv[2] in the namespace FFF_MCP_SERVICE_DIR names until stdin ends.
import { FileFinder } from "../dist/index.js";

const [basePath, logFilePath] = process.argv.slice(2);
const created = await FileFinder.create({ basePath, logFilePath });
if (!created.ok) {
  console.error(created.error);
  process.exit(1);
}
const ready = await created.value.waitForIndexReady(30_000);
if (!ready.ok || !ready.value) {
  console.error(`index not ready: ${JSON.stringify(ready)}`);
  process.exit(1);
}
console.log("ready");
process.stdin.resume();
process.stdin.on("end", async () => {
  await created.value.destroy();
  process.exit(0);
});
