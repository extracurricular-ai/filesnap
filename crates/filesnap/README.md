# filesnap

Git-free file snapshots and rewind: a content-addressed store that puts a
directory back the way it was at an earlier moment.

- **Never touches your version control.** Git is read, never written — and the
  engine works in a directory that has never seen `git init`. Nothing is stored
  inside your repository or worktree.
- **Deletes only against evidence.** A restore removes a file only when the
  capture it is restoring to looked for that path and did not find it. A file
  the engine has never observed is never touched in any direction.
- **Reversible before it begins.** Every restore captures a rescue point first,
  so a rewind can be rewound.
- **Bounded by the project, not by the tree.** The tracked set is a union of
  partitions, none of which grows with the size of the directory. On a working
  repository, a subtree walk enumerated 70,609 files and 116 GB; the union came
  to 6,096 files and 59 MB.

Content-addressed blobs, per-checkpoint manifests with a persistent stat cache,
per-session logs, and mark-and-sweep collection.

The engine knows nothing about its host: it takes opaque string ids and
absolute paths. Reproduce the scope figures on your own repository with:

```text
cargo run -p filesnap --example scan_bench -- /path/to/repo
```

The rules this crate is built to keep are in
[`.specify/memory/constitution.md`](../../.specify/memory/constitution.md); the
places it does not yet keep them are in
[`.specify/memory/compliance.md`](../../.specify/memory/compliance.md).
