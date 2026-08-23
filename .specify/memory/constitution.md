# filesnap Constitution

filesnap is a git-free file snapshot and rewind engine: a content-addressed store
that can put a directory back the way it was at an earlier moment, without a
repository, without touching the user's version control, and without a cost that
grows with the size of the tree.

It exists because the obvious implementations of this idea have failed in public,
in ways that are recorded rather than guessed at. Every rule below names the
failure it prevents; a rule without one is a preference and belongs elsewhere.

**These are obligations, not descriptions.** Where the engine does not yet meet
one, that is a defect with a name and a line number, recorded in
[compliance.md](compliance.md) — never a reason to soften the rule. Rules are
sub-numbered so a violation can be cited exactly: "this breaks V.2" is answerable
in a way "this breaks V" is not.

Choices made *under* these principles — and the alternatives they rule out —
are recorded in [decisions.md](decisions.md).

## Core Principles

### I. The user's version control is not ours to touch (NON-NEGOTIABLE)

**I.1 Git is read, never written.** `ls-files`, `rev-parse`, and reading the index
are permitted. `init`, `add`, `commit`, `reset`, `stash`, `checkout`, and any
write to the object database, the index, or a ref are not — including writes the
engine intends to undo, and including a repository the engine created itself.

**I.2 No snapshot data is stored inside the user's tree.** Not in their
repository, not in their worktree, not as an object only a dangling reference
keeps alive. All of it lives under a store root the engine owns. Temporary files
the engine writes into the user's directories during a restore are the one
permitted exception, and they must be removed on every path, including failure.

**I.3 No repository is a first-class case, not a degraded one.** The engine must
work where `git init` has never been run. Git supplies file *names* and never a
boundary; nothing may fail, warn, or silently reduce its scope because there is no
repository above the workspace.

*Why:* Codex's ghost-commit feature wrote dangling commits into the user's object
database and its restore path touched the user's index; that caused a data-loss
incident and the feature was removed days later. Codex Desktop's turn-diff refs
put 102 GB of orphan objects into a 5.7 GB project and broke libgit2 clients.
Among third-party plugins solving this same problem, one runs `git init` plus
`reset --hard` in the user's repo, and one keeps its only recovery state in
unreferenced stash objects that `git gc` is free to delete. A safety net that
works only for git users, only inside repos, and only for git-visible files fails
exactly the people who need it: the reported incidents are about *untracked*
files.

### II. Deletion requires positive evidence (NON-NEGOTIABLE)

**II.1 A restore may delete a path only against a tombstone** — a record that the
target capture looked for that path and did not find it. Absence from a manifest
is never evidence, because a capture only ever sees what it was asked about.

**II.2 No scan may claim to have enumerated everything,** and no rule may depend
on such a claim. A file the engine has never observed is never touched in any
direction: not written, not deleted, not restored over.

**II.3 Ignore is symmetric and total.** An ignored path is never captured, never
restored, and never deleted. It applies to every route into the store, the edit
hook included, and it must hold from the engine's first operation — not from
whenever some other call happens to have populated the rules.

**II.4 A capture that could not read a path records nothing.** A vanished path is
a tombstone; a path that exists but cannot be read is neither an entry nor a
tombstone. A read failure verifies nothing, and the one thing it must never
become is a licence to delete.

*Why:* deletion inferred from a bounded manifest deletes files nobody looked at.
An earlier revision carried a second ground — absence from a scan that declared
itself `complete` — and it had to go, because once tracking is bounded on every
path no scan can honestly make that claim. What replaced it is an observation
rather than a premise, so it needs nothing kept true elsewhere. In the other
direction, a rollback that only overwrites and never deletes is not a rewind: one
competing plugin leaves every file created after the target sitting on disk.

### III. Every restore is reversible before it begins

**III.1 A rescue point is captured before the first byte is written,** durably and
content-addressed. Pre-restore content is never held only in memory. If the rescue
capture fails, the restore does not run — reversibility is part of the operation,
not a courtesy that degrades to a warning.

