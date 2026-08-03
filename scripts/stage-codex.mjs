import { chmodSync, copyFileSync, existsSync, mkdirSync, realpathSync, statSync } from "node:fs";
import { execFileSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const appRoot = path.resolve(here, "..");
const binaryDir = path.join(appRoot, "src-tauri", "binaries");

function rustTarget() {
  if (process.env.TAURI_ENV_TARGET_TRIPLE) return process.env.TAURI_ENV_TARGET_TRIPLE;
  const details = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const host = details.split("\n").find((line) => line.startsWith("host:"));
  if (!host) throw new Error("Unable to determine the Rust host target.");
  return host.slice("host:".length).trim();
}

function commandPath() {
  if (process.env.CODEX_BIN) return path.resolve(process.env.CODEX_BIN);
  const target = rustTarget();
  const platformPackage = {
    "aarch64-apple-darwin": "codex-darwin-arm64",
    "x86_64-apple-darwin": "codex-darwin-x64",
    "x86_64-pc-windows-msvc": "codex-win32-x64",
    "aarch64-pc-windows-msvc": "codex-win32-arm64"
  }[target];
  if (!platformPackage) throw new Error(`Unsupported connector target: ${target}`);

  const npmRoot = execFileSync(process.platform === "win32" ? "npm.cmd" : "npm", ["root", "-g"], { encoding: "utf8" }).trim();
  const nativeName = process.platform === "win32" ? "codex.exe" : "codex";
  const nativeCandidate = path.join(
    realpathSync(npmRoot),
    "@openai",
    "codex",
    "node_modules",
    "@openai",
    platformPackage,
    "vendor",
    target,
    "bin",
    nativeName
  );
  if (existsSync(nativeCandidate)) return nativeCandidate;

  const lookup = process.platform === "win32" ? "where.exe" : "which";
  const output = execFileSync(lookup, ["codex"], { encoding: "utf8" }).trim();
  const first = output.split(/\r?\n/).find(Boolean);
  if (!first) throw new Error("Codex CLI was not found.");
  return first;
}

const source = commandPath();
if (!existsSync(source) || !statSync(source).isFile()) {
  throw new Error(`Codex executable does not exist: ${source}`);
}
if (process.platform === "win32" && path.extname(source).toLowerCase() !== ".exe") {
  throw new Error("A native codex.exe is required when building on Windows.");
}

const extension = process.platform === "win32" ? ".exe" : "";
const destination = path.join(binaryDir, `codex-${rustTarget()}${extension}`);
mkdirSync(binaryDir, { recursive: true });
copyFileSync(source, destination);
if (process.platform !== "win32") chmodSync(destination, 0o755);

console.log(`Staged Codex ${execFileSync(source, ["--version"], { encoding: "utf8" }).trim()}`);
console.log(`Source: ${source}`);
console.log(`Bundle: ${destination}`);
