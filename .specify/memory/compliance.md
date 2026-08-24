# Compliance Ledger

Where the engine does not yet meet the [constitution](constitution.md). Every
entry cites the line that would have to change, so it can be checked rather than
believed.

The constitution's rules are obligations. An entry here means an obligation is
unmet — never that the rule is softer than it reads. Entries may be deferred; they
may not be forgotten. **No release ships with a `blocking` entry open.**

The detail behind every closed entry — the failing scenario, the cited lines, the
shape of the fix — is in this file's history. It was deleted here rather than
kept, because a ledger of solved problems stops being read, and this one has to
be read.

---

## Open

**None.** C1–C20 were opened against the engine as copied from codex in `39e5efe`
and are closed as of `949a0a4`. What that means is not "the engine is correct"
but "no gap between the engine and the constitution is currently known".

Two things keep that honest:

- Every entry below was found by *reading the code against a written rule*, never
  by using the software. There is no CLI yet, so none of these rules has been
  exercised by a person.
- Four of the worst entries (C5, C12, C13, C15) came from an audit that ran
  **after** the code was believed finished, and two of them had been introduced
  by an earlier round of fixing. Fixing is how defects are made.

---

## Closed

| | Rule | What was wrong | Closed by |
|---|---|---|---|
| C1 | VII.1 | No durable record carried a format version, and `absent`'s serde default made a pre-tombstone record read as "this capture looked for nothing" — voiding every deletion it had licensed | `97677ae`, `59b226b` |
| C2 | V.2 | `mode` was fabricated as `0o644` for content that had no stat behind it, then applied: a rewind wrote the right bytes back and stripped a script's executable bit | `97677ae` |
| C4 | VII.2 | `.tmp` residue was skipped by every enumeration and removed by none, so a stray was permanent — and where a record's name could collide with it, an uncollectable GC root | `8b7d574`, `9cf889a` |
| C5 | VIII.2 | Delete swept blobs with no grace window; and because writes dedup, an object's mtime recorded *creation*, so an old blob adopted by a new capture was never protected by the window at all | `9cf889a`, `23c8721` |
| C6 | II.3 | The edit hook's ignore filter was `None.is_some_and(..)` until the first capture — matching nothing, so an ignored `.env` could enter the store and be kept by every capture after | `949a0a4` |
| C7 | III.5 | `undo_conflicts` compared content only, so another session's `chmod +x` was reverted by an undo and never reported | `949a0a4` |
| C8 | IV.3 | A file over the size limit was a bare `continue` that nothing counted | `6a5c709` |
| C9 | VI.3 | The coverage gap was real and unstated. VI.3 asks for it to be stated, not closed | `949a0a4` |
| C10 | VII.4 | `Manifest::absent` claimed every path was accounted for; three branches record neither an entry nor a tombstone | `949a0a4` |
| C12 | VIII.1, D10 | `retain_turns` removed turn entries by **global elimination** from the delete path, so deleting one conversation could unlink a live session's turn entry — a rewind lost permanently, since nothing rebuilds one | `9cf889a` |
| C13 | VIII.3 | The delete sweep removed manifests before resolving which blobs survived, orphaning blobs no later delete could rediscover | `9cf889a` |
| C14 | D10 | `live_manifest_ids` was a query that wrote: delete's own record cleanup happened inside gc's marking helper | `9cf889a` |
| C15 | — | The delete sweep was less tolerant than the general one, so one corrupt manifest aborted it after the index was gone and before a byte was freed | `9cf889a` |
| C16 | VIII.3, D11 | delete's two unlinks were in the order that leaves an undo record for a session with no log | `29ea3e4` |
| C17 | II.3 | `is_protected: &dyn Fn(&str) -> bool`, with nothing exported that builds one | `59b226b` |
| C18 | — | `ignore` was an undeclared public dependency | `59b226b` |
| C19 | — | `RECENT_LIMIT` / `RECENT_MAX_FILE_BYTES` were `pub` inside a private module, and so unreachable | `59b226b` |
| C20 | III.1 | A failed restore did not hand back the point it could be reversed to — reversibility existing and being out of reach exactly when it was needed | `6a5c709` |

C3 and C11 closed earlier, by the rename and the API pass.

**One defect was introduced and closed inside this work**, recorded because the
pattern is the point. `collect_partition` marked from a single partition and then
swept the *shared* blob store, so deleting a session in workspace A destroyed
workspace B's content while B was live and nothing had been deleted. It came from
implementing D19's records half without its content half — the same shape as C12
and C13: a fix that moved a boundary without moving everything standing on it.

---

## Rules with no test

Quality Gates require every rule to be pinned. These are not.

| Rule | What is unverified | Where |
|---|---|---|
| IV.1 | roots unrelated to the turn's cwd are dropped, and cwd is the fallback | `controller.rs`; every controller test passes `&[]` |
| II.3 | the *current* ignore file governs a restore, so newly ignoring a path protects it retroactively | `store.rs`, `controller.rs` |
| IV.1 | `RECENT_SKIP_DIRS` bounds only the recency partition; a committed `vendor/` still arrives via the index | `scope.rs` |
| — | a capture failure degrades to "no snapshot for this turn" and never fails the caller | `controller.rs` |

Everything else previously listed here is now covered: **II.4** — the dangerous
direction, and the one that headed this table — by
`checkpoint.rs::a_path_that_cannot_be_read_is_skipped_rather_than_declared_absent`,
which exercises both routes to unreadable; **V.2** by `tests/permissions.rs`;
**III.4** by `store_tests.rs::a_full_undo_stack_forgets_the_oldest_rewind_not_the_newest`;
**V.5**'s two halves by `tests/ids.rs` and `workspace_tests.rs`; and **VII.2** by
`sweep_tests.rs::residue_is_reclaimed_once_it_is_settled`.

---

## Checked and found sound

Recorded so they are not re-litigated.

- **A rescue point is not orphaned by the 20-record cap.** `restore_to` appends
  the safety checkpoint to the performing thread's log and to the turn index,
  both append-only, so it stays reachable after its restore record is drained.
  The known cost is the opposite one: a spent undo leaves a permanent GC root
  until the conversation is deleted.
- Content is bytes end to end — `store_bytes(&[u8])`, `load -> Vec<u8>`, no
  decode on any storage path (VI.2).
- Raciness is decided at capture and recorded, with a 2s window and a `(0,0)`
  fingerprint that can never match — V.3 and V.4 both hold.
- Tombstone-only deletion is implemented as written, and tested three ways.
- Zero git writes: the only `Command::new("git")` in the crate is inside a test
  fixture; production reads the index through `gix::discover` (I.1, I.3).

---

**Last reviewed:** 2026-08-24. Opened against `39e5efe`; closed out by
`9cf889a..949a0a4`, which also closed 19 gaps between the code and
[decisions.md](decisions.md). The audit's actual finding was not that the code
was wrong about anything undecided — it was that decisions had been *recorded and
not implemented*, in several cases for many rounds while later work was built on
top of them.
