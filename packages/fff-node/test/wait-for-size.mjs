// Waits until the index of argv[2], in the namespace FFF_MCP_SERVICE_DIR names, holds the file
// argv[3] with argv[4] bytes, then exits.
import { FileFinder } from "../dist/index.js";

const [basePath, file, size, logFilePath] = process.argv.slice(2);
const created = await FileFinder.create({ basePath, logFilePath });
if (!created.ok) {
  console.error(created.error);
  process.exit(1);
}
const deadline = Date.now() + 30_000;
for (;;) {
  const found = await created.value.fileSearch(file, { pageSize: 5 });
  if (found.ok && found.value.items.some((item) => item.relativePath === file && item.size === Number(size))) break;
  if (Date.now() > deadline) {
    console.error(`${file} never reached ${size} bytes`);
    process.exit(1);
  }
  await new Promise((resolve) => setTimeout(resolve, 50));
}
await created.value.destroy();
