# Compliance Ledger

Where the engine does not yet meet the [constitution](constitution.md). Every
entry cites the line that would have to change, so it can be checked rather than
believed.

The constitution's rules are obligations. An entry here means an obligation is
unmet — never that the rule is softer than it reads. Entries may be deferred; they
may not be forgotten. **No release ships with a `blocking` entry open.**

Line numbers are against `crates/file-snapshots/` as copied in `39e5efe`, and
have shifted since the rename; the cited symbol is the durable reference.
Every entry below was inherited with that copy; none was introduced here.

---

## Blocking — must close before the first published release

### C1 · No durable record carries a format version — VII.1

`Manifest`, `FileEntry` (manifest.rs:25-97), `ThreadLog`, `SnapshotRef`
(refs.rs:23-32), `RestoreLog`, `RestoreRecord` (refs.rs:141-161) have no version
field. `turns/<turn-id>` is a bare manifest-id string (refs.rs:188-200), blobs are
raw bytes, and `SnapshotStore::open` (store.rs:72-81) creates no store-level
version file. Every reader is a plain `serde_json` deserialization with no
`deny_unknown_fields`, so unknown data is silently dropped.

`Manifest::absent` is `#[serde(default)]` (manifest.rs:95). That is the hazard
VII.1 names explicitly: a manifest written before tombstones existed reads back as
"this capture looked for nothing", silently voiding every deletion it licensed.

**Failure:** a user's store outlives the binary that wrote it. The first
incompatible change is unrecoverable, because nothing on disk says which format it
is.

### C2 · Mode is fabricated, and then applied to the user's files — V.2

`attach_pre_edit` stamps `mode: 0o644` on every pre-edit image (store.rs:174) —
content that arrived from patch text with no stat at all. `mode_of` returns
`0o644` for every entry on non-unix hosts (manifest.rs:69-73), a supported
platform. The timestamp half of V.2 is honoured (`(0, 0)`, a fingerprint that can
never match); the mode half is not.

This is not cosmetic. `plan_restore` treats mode as part of the comparison
(restore.rs:60) and `apply_plan` chmods what it writes (restore.rs:100).

**Failure:** restoring to a turn whose entry came from the edit hook writes the
right bytes and then silently strips an executable script's `+x`. The invented
mode also makes the planner see a difference where there is none, rewriting files
whose content already matches.

**Shape of the fix:** `FileEntry::mode` becomes `Option<u32>`; `None` is excluded
from the comparison and leaves permissions untouched on restore. Interacts with
C1 — changing the field re-identifies every manifest (VII.3).

### ~~C3 · The host is named in the API and in the user's directories~~ — VI.1

**CLOSED** by the rename and API pass. The crate is `filesnap`, the ignore
file is `.filesnapignore`, the restore temp suffix is
`.filesnap-restore-tmp`, the `codex_home` parameters are `data_dir`, and no
occurrence of the string `codex` remains anywhere under `crates/`.

*Was:* a filename the user had to type (`.codexsnapignore`), vendor-named
temporary files created in the user's own working tree during every restore,
and a vendor prefix on every `use` a consumer would write.

---

## Should fix

### C4 · `.tmp` residue becomes an uncollectable GC root — VII.2

`TurnIndex::all_manifest_ids` (refs.rs:219-228) reads every file under `turns/`,
`.tmp` included, and inserts its contents as a live manifest id.
`TurnIndex::all_restore_logs` (refs.rs:307-320) likewise deserializes `.tmp` files
as real restore logs. But `retain_turns` skips `.tmp` when deleting (refs.rs:329).
The store's other three enumerations do filter residue (blob.rs:107,
manifest.rs:164, refs.rs:102), and they use two different mechanisms — a `.tmp`
suffix check and a `.json` suffix check — so they can drift apart.

**Failure:** an interrupted `set_turn` leaves a `.tmp` file that pins a manifest
live forever and can never itself be removed.

### C5 · Blob collection on the delete path has no grace window — VIII.2

`collect_garbage_for` (refs.rs:392-396) deliberately skips the grace window:
"a candidate is only considered because a thread that has just been removed named
it, so no live session can be in the middle of publishing it." That is sound for
*manifest identity*, which is unique per thread, and unsound for
*content-addressed blobs*, which are shared: `BlobStore::store_bytes` writes
nothing when the hash is already present.

**Failure:** session A hashes content, finds the blob present, writes nothing.
Session B deletes a conversation, finds that hash among a doomed manifest's
entries, sees no manifest on disk naming it — A has not saved yet — and removes
it. A's manifest lands seconds later pointing at a missing blob, and the restore
fails with `MissingBlob`.

**Shape of the fix:** the blob half of the sweep keeps the grace window even on
the delete path; only the manifest half reclaims immediately. VIII.3's "reclaims
immediately" is about the records and the bulk of the content, not about a blob
written in the last five minutes.

### C6 · The ignore filter is inert until the first capture — II.3

