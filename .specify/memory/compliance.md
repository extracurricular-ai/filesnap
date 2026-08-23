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

### C4 · `.tmp` residue becomes an uncollectable GC root, and silently defeats delete — VII.2

**Consequence upgraded 2026-08-23.** This is not only a disk leak. An
interrupted capture in a *live, unrelated* session leaves `turns/<turn>.tmp`
holding a manifest id M. `all_manifest_ids` counts M live; `retain_turns`
refuses to delete `.tmp`. When the user then deletes the conversation that owns
M, `collect_garbage_for` sees M as live, keeps it, and **reports success**. The
deleted conversation's file contents stay on disk and no operation in the crate
can ever remove them — a broken VIII.3 deletion promise, caused by a crash in a
session that has nothing to do with either.

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

**Shape of the fix — corrected 2026-08-23, twice over.**

*First,* the earlier fix here ("give the delete path a grace window for blobs")
is not sufficient on its own. `store_bytes` writes nothing when the hash is
present, so a blob's mtime records when it was *created*, not when it was last
referenced. A three-day-old blob adopted by a capture one second ago is
`settled()` and the window never sees it. Git answers this by *freshening* — it
`utime()`s an existing loose object rather than skipping it silently, so
`gc.pruneExpire` measures last reference. Do that first; the window is the
second half, not the first.

