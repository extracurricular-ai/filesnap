# filesnap-cli

The `filesnap` command: git-free file snapshots and rewind, for a directory
that has never seen `git init` and for one that has.

Installs as `filesnap`. The crate carries the `-cli` suffix and the command
does not, the way `ripgrep` produces `rg`.

```text
cargo install filesnap-cli
```

## What it does

Captures what a directory holds at a moment you name, and puts it back later —
without a repository, and without touching your version control.

```console
$ filesnap capture --session s1 --turn turn-1
{"v":1,"type":"capture.started","session":"s1","turn":"turn-1","roots":["/work/project"]}
{"v":1,"type":"capture.done","manifest":"a1b2c3…","reused":412,"hashed":7,"dropped":0}

$ filesnap restore --session s1 --turn turn-1 --undo-for s1
{"v":1,"type":"restore.written","path":"/work/project/src/main.rs"}
{"v":1,"type":"restore.done","written":1,"deleted":0,"failed":0,"safety":"c3d4e5…"}
```

## Commands

| | |
|---|---|
| `capture` | the state of a workspace at the start of a turn |
| `declare` | what a path holds **before** an edit changes it |
| `log` | what a session can rewind to |
| `restore` | put the workspace back to a turn's state |
| `undo` | reverse this session's most recent restore |
| `delete` | end a session's data |
| `gc` | reclaim what nothing references, across every workspace |
| `status` | what the state of this workspace is — read-only |
| `doctor` | report whether locking works, and clear what an interrupted operation left behind |

## Output

**JSON Lines on stdout, prose on stderr.** Every line is one object carrying a
schema version, so a consumer can parse incrementally and refuse a shape it
does not understand — a version in a header would not survive `grep`, `tail`
or `split`, and a reader here may hold exactly one line.

```console
$ filesnap status | jq -r 'select(.type=="status.unprotected") | "\(.reason)\t\(.path)"'
overSizeLimit   /work/project/data/dump.bin
notARegularFile /work/project/link.rs
```

Exit codes are part of the contract:

| | |
|---|---|
| `0` | everything asked for happened |
| `1` | it ran and reported, but not everything happened — **stdout still says exactly what** |
| `2` | it did not run, or could not report |
| `3` | the arguments were wrong; nothing was attempted |

A restore that could not write every file exits `1`. It is not an error to
ignore: the terminal event names each failure and carries the point the
restore can be reversed to.

## What it will not do

- **Touch your version control.** Git is read as one source of file *names* and
  never written.
- **Delete a file it has never observed.** A restore removes a path only when
  the capture it is restoring to looked for that path and did not find it.
- **Snapshot what you excluded.** `.filesnapignore` is symmetric — an ignored
  path is never stored, never restored, and never deleted by a restore.

## Coverage

A file is tracked if the project's git index lists it, a recency scan finds
it, or an edit declares it. So a file created by a shell command inside a
build directory, over the size limit, or beyond the recency budget is covered
only if it also went through the edit API. `filesnap status` tells you which
files in your project are not protected, and why.

## Locking

Two invocations of one session are held apart by an OS file lock. Some
filesystems have none — and rather than refuse to work there, filesnap
proceeds unlocked, which is cargo's precedent and the right call: a race that
needs two concurrent invocations of one session is not worth breaking a user
who has no other machine.

But it is a fact about your setup, and the only other way to learn it is a
race nobody can reproduce on demand. So `doctor` says:

```console
$ filesnap doctor
{"v":1,"type":"doctor.locking","enforced":true}
{"v":1,"type":"doctor.done","removed":0,"failed":0}
```

The event is **absent**, not `false`, when the store could not be opened at
all — "we could not ask" and "we asked and the answer is no" are different
answers.

The engine is [`filesnap`](https://crates.io/crates/filesnap); this crate is
the command around it.
