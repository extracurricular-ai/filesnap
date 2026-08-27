// The launcher, executed.
//
// `cargo test` spawns the Rust binary directly, so nothing anywhere proved
// that the four exit codes in `crates/filesnap-cli/src/exit.rs` survive
// `bin/filesnap.js` — the wrapper every npm install actually runs. Until this
// file existed, the first execution of that script on any commit was on a
// user's machine, against a published version that can never be replaced.
//
// The staged "binary" is node itself, so a `-e` script can exit with any code
// on any platform without a shebang or a .exe to build.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LAUNCHER = path.join(HERE, "..", "bin", "filesnap.js");

// All six are staged, so nothing here asserts which triple this machine picks.
// That mapping is the launcher's to own; duplicating the choice would only let
// the two drift together.
const TRIPLES = [
  "x86_64-unknown-linux-musl",
  "aarch64-unknown-linux-musl",
  "x86_64-apple-darwin",
  "aarch64-apple-darwin",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
];

function stage({ withBinary = true } = {}) {
  const root = fs.mkdtempSync(path.join(tmpdir(), "filesnap-launcher-"));
  fs.mkdirSync(path.join(root, "bin"), { recursive: true });
  fs.copyFileSync(LAUNCHER, path.join(root, "bin", "filesnap.js"));
  if (withBinary) {
    const name = process.platform === "win32" ? "filesnap.exe" : "filesnap";
    for (const triple of TRIPLES) {
      const dir = path.join(root, "vendor", triple, "bin");
      fs.mkdirSync(dir, { recursive: true });
      const dest = path.join(dir, name);
      // Symlink, then hard link, then copy: the first needs a privilege some
      // machines withhold, and the last copies node six times.
      try {
        fs.symlinkSync(process.execPath, dest);
      } catch {
        try {
          fs.linkSync(process.execPath, dest);
        } catch {
          fs.copyFileSync(process.execPath, dest);
        }
      }
    }
  }
  return path.join(root, "bin", "filesnap.js");
}

const launcher = stage();

function run(launcherPath, ...args) {
  return spawnSync(process.execPath, [launcherPath, ...args], {
    encoding: "utf8",
  });
}

// OK / PARTIAL / FAILED / USAGE. A caller branches on these, and a wrapper
// that flattens any one of them turns a partial restore into a clean run.
for (const code of [0, 1, 2, 3]) {
  test(`exit ${code} reaches the caller unchanged`, () => {
    assert.equal(run(launcher, "-e", `process.exit(${code})`).status, code);
  });
}

test("arguments reach the binary verbatim", () => {
  // `--` ends node's own option parsing; everything after it is argv.
  const r = run(
    launcher,
    "-e",
    "process.stdout.write(process.argv.slice(1).join('|'))",
    "--",
    "--session",
    "s 1",
  );
  assert.equal(r.stdout, "--session|s 1");
});

test("a missing binary is FAILED, not a stack trace", () => {
  const r = run(stage({ withBinary: false }));
  assert.equal(r.status, 2);
  assert.match(r.stderr, /filesnap: /);
  assert.doesNotMatch(r.stderr, /at .*filesnap\.js/);
});
