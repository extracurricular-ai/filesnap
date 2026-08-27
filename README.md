# filesnap

[![crates.io](https://img.shields.io/crates/v/filesnap.svg?label=filesnap)](https://crates.io/crates/filesnap)
[![crates.io](https://img.shields.io/crates/v/filesnap-cli.svg?label=filesnap-cli)](https://crates.io/crates/filesnap-cli)
[![npm](https://img.shields.io/npm/v/filesnap.svg?label=npm)](https://www.npmjs.com/package/filesnap)
[![CI](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml/badge.svg)](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml)
[![Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

English | [中文](README.zh.md)

Git-free file snapshots and rewind: a content-addressed store that puts a
directory back the way it was at an earlier moment.

It works in a directory that has never seen `git init`, and in one that has —
where it reads the index as a source of file *names* and writes nothing. Your
commits, your stash, your worktree state: untouched. Nothing is stored inside
your repository.

Built for agents that edit files and need an undo their user can trust, but
there is nothing agent-shaped about the engine: it takes opaque string ids and
absolute paths, and has no opinion about what a "session" is.

```console
$ filesnap capture --session s1 --turn t1
{"v":1,"type":"capture.done","manifest":"a5a2b149…","reused":0,"hashed":1,"dropped":0}

$ echo "something regrettable" > a.txt

$ filesnap restore --session s1 --turn t1
{"v":1,"type":"restore.done","written":1,"deleted":0,"failed":0,"safety":"e41358f0…"}
```

That `safety` id is the point the rewind itself can be rewound to. Every
restore captures one before it writes anything.

## Adding rewind to a coding agent

Rewind is two problems, and this engine solves exactly one of them.

**The half that is the same for every agent.** Capture what the workspace holds
at the head of a turn, put it back on demand, in a project that may have no
repository, at a cost that does not grow with the checkout. One binary — the
six release builds run 3.4 to 4.2 MB — with no runtime of its own, running in
front of a model request that takes seconds.

**The half that is not.** Where a rewind point sits in a conversation, when a
turn is worth capturing, what happens to the transcript when the files move, and
in which order the two go so a crash between them is survivable. filesnap
declines to decide any of it: the engine exposes no hook, and the CLI has no
`rewind` command, because a command by that name would promise the combined
operation and deliver half of it. The host sequences both halves, because the
host is the only layer holding both.

Two ways in:

- **Rust** — `cargo add filesnap`. `WorkspaceStore::open` and a `TurnScope`,
  then `capture_turn` at the head of a turn and `declare_edits` before each
  write; `target_for_turn` resolves a turn id to a `RestoreTarget` and
  `restore_to` applies it. `scan_report` answers what is not covered. The CLI
  here is the reference consumer, a module per command under
  [`crates/filesnap-cli/src/commands/`](crates/filesnap-cli/src/commands), and
  [`restore.rs`](crates/filesnap-cli/src/commands/restore.rs) is that sequence
  in order.
- **Anything else** — drive the binary. Every command writes versioned JSON
  Lines to stdout and keeps prose on stderr, and the exit codes are part of the
  contract — `2` means the command did not run or could not report, so no
  terminal event reached stdout — which makes a subprocess and a line parser
  the whole integration. `npm install filesnap` gets you the binary and no JS
  API, because this is the intended path rather than a fallback.

One turn, then a rewind into a forked session:

```console
$ filesnap capture --session s1 --turn t3
$ filesnap declare --session s1 --turn t3 --path /abs/src/main.rs
$ filesnap restore --session s1 --turn t3 --undo-for s2
$ filesnap undo    --session s2
```

`declare` runs immediately before your tool writes, and it is what reaches the
files the two scanned partitions cannot — ship `capture` and `restore` alone
and the integration looks correct while silently missing them. `--undo-for`
names the session the undo record is filed in, so it has to be the session the
user ends up in; a restore without it is deliberately not undoable. The ids are
yours, within one rule the store enforces rather than repairs: `[A-Za-z0-9._-]`,
at most 200 bytes, never starting with `_`. A session is serialised against
itself and nothing wider. When a conversation ends, `filesnap delete --session`
removes it and `filesnap gc` reclaims what nothing references; `filesnap
doctor` clears what an interrupted operation left behind.

## Worked example: dsh-filesnap

[**dsh-filesnap**](https://github.com/extracurricular-ai/dsh-filesnap) is
rewind and redo for [DeepSeek
Harness](https://github.com/deepseek-ai/deepseek-harness), shipped as a plugin:
`/rewind` lists the turns a session can return to, picking one forks the
conversation there and puts the files back with it, and `/redo` reverses that.
It is TypeScript, it drives this binary as a subprocess, and it is what driving
the binary costs in practice.

**Its whole transport to this engine is
[`src/cli.ts`](https://github.com/extracurricular-ai/dsh-filesnap/blob/main/src/cli.ts)
— 116 non-comment lines.** Spawn it, parse the JSON Lines, map the exit codes.
Nothing in that file knows what a rewind is: no turn, no session, no
conversation. Its one coupling to its host is the call it spawns with, so that
is the part you retarget; the rest is this engine's contract and copies as it
stands.

The other ~1,100 lines of that plugin's host half are what filesnap refused to
decide, and they are what the integration cost: capture on `agent/pre-step`,
before the model request and before any tool runs; pre-images declared on the
write and edit seams, at the last moment the old bytes still exist; fork first
and restore into the fork, because the undo record has to land in the session
the user ends up standing in.

Capture cost, measured there:

| | files captured | first capture | every capture after |
|---|---|---|---|
| the plugin's own repository | 84 | 20 ms | 8 ms |
| the harness monorepo | 7,995 of 70,918 on disk | 1.75 s | 268 ms |

*(Measured on the plugin author's machine with `filesnap capture`, warm page
cache. Your numbers will differ; the shape will not.)*

**files captured** is [the bounded scan](#what-a-snapshot-covers): a snapshot
covers what a turn can plausibly touch rather than everything under the root.
**every capture after** is content addressing — the plugin's own repository
hashed nothing on its second capture and reused all 84 files, so ten captures
of an unchanged file store it once.

Before dsh there was a fork of OpenAI Codex, where this engine was written and
where the design shipped first. It is not a second example to read: that fork
is an ancestor rather than a consumer, carrying its own inlined copy, and fixes
do not flow between the two in either direction.

## Which crate do you want

| | |
|---|---|
| **[`filesnap-cli`](crates/filesnap-cli)** | the `filesnap` command. What a person, a shell script, or a program in any language installs. |
| **[`filesnap`](crates/filesnap)** | the engine, as a library. What a Rust program embeds. |

```console
$ cargo install filesnap-cli    # installs the `filesnap` binary
$ npm install -g filesnap       # the same command, prebuilt
```

The crate carries the `-cli` suffix and the command does not, the way
`ripgrep` produces `rg`. The npm package is a launcher: the build for your
platform — Linux, macOS and Windows, on x64 and arm64 — arrives as an optional
dependency, so the install downloads one of the six and not all of them.

Each has its own README with the detail: the [command
surface](crates/filesnap-cli/README.md) — nine commands, the JSON Lines
contract, and the exit codes — and the [engine's
design](crates/filesnap/README.md).

## What it will not do

- **Touch your version control.** Git is read, never written.
- **Delete a file it has never observed.** A restore removes a path only when
  the capture it is restoring to looked for that path and did not find it. A
  tombstone is the only licence a restore has to delete.
- **Lose the rest of a restore to one bad file.** A file that cannot be written
  is named, the others still land, and the exit code says so — `1`, not `0`.
- **Snapshot what you excluded.** `.filesnapignore` is symmetric: an ignored
  path is never stored, never restored, and never deleted by a restore.
- **Grow with your tree.** The tracked set is a union of three bounded
  partitions, none of which scales with the size of the directory — see [what a
  snapshot covers](#what-a-snapshot-covers).

## What a snapshot covers

Three partitions, unioned. Each answers a different question, and each is
bounded by something other than the size of the tree — which is the property a
plain subtree walk lacks, and the reason it was abandoned.

1. **Git-tracked** — what the project itself calls its files, read straight out
   of the index file: the index is a file, `git ls-files` is a process, and
   this runs at the head of every turn. Bounded by the project instead of by
   what has been built inside it, because
   build output is precisely what is not committed. Nothing here is dropped for
   being large: whatever a project commits is the project's own content,
   however big.
2. **Edit-touched** — paths the host declares in the moment before it writes
   them, which is the last moment the old bytes still exist. Bounded by what
   the agent did rather than by what is on disk: every entry is a file someone
   deliberately changed, and nothing here grows with the tree. How long a path
   stays watched afterwards is yours to set — `--declared-window`, 99 turns by
   default, or `unlimited`.
3. **Recently modified** — the residue: what a shell command or the user's own
   editor changed outside the other two. This is the one partition a large tree
   could flood, so it is the one with a hard budget — 100 files per root,
   nothing over 16 MB, and a skip list of directories that churn
   (`node_modules`, `target`, `dist`, `vendor`, …).

**With no repository the first partition is simply empty**, and the other two
carry the workspace between them. That is a first-class case, not a degraded
one.

**The order is load-bearing.** Recency runs last and is told what the first two
already hold. Measured on this repository before that exclusion existed, 97 of
its 100 slots went to files the git index had already supplied — invisible
until enough tracked files change at once (a `cargo fmt`, a branch switch, a
codegen run) to push the genuinely untracked ones off the end of the list
entirely.

**A walk could never be the only partition, however cheap it got.** Only a
list-shaped partition can propose a path that is not there, and a path proposed
and not found is the tombstone a restore needs before it is allowed to delete
anything. An engine built on a walk can put a deleted file back; it can never
remove a created one.

**The recency walk skips hidden entries**, so `.env` and credential files never
arrive that way — but a hidden file the project *commits* still comes in
through the index, and work product that happens to be hidden still enters
through the edit seam. `.filesnapignore` (above) is the one rule that closes
every direction at once.

On a different working repository a subtree walk found 70,609 files and 116 GB;
the git-tracked and residue partitions together came to 6,096 files and 59 MB.
Reproduce it on yours:

```text
cargo run -p filesnap --example scan_bench -- /path/to/repo
```

Total coverage is not promised, and the gap is stated rather than implied: a
file created by a shell command inside `target/`, inside a dotted directory,
over the size limit, or beyond the recency budget on a busy turn is covered
only if it also went through the edit seam. `filesnap status` names every file
the scan looked at and could not store, and why — too large, unreadable, or not
a regular file. The two bounds it cannot name are the churn directories it
never descends into and the tail past the budget, and covering those is what
the edit seam is for.

## Where it keeps things

Under your platform data directory (`$XDG_DATA_HOME` or `~/.local/share` on
Unix, `%LOCALAPPDATA%` on Windows), never inside your project. `--data-dir`
overrides it.

The format version is part of the path — `filesnap/v2/` — so upgrades do not
migrate and do not guess. A build meeting a store it does not understand
refuses it rather than misreading it, and a store from an older format is left
alone rather than rewritten.

## Building from source

Rust 1.89 or newer (set by `std::fs::TryLockError`, which the session lock
uses), edition 2024.

```console
$ cargo test --workspace --features filesnap/test-support
$ cargo clippy --workspace --all-targets --features filesnap/test-support
```

CI runs the suite on Linux, macOS and Windows, plus a build on the declared
MSRV. Platform-specific behaviour is tested where it exists, not predicted:
the Windows-only tests live in
[`crates/filesnap/tests/windows.rs`](crates/filesnap/tests/windows.rs) and
their Unix counterparts in
[`permissions.rs`](crates/filesnap/tests/permissions.rs).

## Design record

The rules this project is built to keep are in
[`.specify/memory/constitution.md`](.specify/memory/constitution.md), the
numbered decisions behind them in
[`decisions.md`](.specify/memory/decisions.md), and the places it does not yet
keep them in [`compliance.md`](.specify/memory/compliance.md). Comments in the
source cite decision numbers rather than restating the reasoning.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE) — this project
originated inside a fork of [OpenAI Codex](https://github.com/openai/codex)
and is released under the same licence.
