#!/usr/bin/env node
// Stage one npm package and print the tarball it packed.
//
//   node npm/build.mjs --package filesnap            --version 0.2.0
//   node npm/build.mjs --package filesnap-linux-x64  --version 0.2.0 --vendor-src dist
//
// **Seven packages, one npm name.** All seven publish as `filesnap`,
// distinguished by a version suffix — `0.2.0` for the launcher and
// `0.2.0-linux-x64` for each platform build (D37). The hyphenated names above
// exist only as alias keys in the launcher's `optionalDependencies`; nothing
// by those names is ever published.
//
// The two things this generates rather than checking in are the two that
// cannot be written down ahead of a version: the launcher's alias block, whose
// specs pin exact versions, and each platform manifest's `version`/`os`/`cpu`.
// A checked-in file carrying a version is a file that is wrong between
// releases.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "..");

/** The six platform builds: npm's own naming on the left, Rust's on the right. */
const PLATFORMS = [
  { slug: "linux-x64", triple: "x86_64-unknown-linux-musl", os: "linux", cpu: "x64" },
  { slug: "linux-arm64", triple: "aarch64-unknown-linux-musl", os: "linux", cpu: "arm64" },
  { slug: "darwin-x64", triple: "x86_64-apple-darwin", os: "darwin", cpu: "x64" },
  { slug: "darwin-arm64", triple: "aarch64-apple-darwin", os: "darwin", cpu: "arm64" },
  { slug: "win32-x64", triple: "x86_64-pc-windows-msvc", os: "win32", cpu: "x64" },
  { slug: "win32-arm64", triple: "aarch64-pc-windows-msvc", os: "win32", cpu: "arm64" },
];

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith("--")) die(`unexpected argument: ${argv[i]}`);
    args[argv[i].slice(2)] = argv[i + 1];
  }
  return args;
}

function die(message) {
  process.stderr.write(`build.mjs: ${message}\n`);
  process.exit(1);
}

const args = parseArgs(process.argv.slice(2));
const name = args.package ?? die("--package is required");
const version = args.version ?? die("--version is required");
const outDir = args["pack-dir"] ?? path.join(ROOT, "target", "npm");

// A version this script would silently mangle into a different one is a
// release nobody can reason about. Plain `x.y.z` only: the platform suffix is
// this script's to append, never the caller's.
if (!/^\d+\.\d+\.\d+$/.test(version)) {
  die(`--version must be a plain x.y.z, got "${version}"`);
}

// Staged outside the repository: `target/` is a build output directory, and on
// an overlay filesystem it is also where a recursive copy into it was refused.
const staging = fs.mkdtempSync(path.join(tmpdir(), "filesnap-npm-"));
fs.mkdirSync(outDir, { recursive: true });

const base = JSON.parse(fs.readFileSync(path.join(HERE, "package.json"), "utf8"));
// `private` exists in the checked-in manifest so that a stray `npm publish`
// inside `npm/` cannot ship a versionless launcher with no platform packages.
// Staging is the one place entitled to remove it.
delete base.private;

if (name === "filesnap") {
  stageLauncher();
} else {
  const platform = PLATFORMS.find((p) => `filesnap-${p.slug}` === name);
  if (!platform) {
    die(`unknown package "${name}" — expected filesnap or one of ${PLATFORMS.map((p) => `filesnap-${p.slug}`).join(", ")}`);
  }
  stagePlatform(platform);
}

const tarball = pack();
fs.rmSync(staging, { recursive: true, force: true });
// The only thing on stdout, so a caller can use it directly.
process.stdout.write(`${tarball}\n`);

function stageLauncher() {
  // One file, copied as one file. Copying "the bin directory" says the same
  // thing less precisely, and drags in a pile of mode- and link-preservation
  // behaviour this does not want.
  fs.mkdirSync(path.join(staging, "bin"), { recursive: true });
  const launcher = path.join(staging, "bin", "filesnap.mjs");
  fs.copyFileSync(path.join(HERE, "bin", "filesnap.mjs"), launcher);
  fs.chmodSync(launcher, 0o755);
  fs.copyFileSync(path.join(HERE, "README.md"), path.join(staging, "README.md"));

  // Alias specs, which is what lets six versions of one name coexist in a
  // single node_modules. Exact versions, not ranges: a range could resolve a
  // launcher onto a platform build from another release.
  const optionalDependencies = {};
  for (const { slug } of PLATFORMS) {
    optionalDependencies[`filesnap-${slug}`] = `npm:filesnap@${version}-${slug}`;
  }

  write({ ...base, version, optionalDependencies });
}

function stagePlatform({ slug, triple, os, cpu }) {
  const src = args["vendor-src"] ?? die("--vendor-src is required for a platform package");
  const binary = path.join(src, triple, "bin", os === "win32" ? "filesnap.exe" : "filesnap");
  if (!fs.existsSync(binary)) {
    die(`no binary at ${binary} — the build job for ${triple} did not produce one`);
  }

  const destDir = path.join(staging, "vendor", triple, "bin");
  fs.mkdirSync(destDir, { recursive: true });
  const dest = path.join(destDir, path.basename(binary));
  fs.copyFileSync(binary, dest);
  // npm preserves the executable bit inside a tarball, but only if it is set
  // on the file being packed — and an artifact that has been through
  // upload/download has lost it.
  if (os !== "win32") fs.chmodSync(dest, 0o755);

  write({
    name: base.name,
    version: `${version}-${slug}`,
    description: `${base.description} (${slug} binary)`,
    // `os` and `cpu` are what make npm unpack exactly one of these six. A
    // platform package installed on the wrong machine is not an error npm
    // reports; it is one it silently skips, which is the intended behaviour
    // for an optional dependency.
    os: [os],
    cpu: [cpu],
    license: base.license,
    repository: base.repository,
    homepage: base.homepage,
    // No `bin`: the launcher owns the command name. A platform package that
    // declared one would race it for the same entry in .bin/.
    files: ["vendor/"],
  });
}

function write(manifest) {
  fs.writeFileSync(
    path.join(staging, "package.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );
}

function pack() {
  // `npm pack --json` changed shape in npm 12 — a list became an object keyed
  // by package name — and took a release down with it. Read the filename off
  // the plain output instead, which has been one line since forever.
  const out = execFileSync("npm", ["pack", "--pack-destination", outDir], {
    cwd: staging,
    encoding: "utf8",
  });
  const file = out.trim().split("\n").pop().trim();
  const tarball = path.join(outDir, file);
  if (!fs.existsSync(tarball)) die(`npm pack reported "${file}" but it is not there`);
  return tarball;
}