**III.2 The rescue capture always observes the target's own paths.** Whatever the
caller scanned, the target's entries and its tombstones are added to it. A plan
can only write the former and only delete the latter, so observing both makes the
capture sufficient by construction rather than by an argument about what the
caller knew.

**III.3 Restoring to a point is idempotent.** The same target resolves to the same
state from anywhere, and applying it twice changes nothing the second time.
Resolution is keyed on an identifier that is stable across branches, so rewinding
to one point twice can never land on two different states.

**III.4 The restore log is a stack.** A rewind pushes; an undo pops the record it
reverses. Targeting "the most recent restore" instead makes the second undo
reverse the first undo — files oscillating between two states while history keeps
stepping backwards, which is the worst kind of divergence because each individual
step looks correct.

**III.5 What a restore would overwrite is checked by content, not by fingerprint.**
Before an undo replaces work, the paths it would write are hashed and compared
against the state its record describes; anything that no longer matches was
changed by something else and the user is asked first. A stat comparison can miss
a same-length rewrite inside one timestamp tick, and a false "unchanged" here
means overwriting someone's work.

*Why:* III.1 is what licenses Principle II's deletions at all. III.2 was learned
the hard way: deriving the rescue scope from the caller's history alone silently
fails after a fork, because the calling thread's history contains neither the
rescue point nor anything learned after the fork, and a path recorded absent and
then put back on disk by something outside that history survived an undo without a
word.

### IV. Cost is bounded by the project, not by the tree

**IV.1 The tracked set is a union of partitions, each bounded by something other
than the size of the directory tree.** A subtree walk is not an acceptable scope:
on a working repository it enumerated 70,609 files and 116 GB, almost all of it
build output, against 6,096 files and 59 MB for the union — and the gap grows
purely from the repository being worked in.

**IV.2 Exclusion happens before ranking, never after.** A cap applied to a list
that is deduplicated afterwards is not a cap on new information: measured before
this was fixed, 97 of one partition's 100 slots went to files another partition
had already supplied.

**IV.3 A limit degrades one entry, never the checkpoint** — and a degraded entry
is recorded as seen-and-not-stored, and surfaced. A size limit that drops a path
before the capture is told it exists is not a bound; it is silent data loss
wearing the costume of one.

**IV.4 Bounds are properties of the mechanism, not knobs a user must find.** The
engine is correct and affordable where nothing has been configured, and "raise the
cap" is never the answer to a tree that is too large. Narrowing scope is the
legitimate exception: the user may declare paths invisible (II.3) and may say
which roots are the workspace, because those state what the workspace *is* rather
than repairing a bound that was chosen wrong. A setting that widens a bound is a
bound that needs fixing, not exposing.

*Why:* a subsystem whose per-turn cost scales with how long a repository has been
built in is one users turn off; one that answers a large tree by refusing to
snapshot is one that is not there when it is needed. Two competing implementations
bind themselves to the whole worktree and then bolt on a refusal threshold or a
hardcoded list of directory names — the same bet, twice, that the tree stays
small.

### V. Record what was observed; never infer

**V.1 Enumeration proposes; only the capture decides existence.** A partition that
holds a list of paths must propose every path on it and may never filter by
whether the path is on disk — a path enumerated and then not found is the evidence
II.1 runs on. A partition that discovers candidates by walking can only propose
what exists, which is exactly why a walk can never be the only partition:
tombstones come from list-shaped partitions alone, and an engine built on a walk
can restore a deleted file but can never delete a created one.

**V.2 Metadata is recorded, never fabricated.** What was not observed must be
representable as absent rather than filled in with a plausible default. An entry
built from content that arrived without a stat carries no invented timestamp and
no invented mode — and a platform that has no permission bits records that it has
none, rather than inventing bits a restore will then apply to the user's files.

**V.3 Raciness is judged once, at capture, and stored in the entry.** A file
captured within the racy window of its own last write records a fingerprint that
can never match, and is re-read until some later capture finds it settled. Judging
it at lookup time covers only the first seconds after a write.

