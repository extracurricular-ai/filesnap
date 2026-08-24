//! What each sweep is allowed to reach, and what it must leave alone.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::*;
use crate::manifest::FileEntry;
use crate::manifest::Manifest;
use pretty_assertions::assert_eq;

struct Store {
    dir: tempfile::TempDir,
    refs: RefStore,
    turns: TurnIndex,
    manifests: ManifestStore,
}

impl Store {
    /// Paths are spelled here rather than exposed by the implementation:
    /// these tests must be able to damage a record, and a test-only accessor
    /// on the real type would outlive the test that needed it.
    fn log_path(&self, thread_id: &str) -> PathBuf {
        self.dir
            .path()
            .join("refs")
            .join(format!("{thread_id}.json"))
    }

    fn restore_path(&self, thread_id: &str) -> PathBuf {
        self.dir
            .path()
            .join("restores")
            .join(format!("{thread_id}.json"))
    }
}

fn store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    Store {
        refs: RefStore::open(dir.path().join("refs")).unwrap(),
        turns: TurnIndex::open(dir.path()).unwrap(),
        manifests: ManifestStore::open(dir.path().join("manifests")).unwrap(),
        dir,
    }
}

/// A manifest naming one made-up hash, persisted.
fn manifest(s: &Store, hash: &str) -> String {
    let mut m = Manifest::default();
    m.entries.insert(
        "/f".into(),
        FileEntry {
            mode: Some(0o644),
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
            hash: hash.to_string(),
        },
    );
    s.manifests.save(&m).unwrap()
}

fn age_out(path: &Path) {
    let when = SystemTime::now() - GC_GRACE - Duration::from_secs(60);
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
}

fn ids(s: &Store) -> BTreeSet<String> {
    s.manifests.ids().unwrap().into_iter().collect()
}

/// Delete removes only what the sessions it was given had named.
///
/// The previous implementation reconciled the whole turn index by global
/// elimination, so deleting one conversation could unlink a live session's
/// turn entry. Nothing here enumerates: every candidate came from a log that
/// is now gone.
#[test]
fn a_prune_touches_nothing_the_doomed_sessions_did_not_name() {
    let s = store();
    let doomed_id = manifest(&s, "hash-doomed");
    let bystander_id = manifest(&s, "hash-bystander");

    s.refs
        .append("doomed", "turn-doomed".into(), doomed_id.clone())
        .unwrap();
    s.turns.set_turn("turn-doomed", &doomed_id).unwrap();
    s.refs
        .append("bystander", "turn-live".into(), bystander_id.clone())
        .unwrap();
    s.turns.set_turn("turn-live", &bystander_id).unwrap();

    // Delete reads what the doomed log named, then unlinks it.
    let doomed_turns = BTreeSet::from([crate::refs::safe_file_name("turn-doomed")]);
    let doomed_manifests = BTreeSet::from([doomed_id]);
    s.refs.remove("doomed").unwrap();

    let stats = prune_sessions(
        &s.refs,
        &s.turns,
        &s.manifests,
        &doomed_turns,
        &doomed_manifests,
    )
    .unwrap();

    assert_eq!(stats.manifests_removed, 1);
    assert_eq!(ids(&s), BTreeSet::from([bystander_id.clone()]));
    assert_eq!(
        s.turns.manifest_for_turn("turn-live").unwrap(),
        Some(bystander_id),
        "the bystander's turn entry is untouched"
    );
    assert_eq!(s.turns.manifest_for_turn("turn-doomed").unwrap(), None);
}

/// A manifest a surviving session still names is kept, even though a deleted
/// session named it too. Manifests are shared by content, not by lineage.
#[test]
fn a_prune_keeps_what_a_survivor_still_names() {
    let s = store();
    let shared = manifest(&s, "hash-shared");
    s.refs
        .append("doomed", "turn-a".into(), shared.clone())
        .unwrap();
    s.refs
        .append("survivor", "turn-b".into(), shared.clone())
        .unwrap();
    s.refs.remove("doomed").unwrap();

    let stats = prune_sessions(
        &s.refs,
        &s.turns,
        &s.manifests,
        &BTreeSet::from([crate::refs::safe_file_name("turn-a")]),
        &BTreeSet::from([shared.clone()]),
    )
    .unwrap();

    assert_eq!((stats.manifests_kept, stats.manifests_removed), (1, 0));
    assert!(s.manifests.load(&shared).is_ok());
}

