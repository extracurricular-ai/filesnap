# filesnap

[![crates.io](https://img.shields.io/crates/v/filesnap.svg?label=filesnap)](https://crates.io/crates/filesnap)
[![crates.io](https://img.shields.io/crates/v/filesnap-cli.svg?label=filesnap-cli)](https://crates.io/crates/filesnap-cli)
[![CI](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml/badge.svg)](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml)
[![Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

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

## Which crate do you want

| | |
|---|---|
| **[`filesnap-cli`](crates/filesnap-cli)** | the `filesnap` command. What a person or a shell script installs. |
| **[`filesnap`](crates/filesnap)** | the engine, as a library. What a program embeds. |

```console
$ cargo install filesnap-cli    # installs the `filesnap` binary
```

The crate carries the `-cli` suffix and the command does not, the way
`ripgrep` produces `rg`.

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
- **Grow with your tree.** The tracked set is a union of bounded partitions.
  On one working repository a full subtree walk found 70,609 files and 116 GB;
  the union came to 6,096 files and 59 MB.

`filesnap status` tells you which files in your project are *not* protected,
and why — because a bound you have to guess at is not a bound.

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