**V.4 The content hash is the cached value, never part of the fingerprint key.**
Folding it in would require reading every file on every checkpoint — precisely the
cost the cache exists to avoid.

**V.5 One file has one spelling.** Path keys are anchored on the caller's root and
never on a spelling recovered from elsewhere. Two spellings of one file are two
entries, captured twice and restored inconsistently.

*Why:* a second write landing in the same timestamp tick as the read before it
leaves `(size, mtime)` unchanged forever — and here that is routine rather than
exotic, because an agent edit and the capture after it land milliseconds apart.
Getting V.3 wrong produces a snapshot that silently holds the previous bytes.
Getting V.2 wrong is worse: invented metadata is not inert, it is applied.

### VI. The engine knows no host

**VI.1 The library takes opaque identifiers and paths.** It must not depend on any
coding agent, editor, or harness; must not name one in its API, its constants, its
error text, or any filename it writes into the user's directories; and must not
require its caller to be one. Integrations consume the library; the library never
reaches back.

**VI.2 Content is bytes on every path that stores or reproduces it.** Capture
reads bytes, the store holds bytes, restore writes them back unchanged — no
encoding sniffing, no newline translation, no round trip through a string.
Rendering is the single exception: a CLI may decode a copy in order to print a
diff or a preview, must never write that copy back, and must say that content is
undecodable rather than quietly substituting replacement characters.

**VI.3 Coverage must not depend on the host's edit tool.** Changes made by a shell
command, a subagent, or the user's own editor are caught by the next capture's
rescan. Where the bounded partitions cannot reach a path, that gap is stated in
the interface — an engine that quietly sees only what flowed through one tool has
a hole exactly where users do their least reversible work.

*Why:* the two most-used competing plugins each bind their engine to one host's
internals, and one tracks only the paths its three edit tools touched, so a file
created by a shell command can never be recovered. Bytes matter for the same
reason: a plugin that reads and writes UTF-8 corrupts every binary file it claims
to protect.

### VII. The store outlives the binary that wrote it

**VII.1 Every durable record carries a format version,** and a reader meeting a
version it does not understand fails loudly rather than guessing. No best-effort
coercion of unknown data: an incompatible change means a new store version and an
explicit migration, never a silent reinterpretation. A missing field that defaults
to a benign-looking value is the sharpest form of this hazard — a record written
before tombstones existed must not read back as "this capture looked for nothing".

**VII.2 Every durable write is atomic, and every enumeration accounts for the
residue of an interrupted one.** A temporary file counted as a live record is
worse than one counted as garbage: it becomes a root nothing can ever collect.

**VII.3 A content-addressed record holds the state it describes and nothing
else** — not how the engine came to learn of a path, not a claim about the scan
that produced it. Anything else folded in makes two identical trees hash
differently and stop deduplicating. The same arithmetic governs change: adding a
field re-identifies every record already on disk, and giving the manifest its
tombstone set came one serialization default away from orphaning every manifest in
every existing store.

**VII.4 A property claimed in a doc comment that no code enforces is a defect.**
Where the guarantee lives and where it is explained must be the same place, or the
explanation must say where the guarantee actually is. A reader who audits the
stated property will otherwise verify the wrong thing — which is how three
separate defects in this engine's scope construction went unnoticed while every
stated rule passed inspection.

*Why:* inside a single application the reader and the writer always shipped
together and the store was disposable. A published tool has neither property: a
user's store outlives the binary that wrote it, and the first incompatible change
is unrecoverable if nothing on disk says which format it is. This is the one place
a competing implementation is ahead of this one.

### VIII. What is reachable is kept; what is unreachable is collected

**VIII.1 Reclamation is by reachability, never by age.** A record is live when
something names it, and every log, every index, and every restore record is a
root. A root the sweep does not know about is a record it deletes while something
still needs it. There are no timers and no expiry: a rewind target that worked
yesterday must not fail today because a clock passed a threshold.