/// One unreadable log defers reclamation instead of failing the call, and
/// removes nothing at all.
///
/// This is what keeps delete free of preconditions (D9): a corrupt log
/// belonging to another session cannot make deleting this one fail, and
/// cannot make it delete a manifest that log may still name.
#[test]
fn an_unreadable_log_defers_reclamation_rather_than_guessing() {
    let s = store();
    let doomed_id = manifest(&s, "hash-doomed");
    s.refs
        .append("doomed", "turn-a".into(), doomed_id.clone())
        .unwrap();
    s.refs
        .append("other", "turn-b".into(), manifest(&s, "hash-other"))
        .unwrap();
    s.refs.remove("doomed").unwrap();

    // A third session's log is truncated mid-write.
    fs::write(s.log_path("corrupt"), b"{\"version\":1,\"entr").unwrap();

    let before = ids(&s);
    let stats = prune_sessions(
        &s.refs,
        &s.turns,
        &s.manifests,
        &BTreeSet::from([crate::refs::safe_file_name("turn-a")]),
        &BTreeSet::from([doomed_id]),
    )
    .unwrap();

    assert_eq!(stats.manifests_removed, 0);
    assert_eq!(
        ids(&s),
        before,
        "nothing is removed on an incomplete answer"
    );
}

/// The collector finds what a crash left behind — a manifest no log, turn or
/// undo record names — which the scoped prune by design cannot.
#[test]
fn collection_reclaims_the_orphan_a_prune_cannot_see() {
    let s = store();
    let live = manifest(&s, "hash-live");
    let orphan = manifest(&s, "hash-orphan");
    s.refs.append("t", "turn-a".into(), live.clone()).unwrap();
    s.turns.set_turn("turn-a", &live).unwrap();

    // Young: collection spares what it cannot yet judge.
    let stats = collect_partition(&s.refs, &s.turns, &s.manifests).unwrap();
    assert_eq!(stats.manifests_removed, 0);

    age_out(&s.manifests.path_for(&orphan));
    age_out(&s.manifests.path_for(&live));
    let stats = collect_partition(&s.refs, &s.turns, &s.manifests).unwrap();
    assert_eq!((stats.manifests_kept, stats.manifests_removed), (1, 1));
    assert_eq!(ids(&s), BTreeSet::from([live]));
}

/// **Neither sweep may remove content.** The signature is the guard: there is
/// no blob store to pass, so a partition-scoped answer cannot be applied to
/// the shared one. Both used to take `&BlobStore` and delete from it.
#[test]
fn a_partition_sweep_reports_no_content_reclaimed() {
    let s = store();
    let id = manifest(&s, "hash-1");
    s.refs.append("t", "turn-a".into(), id).unwrap();

    let swept = collect_partition(&s.refs, &s.turns, &s.manifests).unwrap();
    let pruned = prune_sessions(
        &s.refs,
        &s.turns,
        &s.manifests,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
    .unwrap();

    assert_eq!((swept.blobs_kept, swept.blobs_removed), (0, 0));
    assert_eq!((pruned.blobs_kept, pruned.blobs_removed), (0, 0));
}

/// An undo record whose session has no log is residue, not a root.
///
/// Left in place it is read as a root forever, pinning both manifests it
/// names — the leak that made deleting the pair together necessary (D11).
#[test]
fn collection_drops_an_undo_record_whose_session_is_gone() {
    let s = store();
    let target = manifest(&s, "hash-target");
    let safety = manifest(&s, "hash-safety");
    s.turns
        .push_restore(
            "vanished",
            crate::refs::RestoreRecord {
                target_manifest_id: target.clone(),
                safety_manifest_id: safety.clone(),
            },
        )
        .unwrap();

    // Young residue is left alone: a rewind writes the undo record under a
    // session whose own log may be a moment away.
    assert!(s.turns.orphan_restore_logs(&s.refs).unwrap().is_empty());

    for p in [
        s.restore_path("vanished"),
        s.manifests.path_for(&target),
        s.manifests.path_for(&safety),
    ] {
        age_out(&p);
    }

    let stats = collect_partition(&s.refs, &s.turns, &s.manifests).unwrap();
    assert_eq!(
        stats.manifests_removed, 2,
        "the record went, and took its two manifests with it in the same pass"
    );
    assert_eq!(ids(&s), BTreeSet::new());
}

/// `.tmp` residue past the window is reclaimed. Nothing used to remove one:
/// every enumeration merely skipped it, which made a stray permanent (C4).
#[test]
fn residue_is_reclaimed_once_it_is_settled() {
    let dir = tempfile::tempdir().unwrap();
    let stray = dir.path().join("half-written.tmp");
    let real = dir.path().join("record.json");
    fs::write(&stray, b"partial").unwrap();
    fs::write(&real, b"{}").unwrap();

    assert_eq!(sweep_residue(dir.path()), 0, "fresh residue may be in use");

    age_out(&stray);
    age_out(&real);
    assert_eq!(sweep_residue(dir.path()), 1);
    assert!(!stray.exists());
    assert!(real.exists(), "a real record is not residue");
}
