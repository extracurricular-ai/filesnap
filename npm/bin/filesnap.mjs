#!/usr/bin/env node
// The `filesnap` command, as npm installs it.
//
// This file exists to find one binary and get out of the way. The six
// platform builds are published as six *versions of this same npm name*
// (D37), named from here through alias specs in `optionalDependencies`, and
// `os`/`cpu` on each of them is what makes npm unpack exactly one.
//
// **`.mjs`, not `.js`.** As `.js` this file is an ES module only because the
// package.json beside it says `"type": "module"` — a dependency on a sibling
// that is invisible from here, and one node 18 enforces strictly while node 22
// and later paper over with ES-module detection. The extension says what the
// file is, so nothing has to be true elsewhere for it to load.
//
// Everything below is either locating that binary or forwarding a signal to
// it. Nothing here parses arguments or interprets output: the contract is the
// binary's (JSON Lines on stdout, prose on stderr, exit codes 0/1/2/3), and a
// launcher that inspected either would be a second place for it to drift.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

// Linux is musl deliberately. A glibc build carries a symbol-version floor set
// by whatever machine produced it, which would break older distributions and
// containers — the one failure an npm-installed binary cannot help its user
// debug. filesnap has no C dependency, so the static build costs nothing; it
// is marginally *smaller* than the glibc one.
const TRIPLE_BY_PLATFORM = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
  "win32-arm64": "aarch64-pc-windows-msvc",
};

/**
 * Exit the way the binary would have.
 *
 * `2` is the CLI's own "it did not run, or could not report" (D40). A launcher
 * that cannot find its binary is exactly that, seen one level up — so a caller
 * checking exit codes does not need to know whether it was the wrapper or the
 * program that failed. `throw` would print a stack trace, which tells a user
 * nothing they can act on.
 */
function fail(message) {
  process.stderr.write(`filesnap: ${message}\n`);
  process.exit(2);
}

/** How this user would reinstall, so the error names their own tool. */
function installCommand() {
  const agent = process.env.npm_config_user_agent || "";
  if (agent.startsWith("pnpm")) return "pnpm add -g filesnap";
  if (agent.startsWith("yarn")) return "yarn global add filesnap";
  if (/\bbun\//.test(agent)) return "bun install -g filesnap";
  return "npm install -g filesnap";
}

// Android runs the Linux builds.
const platform = process.platform === "android" ? "linux" : process.platform;
const slug = `${platform}-${process.arch}`;
const triple = TRIPLE_BY_PLATFORM[slug];
if (!triple) {
  fail(
    `there is no build for ${process.platform} ${process.arch}. ` +
      `Install from source instead: cargo install filesnap-cli`,
  );
}

const platformPackage = `filesnap-${slug}`;

function locateBinary() {
  let vendorRoot;
  try {
    // Ask the resolver rather than guessing at a path: pnpm, Yarn PnP and
    // hoisting all put the platform package somewhere different, and every
    // one of them can answer this.
    const manifest = require.resolve(`${platformPackage}/package.json`);
    vendorRoot = path.join(path.dirname(manifest), "vendor");
  } catch {
    // No resolver answer: a tarball someone unpacked by hand, or a bundler
    // that flattened node_modules. Fall back to this package's own tree.
    vendorRoot = path.join(here, "..", "vendor");
  }

  const binary = path.join(
    vendorRoot,
    triple,
    "bin",
    platform === "win32" ? "filesnap.exe" : "filesnap",
  );
  return existsSync(binary) ? binary : null;
}

const binary = locateBinary();
if (!binary) {
  fail(
    `the ${platformPackage} package is missing or has no binary. ` +
      `Reinstall with \`${installCommand()}\`.`,
  );
}

// `spawn`, not `spawnSync`: a synchronous spawn makes Node deaf to signals for
// the whole run, so a Ctrl-C during a capture over a large project would be
// swallowed and the user would watch it keep going.
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

const FORWARDED = ["SIGINT", "SIGTERM", "SIGHUP"];
const handlers = new Map();
for (const signal of FORWARDED) {
  const handler = () => {
    if (child.exitCode === null && !child.killed) child.kill(signal);
  };
  try {
    process.on(signal, handler);
    handlers.set(signal, handler);
  } catch {
    // Windows does not raise every one of these. Not being able to forward a
    // signal that cannot arrive is not a problem worth reporting.
  }
}

child.on("error", (err) => fail(`could not run ${binary}: ${err.message}`));

child.on("exit", (code, signal) => {
  for (const [name, handler] of handlers) process.off(name, handler);
  if (signal) {
    // Die of the same signal rather than translating it to a number, so a
    // shell reports "terminated" and a supervising process sees what happened.
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 2);
});
