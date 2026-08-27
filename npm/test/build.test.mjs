// The generator's output, read back, and the launcher's triple table held
// against the tree the build actually stages.
//
// npm does not reject an unrecognised `os`; it silently installs the package
// nowhere. A typo there is not a red build, it is a burned version number —
// a published version can never be replaced (D37). And the two triple tables,
// `filesnap.js`'s and `build.mjs`'s, are written out separately with nothing
// comparing them: a drift between them is invisible at build and publish time
// and surfaces only as "the package is missing or has no binary" on a user's
// machine, for a binary sitting one directory over.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BUILD = path.join(HERE, "..", "build.mjs");
const LAUNCHER = path.join(HERE, "..", "bin", "filesnap.js");
const BASE = JSON.parse(
  fs.readFileSync(path.join(HERE, "..", "package.json"), "utf8"),
);
const VERSION = "9.9.9";

// Spelled out here rather than imported from build.mjs: importing the constant
// would make this agree with the implementation by construction. The left
// column is what npm compares against `process.platform` / `process.arch`,
// verbatim; the right is the directory the release stages under.
const PLATFORMS = [
  { slug: "linux-x64", triple: "x86_64-unknown-linux-musl", os: "linux", cpu: "x64" },
  { slug: "linux-arm64", triple: "aarch64-unknown-linux-musl", os: "linux", cpu: "arm64" },
  { slug: "darwin-x64", triple: "x86_64-apple-darwin", os: "darwin", cpu: "x64" },
  { slug: "darwin-arm64", triple: "aarch64-apple-darwin", os: "darwin", cpu: "arm64" },
  { slug: "win32-x64", triple: "x86_64-pc-windows-msvc", os: "win32", cpu: "x64" },
  { slug: "win32-arm64", triple: "aarch64-pc-windows-msvc", os: "win32", cpu: "arm64" },
];

/** Run build.mjs into a scratch dir; return [tarball, manifest]. */
function generate(pkg, vendor) {
  const work = fs.mkdtempSync(path.join(tmpdir(), "filesnap-buildtest-"));
  const argv = ["--package", pkg, "--version", VERSION, "--pack-dir", work];
  if (vendor) argv.push("--vendor-src", vendor);
  const tarball = execFileSync("node", [BUILD, ...argv], {
    encoding: "utf8",
  }).trim();
  // `tar` is on all three runner images; Windows ships bsdtar.
  const manifest = JSON.parse(
    execFileSync("tar", ["xzOf", tarball, "package/package.json"], {
      encoding: "utf8",
    }),
  );
  return [tarball, manifest];
}

/** A vendor tree holding one stand-in binary per triple, as the release job's would. */
function vendorTree(rows) {
  const root = fs.mkdtempSync(path.join(tmpdir(), "filesnap-vendor-"));
  for (const { triple, os } of rows) {
    const dir = path.join(root, triple, "bin");
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(
      path.join(dir, os === "win32" ? "filesnap.exe" : "filesnap"),
      "binary",
    );
  }
  return root;
}

/**
 * The launcher's table, read out of its source.
 *
 * Read rather than imported: the launcher is a script, and importing it
 * locates a binary and spawns one.
 */
function launcherTable() {
  const src = fs.readFileSync(LAUNCHER, "utf8");
  const body = /const TRIPLE_BY_PLATFORM = \{([^}]*)\}/.exec(src);
  assert.ok(body, "TRIPLE_BY_PLATFORM is not where this test expects it");
  const table = {};
  for (const [, slug, triple] of body[1].matchAll(/"([^"]+)":\s*"([^"]+)"/g)) {
    table[slug] = triple;
  }
  return table;
}

test("each platform manifest carries the os, cpu and version npm selects on", () => {
  const vendor = vendorTree(PLATFORMS);
  for (const { slug, os, cpu } of PLATFORMS) {
    const [, manifest] = generate(`filesnap-${slug}`, vendor);
    assert.deepEqual(
      manifest,
      {
        name: "filesnap",
        version: `${VERSION}-${slug}`,
        description: `${BASE.description} (${slug} binary)`,
        os: [os],
        cpu: [cpu],
        license: BASE.license,
        repository: BASE.repository,
        homepage: BASE.homepage,
        files: ["vendor/"],
      },
      `manifest for filesnap-${slug}`,
    );
  }
});

test("the launcher pins every platform build to this exact version", () => {
  const [, manifest] = generate("filesnap");
  assert.deepEqual(
    manifest.optionalDependencies,
    Object.fromEntries(
      PLATFORMS.map(({ slug }) => [
        `filesnap-${slug}`,
        `npm:filesnap@${VERSION}-${slug}`,
      ]),
    ),
  );
  assert.equal(manifest.version, VERSION);
  // `private` survives into a tarball only if staging forgot to strip it, and
  // the launcher would then be unpublishable.
  assert.equal(manifest.private, undefined);
});

test("every slug the launcher believes in resolves to a binary the build packs", () => {
  const table = launcherTable();
  assert.deepEqual(
    table,
    Object.fromEntries(PLATFORMS.map(({ slug, triple }) => [slug, triple])),
  );

  for (const [slug, triple] of Object.entries(table)) {
    const win = slug.startsWith("win32");
    // Staged under the triple the *launcher* believes in. If build.mjs
    // disagrees it finds no binary here and dies — the drift, caught.
    const vendor = vendorTree([{ triple, os: win ? "win32" : "linux" }]);
    const [tarball] = generate(`filesnap-${slug}`, vendor);

    // The exact path the launcher joins, relative to the package root it
    // resolves, asserted against what is really inside the tarball.
    const wanted = ["package", "vendor", triple, "bin", win ? "filesnap.exe" : "filesnap"].join("/");
    const listing = execFileSync("tar", ["tzf", tarball], { encoding: "utf8" })
      .split("\n")
      .map((l) => l.replace(/^\.\//, "").trim());
    assert.ok(
      listing.includes(wanted),
      `filesnap-${slug}: the launcher looks for ${wanted}, the tarball holds:\n  ${listing.filter(Boolean).join("\n  ")}`,
    );
  }
});
