#!/usr/bin/env node
// Copies the release `msmd` build into src-tauri/binaries/ under the name Tauri's sidecar
// bundling expects (`msmd-<target triple>[.exe]`). Run after `cargo build --release -p msmd`.
//   node scripts/stage-sidecar.mjs [path/to/target/dir]
import { execSync } from "node:child_process";
import { copyFileSync, mkdirSync, existsSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..", "..");
const target = resolve(process.argv[2] ?? process.env.CARGO_TARGET_DIR ?? join(root, "target"));
const triple = execSync("rustc -vV", { encoding: "utf8" }).match(/^host: (\S+)$/m)[1];
const ext = process.platform === "win32" ? ".exe" : "";
const from = join(target, "release", `msmd${ext}`);
if (!existsSync(from)) {
  console.error(`${from} not found; run: cargo build --release -p msmd`);
  process.exit(1);
}
const dir = join(here, "..", "src-tauri", "binaries");
mkdirSync(dir, { recursive: true });
copyFileSync(from, join(dir, `msmd-${triple}${ext}`));
console.log(`staged ${from} -> binaries/msmd-${triple}${ext}`);
