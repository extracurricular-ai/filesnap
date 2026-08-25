#![allow(clippy::unwrap_used)]

use super::*;

/// The version directory this build owns, and one above it. Derived rather
/// than spelled, so bumping the format does not need a test rewrite — and so
/// a test cannot go on asserting a version nothing writes any more.
fn ours() -> String {
    format!("v{FORMAT_VERSION}")
}

fn newer() -> String {
    format!("v{}", FORMAT_VERSION + 1)
}
use pretty_assertions::assert_eq;

/// Two spellings of one directory must be one partition. This is the case
/// V.5 exists for, and on macOS it is the default rather than an edge case:
/// `/var` is a symlink to `/private/var`, so a session opened by one name and
/// resumed by the other would otherwise have two separate histories.
#[test]
fn a_symlinked_spelling_resolves_to_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("project");
    std::fs::create_dir_all(real.join("src")).unwrap();

    let direct = WorkspaceKey::of(&real).unwrap();
    let via_dots = WorkspaceKey::of(&real.join("src").join("..")).unwrap();
    assert_eq!(
        direct, via_dots,
        "`.` and `..` are not a different workspace"
    );

    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            WorkspaceKey::of(&link).unwrap(),
            direct,
            "a symlinked path is the same workspace, not a second one"
        );
    }
}

#[test]
fn distinct_directories_get_distinct_keys() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    assert_ne!(WorkspaceKey::of(&a).unwrap(), WorkspaceKey::of(&b).unwrap());
}

/// Canonicalization needs the directory to exist, and there is deliberately no
/// fallback to the literal path: a fallback would give two spellings two
/// partitions and neither would look wrong.
#[test]
fn a_missing_workspace_is_an_error_rather_than_a_guess() {
    let dir = tempfile::tempdir().unwrap();
    assert!(WorkspaceKey::of(&dir.path().join("nope")).is_err());
}

#[test]
fn the_store_root_is_created_under_a_version_directory() {
    let home = tempfile::tempdir().unwrap();
    let root = store_root(home.path()).unwrap();
    assert_eq!(root, home.path().join("filesnap").join(ours()));
    assert!(root.is_dir());
    // Opening again finds the same root rather than making a second one.
    assert_eq!(store_root(home.path()).unwrap(), root);
}

/// A store written by a newer build is refused rather than guessed at
/// (VII.1). The alternative — reading it anyway — is the failure the whole
/// versioning scheme exists to prevent.
#[test]
fn a_store_only_a_newer_build_understands_is_refused() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("filesnap").join(newer())).unwrap();

    // Destructured rather than matched against literals: a pattern cannot
    // hold a constant, and spelling the numbers is how a test ends up
    // asserting a version the code stopped using.
    match store_root(home.path()).unwrap_err() {
        SnapshotError::UnknownStoreVersion {
            found, supported, ..
        } => {
            assert_eq!((found, supported), (FORMAT_VERSION + 1, FORMAT_VERSION));
        }
        other => panic!("expected a refusal naming both versions, got {other:?}"),
    }
}

/// Our own version alongside a newer one is not a conflict — this build reads
/// what it wrote and leaves the rest alone.
#[test]
fn a_newer_version_beside_our_own_is_not_an_error() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("filesnap").join(ours())).unwrap();
    std::fs::create_dir_all(home.path().join("filesnap").join(newer())).unwrap();

    assert_eq!(
        store_root(home.path()).unwrap(),
        home.path().join("filesnap").join(ours())
    );
}

/// Enumerating partitions is a whitelist, not a blacklist: a key is a hex
/// digest and anything else is residue. A blacklist stops the residue it
/// knows about and admits the next kind (D9).
#[test]
fn partition_enumeration_admits_only_well_formed_keys() {
    let home = tempfile::tempdir().unwrap();
    let root = store_root(home.path()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key = WorkspaceKey::of(dir.path()).unwrap();

    std::fs::create_dir_all(partition_dir(&root, &key)).unwrap();
    for residue in ["scratch", &format!("{}.tmp", key.as_str()), "0011"] {
        std::fs::create_dir_all(root.join("workspaces").join(residue)).unwrap();
    }

    assert_eq!(all_partitions(&root).unwrap(), vec![key]);
}

#[test]
fn an_empty_store_has_no_partitions() {
    let home = tempfile::tempdir().unwrap();
    let root = store_root(home.path()).unwrap();
    assert_eq!(all_partitions(&root).unwrap(), Vec::new());
}