*Second,* the hazard is not confined to blobs. **`ManifestStore::save` dedupes
identically** (manifest.rs:122): a live session that recaptures unchanged state
re-derives an existing manifest id and writes nothing, so the manifest is
instantly collectable by both sweeps despite being about to become live. Its log
entry then points at a manifest that is gone, `latest_manifest` returns
`MissingManifest`, and **that session stops capturing entirely**. The
constitution states this hazard for content only (VIII.2, "true of record
identity and false of content") — manifest *ids* are content-addressed too, so
the sentence is drawn in the wrong place and needs widening.

The premise `collect_garbage_for` rests on — "a candidate is only considered
because a thread that has just been removed named it, so no live session can be
in the middle of publishing it" (refs.rs:376) — is false under dedup in both
directions. A concurrent session publishes by *reusing the doomed file*.

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

### C12 · Deleting one session can amputate another's history — D10, VIII.1

`retain_turns` (refs.rs:306) is reached from the delete path via
`live_manifest_ids` (refs.rs:436). It unlinks **every** file in `turns/` not
named by a currently-readable surviving log — global elimination, not scoped to
the sessions being deleted, and with no grace window.

Every capture writes its log entry *before* its turn file (store.rs:125→134,
185→192, 219→228), which opens the window:

```
session A            capture: refs.append(T) … then set_turn(T)
process B            delete: thread_logs() reads the logs   ← before A's append
session A            refs.append(T) lands
session A            set_turn(T) writes turns/T
process B            retain_turns() unlinks turns/T as unreferenced
```

**Failure:** A's log still carries turn T, the index does not, and nothing ever
rebuilds it — `set_turn` runs only at capture time and `target_for_turn` is the
sole resolution path. The user's rewind to that turn reports no snapshot,
permanently, in a session nobody deleted. This is the exact shape D10 forbids: a
sweep editing records that belong to an operation it should be independent of.

**Shape of the fix:** delete scopes turn removal to the doomed sessions' own
turn ids. Any global reconciliation moves behind the `settled()` check the
manifest and blob halves already use.

### C13 · The delete sweep removes manifests before it knows which blobs survive — VIII.3

`collect_garbage_for` collects blob candidates into an in-memory set
(refs.rs:397), **removes the manifests** (refs.rs:409), and only then subtracts
hashes still referenced by surviving manifests (refs.rs:414-418) before removing
the rest (refs.rs:419).

**Failure:** any failure between those two points — the `?` on
`manifests.load` at refs.rs:415 racing a concurrent sweep, a failed
`blobs.remove`, or a crash — permanently orphans blobs that no future
`gc_for` can rediscover, because its candidate set is only ever that delete's
own doomed manifests, which are already gone. Only the full `collect_garbage`,
which enumerates `blobs/` directly, can find them. The user sees a warning and
keeps the bytes.

Note also refs.rs:403 (`let Ok(manifest) = manifests.load(id) else { continue }`)
silently drops any doomed id whose manifest is already gone, so the blobs it
named are never even considered.

**Shape of the fix — retired by the layout (D19), not fixed directly.** Delete no
longer touches blobs at all: content is global and only `gc` reclaims it, and
gc's blob pass enumerates `blobs/` and marks from surviving manifests, so it is a
function of what is on disk and repairs itself after any interruption. There is
no remembered doomed set left to lose. D20's ordering rule still governs gc's own
two passes.

### C14 · `live_manifest_ids` is a query that writes — D10

`live_manifest_ids` (refs.rs:427) answers "which manifests are live" and, as a
side effect, calls `retain_turns` to prune the turn index. Both delete and gc
reach it. So delete's own record cleanup happens inside gc's marking helper, and
delete's result depends on what that helper read — which is also how
unwhitelisted `.tmp` residue (C4) enters delete's liveness computation.

One hole, two symptoms, and the reason the two operations cannot currently be
separated: the query and the mutation are the same function.

**Shape of the fix:** split into a read-only `live_manifests(refs)` both call,
plus explicit pruning each performs for the records it owns.

### C15 · The delete sweep is more fragile than the general one, in the wrong direction

`collect_garbage_for`'s blob-liveness scan loads **every** manifest in the store
with hard `?` propagation (refs.rs:414-415), with no liveness filter. The
general sweep is strictly more tolerant: it only loads manifests it has already
decided to keep (refs.rs:478), so a dead corrupt manifest is unlinked rather
than fatal.

**Failure:** one corrupt or concurrently-unlinked manifest belonging to an
unrelated surviving session aborts the delete sweep *after* the index is gone
and *before* one blob is freed — index removed, bytes kept, which store.rs:587
says is precisely what this function exists to prevent. `refs.thread_logs()?`
and `retain_turns`'s intolerance of a racing `NotFound` (refs.rs:315) abort even
earlier, before anything is reclaimed.

**Shape of the fix:** tolerate an unloadable manifest in the blob-liveness scan
the way the doomed loop above it already does, and continue past a single failed
unlink instead of returning on the first.

*Also worth fixing: store.rs:593 claims "a partial sweep is not a corrupt store
— the next one finishes the job", while refs.rs:379 admits "with the sweep only
running on deletion, 'later' can mean never". The two comments contradict each
other and the second one is right.*

### C16 · delete's two unlinks are in the order that makes a crash illegal — VIII.3, D11

`forget_sessions` removes the session log first and the undo records second:

```rust
store.rs:628   store.remove_thread(thread_id)     // refs/<id>.json
store.rs:631   store.remove_restores(thread_id)   // restores/<id>.json
```

**Failure:** interrupted between the two, the store holds an undo record for a
session with no log — a state no normal operation produces. `all_restore_logs`
reads every file under `restores/` without checking whether its session still
exists, so that orphan record becomes a permanent GC root, pinning its target
and safety manifests and their blobs for good.

**Shape of the fix:** swap them. Removing the undo record first leaves, at the
only interruption point, a session that simply never rewound — which is legal
and indistinguishable from the ordinary case. Ordering is the whole fix; no
journal, no lock, no marker file.

### C17 · II.3's enforcement point is handed to the caller with no supported form — II.3

`restore_to` and `undo_conflicts` take `is_protected: &dyn Fn(&str) -> bool`, and
the crate exports nothing that builds one. Both call sites in the test suite
hand-write the same closure:

```rust
let ignore = load_ignore(&ws);
let protect = move |path: &str| is_ignored(&ignore, Path::new(path));
```

**Failure:** symmetric ignore is the rule that keeps a restore from writing over
or deleting a path the user excluded, and the only thing standing behind it is a
closure every consumer writes from scratch. A wrong one does not fail — it
silently stops protecting. The keys are manifest path *strings* while
`is_ignored` takes a `Path`, so the conversion is a step a consumer can plausibly
get wrong without noticing.

**Resolved by D12**: the parameter becomes the ignore rules themselves, so there
is no predicate to build.

### C18 · `ignore` is an undeclared public dependency

`Gitignore` appears in `load_ignore`, `is_ignored`, `recent_files`, and
`git_tracked_files`, and the crate does not re-export it.

**Failure:** a consumer who stores the value in a struct must add `ignore` to
their own `Cargo.toml` and keep its version in step with ours, with nothing
saying so. The coupling itself is accepted (D13); the defect is that it is
accidental rather than declared.

**Resolved by D13**: `pub use ignore::gitignore::Gitignore;` plus a documented
statement that this is a public dependency.

### C19 · `RECENT_LIMIT` and `RECENT_MAX_FILE_BYTES` are unreachable public constants

Both are `pub const` in `scope.rs` (:94, :97), which is a private module, and
neither is re-exported from `lib.rs`. Dead surface of the same class as the
closed C11.

**Resolved by D14**: they become the `Default` of a `ScanLimits` parameter rather
than exported constants, so nothing needs to read them.

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

**Last reviewed:** 2026-08-23. Opened against `39e5efe`; C3 and C11 closed by
the rename and API pass. C12–C15 added, and C4/C5 sharpened, by the delete/gc
boundary audit — C4's consequence turned out to be a broken deletion promise
rather than a disk leak, and C5's stated fix was wrong twice over. C16 added
with D11. C17–C19 added from the API walkthrough — three defects I had named in
conversation, said I would record, and then did not; they sat unrecorded for
several rounds while the delete/gc work went on. **No code has changed since the
API pass; every open entry is still open.**