`ignore_root` starts as `None` (controller.rs:53), and the edit hook's filter is
`ignore.as_ref().is_some_and(|rules| is_ignored(rules, &path))` (controller.rs:186)
— `None.is_some_and(..)` is `false`, so nothing is ignored. The doc asserts the
ordering that saves it ("Recorded by the turn-start checkpoint, which always
precedes tool execution within a turn", controller.rs:51) — a promise about a
caller in another crate, with no assertion, no fallback, and no error here.

**Failure:** an edit that lands before the first capture puts an ignored file — a
`.env`, a key — into the blob store, and every later capture then keeps it,
because the path is registered as an extra. II.3 requires the rule to hold from
the first operation.

**Shape of the fix:** fall back to the turn's cwd when no root has been recorded.

### C7 · `undo_conflicts` ignores mode — III.5

`undo_conflicts` compares content hashes only (store.rs:251), while `plan_restore`
treats a mode difference as a write (restore.rs:60). The crate re-reads mode even
on a stat-cache hit precisely because chmod does not bump mtime
(checkpoint.rs:132).

**Failure:** another session's `chmod +x` is reverted by an undo and never
reported as a conflict — the one thing store.rs:221 says stands between a
concurrent edit and silent loss.

### C8 · An over-size file is dropped silently — IV.3

`recent_files` drops any file over `RECENT_MAX_FILE_BYTES` with a bare `continue`
(scope.rs:306-308), before the capture is told the path exists. It is not recorded
as uncaptured — there is no such state on `FileEntry` or `Manifest` — and it is
not counted in `CheckpointStats.skipped`, which counts a different set
(checkpoint.rs:117/123/144). The controller logs that count as a bare aggregate
with no paths (controller.rs:157).

**Failure:** the user is never told a file is outside the safety net. IV.3
requires the entry to be recorded as seen-and-not-stored, and surfaced.

### C9 · Coverage of shell-made changes is conditional — VI.3

Of the three partitions, only the edit-touched one is unbounded. The git index
cannot see an untracked file, and the recency walk skips hidden directories, skips
a hardcoded name list, drops files over 16 MB, and returns at most 100 paths
(scope.rs:80/83/113). A file created by a shell command inside `target/`, inside a
dotted directory, over 16 MB, or beyond the recency budget on a busy turn is
covered *only* if it flowed through the host's edit tool.

**Failure:** none, if stated. VI.3 does not demand total coverage — it demands
that the gap be stated in the interface rather than implied away. Today nothing
states it.

### C10 · A doc comment claims a property the code does not enforce — VII.4

`manifest.rs:92` states that every path a capture was asked about "either has an
entry or appears here". Three branches in `capture` record neither: a non-ENOENT
stat error, a non-regular file, and a read failure (checkpoint.rs:117, 123, 144).
`capture`'s own doc states the real, weaker rule, so the two comments disagree.

The *behaviour* is the correct one and is exactly what II.4 requires. The defect
is the claim, and it matters because II.1's deletion rule is read off the stronger
version — an auditor checking the stated property verifies something the code does
not promise.

### ~~C11 · `collect_garbage` is exported but uncallable~~

**CLOSED** by the API pass. The re-export is gone, along with the dead
`TurnIndex::restore_logs` it kept alive; `SnapshotStore::gc()` is the entry
point, as it always was in practice.

*Was:* a re-export whose signature named `&TurnIndex`, a type not exported
from a private module — public surface no consumer could ever call.

---

## Rules with no test

Quality Gates require every rule to be pinned. These are not.

| Rule | What is unverified | Where |
|---|---|---|
| **II.4** | a path that exists but cannot be read must never land in `absent` | checkpoint.rs:117-123, 143 |
| V.2 | a mode-only difference forces a write on restore | restore.rs:60; the unit helper hardcodes `0o644` (restore.rs:143) |
| IV.1 | roots unrelated to the turn's cwd are dropped, cwd is the fallback | controller.rs:134-145; every controller test passes `&[]` |
| III.4 | the restore log truncates from the front, keeping the newest 20 | refs.rs:242-244; no test pushes more than 3 |
| V.5 | ids are mapped to a filename-safe set, and two ids can collide | refs.rs:355-364, duplicated at refs.rs:111-126 |
| II.3 | the *current* ignore file governs a restore, so newly ignoring a path protects it retroactively | store.rs:376, controller.rs:174 |
| V.5 | workdir vs root spellings reconciled via canonicalize (`/var` vs `/private/var` — the default on macOS) | scope.rs:216-230 |
| IV.1 | `RECENT_SKIP_DIRS` affects only the recency partition; a committed `vendor/` still arrives via the index | scope.rs:100-112 |
| — | a capture failure degrades to "no snapshot for this turn" and never fails the caller | controller.rs:105-112, 187-190 |
| VII.2 | `.tmp` residue is excluded from enumeration after an interrupted write | blob.rs:107, refs.rs:329, manifest.rs:164 |

II.4 is first because it is the dangerous direction: the code is right, nothing
checks it, and if it ever regressed the tombstone rule would turn a permission
error into a licence to delete.

---

## Checked and found sound

Recorded so they are not re-litigated.

- **A rescue point is not orphaned by the 20-record cap.** `restore_to` appends the
  safety checkpoint to the performing thread's log and the turn index
  (store.rs:414), both append-only, so it stays reachable after its restore record
  is drained. The known cost is the opposite one: a spent undo leaves a permanent
  GC root until the conversation is deleted.
- Content is bytes end to end — `store_bytes(&[u8])`, `load -> Vec<u8>`, no decode
  on any storage path (VI.2).
- Raciness is decided at capture and recorded, with a 2s window and a `(0,0)`
  fingerprint that can never match (checkpoint.rs:37-46, manifest.rs:37-47) — V.3
  and V.4 both hold.
- Tombstone-only deletion is implemented as written and tested three ways
  (restore.rs:70-76).
- Zero git writes: the only `Command::new("git")` in the crate is inside a test
  fixture (scope.rs:350); production reads the index through `gix::discover`
  (I.1, I.3).

---

**Last reviewed:** 2026-08-22. Opened against `39e5efe`; C3 and C11 closed by
the rename and API pass.