**VIII.2 Nothing is collected until it has been on disk for a grace window,** and
a file whose age cannot be read counts as young. A capture publishes in more than
one step, nothing coordinates the processes, and a sweep interleaved with a
publish takes away a snapshot its writer believes it holds. This applies to
content as much as to records: content-addressed storage means a second writer may
adopt a blob without writing anything, so "no live writer can be publishing this"
is true of record identity and false of content.

**VIII.3 Deleting a conversation deletes everything held for it** — its log, the
undo records filed under it, and the captured content — and reclaims immediately
rather than eventually. That has to be wired to the delete path explicitly;
otherwise deleting a conversation removes the index to its data and keeps every
byte of it.

**VIII.4 The store is written by capture and by nothing else.** A session that
captures nothing leaves no file behind. State created merely by constructing an
engine makes this the one component that litters, and it destroys the only honest
answer to "did this session ever capture anything?", which is whether anything is
there.

*Why:* an age-based sweep breaks restore for anything older than the threshold,
which is the one thing a rewind engine exists to provide; the alternative failure,
no collection at all, is the 102 GB incident. Git answers the publish race the
same way, with a prune expiry rather than a lock: reclamation is delayed, nothing
is lost.

## Scope and Non-Goals

**In scope:** local filesystems; capture and restore of regular files and their
permission bits; a bounded tracked set; a store owned entirely by the engine; a
library, a CLI over that library, and host integrations that consume the CLI.

**Out of scope, deliberately:**

- Changes made outside the configured workspace roots by processes the engine
  never observes. Unobservable, a documented limitation of every peer tool, and to
  be stated in the interface rather than papered over.
- Any knowledge of a specific coding agent inside the engine.
- Sub-file delta storage. The blob interface is shaped so content-defined chunking
  can be added later without changing callers; adding it now is not warranted.
- Retention knobs, size caps, and prune commands as a substitute for bounded
  tracking and reachability-based collection.

**Unsettled, and to be settled before the CLI ships:** the capture path takes no
lock — VIII.2's grace window answers the publish race the way git does. A path
that *mutates the workspace* is a different question. A library inside one
long-lived process serialised restores by construction; a CLI does not, and two
invocations can restore into one directory concurrently. Either the interface
prevents that or it takes a real lock with owner identity and stale recovery. An
in-process guard is not a lock.

## Quality Gates

- **Every rule above must be pinned by a test.** A rule nothing verifies has
  already been broken once without anyone noticing. Where a rule is unverified,
  that is tracked in [compliance.md](compliance.md) as a defect, not tolerated as
  a style.
- **The dangerous direction gets the test.** For II.4 that means proving a path
  the engine could not read is never treated as absent. The code is right today
  and nothing checks it, three lines from a rule that is tested three ways.
- `cargo test`, `cargo clippy --all-targets`, and `cargo fmt --check` are clean
  before every commit. The clippy deny list, the rustfmt configuration, and the
  pinned toolchain are part of the contract, not local preference.
- **Published API is a permanent commitment.** No bool or bare `Option` parameters
  that make a call site read `f(false, None, true)`; prefer enums, named
  constructors, and newtypes so the call site says what it means. Nothing is
  exported that a consumer cannot actually call.
- Integration tests exercise the engine through its public surface, the way a
  consumer would, and assert whole values rather than field by field.

## Governance

This constitution supersedes convenience. A change that violates a rule is not
merged because it is smaller, faster, or already written; it is merged after the
rule is amended, or not at all.

Amendments require what the rules themselves have: the failure the new rule
prevents, stated concretely.

Every plan and specification is checked against these rules before implementation
begins. A design that cannot satisfy one is recorded as an argued exception with
its cost stated, in [compliance.md](compliance.md), rather than left for a reader
to find in the code. That ledger is part of this constitution's enforcement, not a
backlog: an entry may be deferred, but it may not be forgotten, and no release
ships with an entry marked as blocking still open.

The project descends from an engine written inside a fork of OpenAI Codex and
shares no code path with it going forward. Fixes do not flow in either direction
automatically; a defect found here that also exists there is reported, not synced.

**Version**: 1.0.0 | **Ratified**: 2026-08-22 | **Last Amended**: 2026-08-22
