import { spawn } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, existsSync } from "node:fs";
import { delimiter, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const [command, ...args] = process.argv.slice(2);
const env = { ...process.env };
const pathKey =
  Object.keys(env).find((key) => key.toLowerCase() === "path") || "PATH";

// Terminals opened before installing Rust may not know where Cargo lives yet.
env[pathKey] = [
  resolve(env.CARGO_HOME || resolve(homedir(), ".cargo"), "bin"),
  dirname(process.execPath),
  env[pathKey] || "",
].join(delimiter);

// A local path file lets VS Code's NPM Scripts play button work without a terminal setup.
const gamePath = resolve(root, ".local/game-path.txt");
if (command === "dev" && !env.WAW_ROOT && existsSync(gamePath)) {
  env.WAW_ROOT = readFileSync(gamePath, "utf8").trim();
}

try {
  const child = spawn(
    process.execPath,
    [resolve(root, "node_modules/@tauri-apps/cli/tauri.js"), command, ...args],
    {
      cwd: root,
      env,
      stdio: "inherit",
    },
  );

  child.on("error", (error) => {
    console.error(error.message);
    process.exitCode = 1;
  });

  child.on("exit", (code) => {
    process.exitCode = code ?? 1;
    if (code === 0 && command === "build") {
      try {
        mkdirSync(resolve(root, "release"), { recursive: true });
        copyFileSync(
          resolve(root, "src-tauri/target/release/waw-fx-viewer.exe"),
          resolve(root, "release/waw-fx-viewer.exe"),
        );
        console.log("Ready: release/waw-fx-viewer.exe");
      } catch (error) {
        console.error(`Could not copy the EXE to release: ${error.message}`);
        process.exitCode = 1;
      }
    }
  });
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}

