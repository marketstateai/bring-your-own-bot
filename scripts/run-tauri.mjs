import { existsSync } from "node:fs";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const appRoot = path.resolve(here, "..");
const candidates = [
  path.join(appRoot, "node_modules", "@tauri-apps", "cli", "tauri.js"),
  path.join(appRoot, "..", "orama", "node_modules", "@tauri-apps", "cli", "tauri.js")
];
const cli = candidates.find(existsSync);

if (!cli) {
  console.error("Tauri CLI is unavailable. Run npm install in this repository.");
  process.exit(1);
}

const child = spawn(process.execPath, [cli, ...process.argv.slice(2)], {
  cwd: appRoot,
  stdio: "inherit"
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  process.exit(code ?? 1);
});
