//! Checkpoint manifests: the inventory of one captured state.
//!
//! A manifest maps file paths to `(mode, size, mtime, content hash)`.
//! The `(size, mtime)` pair doubles as the persistent stat cache: a later
//! checkpoint reuses the recorded hash for any file whose stat fingerprint
//! is unchanged, so unchanged files are never re-read.
//!
//! Manifests are themselves content-addressed: the manifest id is the
//! SHA-256 of the canonical JSON serialization (`BTreeMap` keys give a
//! stable order), so identical states dedup to a single stored manifest.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::blob::BlobStore;
use crate::error::Result;
use crate::error::SnapshotError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Unix permission bits (`mode & 0o7777`), or `None` when they were never
    /// observed.
    ///
    /// `None` is not "default permissions" — it is the absence of an
    /// observation, and a restore leaves such a file's permissions exactly as
    /// it finds them. Two things produce it: content that arrived from an
    /// edit rather than from the filesystem, which has no stat behind it at
    /// all, and a platform with no permission bits to report.
    ///
    /// It was `u32` with an invented `0o644` in both cases, which was not
    /// inert: the planner compares mode and the applier chmods, so restoring
    /// a pre-edit image of an executable script stripped its `+x`, and a file
    /// whose content already matched was rewritten anyway because an invented
    /// mode never equals a real one.
    pub mode: Option<u32>,
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    /// SHA-256 of the file content, stored in the blob store.
    pub hash: String,
}

impl FileEntry {
    /// Stat-cache check: does `meta` match this entry's fingerprint?
    pub fn stat_matches(&self, meta: &fs::Metadata) -> bool {
        // A zero mtime is the marker for an entry whose fingerprint was never
        // trustworthy — captured within its own write's timestamp tick, or
        // reconstructed from a pre-edit image that had no stat at all. Such an
        // entry must never satisfy the fast path, including against a file
        // that genuinely carries an epoch mtime.
        if (self.mtime_secs, self.mtime_nanos) == (0, 0) {
            return false;
        }
        let (secs, nanos) = mtime_parts(meta);
        self.size == meta.len() && self.mtime_secs == secs && self.mtime_nanos == nanos
    }
}

/// Extract `(secs, nanos)` of the mtime relative to the unix epoch.
/// Pre-epoch or unreadable mtimes collapse to `(0, 0)`, which simply
/// disables the stat-cache fast path for that file.
pub fn mtime_parts(meta: &fs::Metadata) -> (i64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs() as i64, d.subsec_nanos()))
}

/// Permission bits to record for a file (`mode & 0o7777`), or `None` where
/// the platform has none to report.
///
/// `None` off-unix rather than a plausible `0o644`: inventing one would make
/// a restore chmod every file it wrote on Windows to permissions nothing ever
/// observed.
pub fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }
    // Windows has exactly one observable permission — `FILE_ATTRIBUTE_READONLY`
    // — so that is what `mode` records there. Mapping it onto `0o444` / `0o644`
    // is lossy in one direction and exact in the other: everything Windows can
    // tell us survives, and a mode written on unix still reads as "writable or
    // not" here. `None` would have thrown the bit away in both directions,
    // which is how a restore silently made a read-only file writable.
    #[cfg(windows)]
    {
        Some(if meta.permissions().readonly() {
            0o444
        } else {
            0o644
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version of this record.
    ///
    /// The store's own directory carries a version too, and this is the finer
    /// half of the same guarantee: the path catches a store a different build
    /// wrote, and this catches a record that build left inside one of ours —
    /// which is what a migration interrupted halfway looks like.
    pub version: u32,
    /// Path → entry. Paths are stored as strings (lossy UTF-8) so
    /// manifests serialize portably; keys are absolute paths.
    pub entries: BTreeMap<String, FileEntry>,
    /// Paths this capture looked for and did not find.
    ///
    /// The only evidence a restore may delete on. Earlier designs inferred it
    /// instead — "the scan covered this directory exhaustively, so absence
    /// proves non-existence" — which made the inference only as sound as the
    /// scan was exhaustive, and therefore required scanning everything. That
    /// cost was unbounded and the claim was fragile: a manifest also carries
    /// paths the edit hook picked up from anywhere on disk, which no scan ever
    /// covered.
    ///
    /// Recording the observation directly costs one set and needs no premise.
    /// A path the capture was asked about and found *missing* appears here,
    /// whether it came from the index, from recency, or from an edit that
    /// created a file where none was.
    ///
    /// **Not every path the capture was asked about is accounted for**, and
    /// the earlier wording here said it was. Three branches record neither an
    /// entry nor a tombstone: a stat error that is not `NotFound`, a
    /// non-regular file, and a read failure. That is exactly right — II.4
    /// requires it, because a failed read verifies nothing and a tombstone is
    /// a licence to delete — but II.1's deletion rule reads off this comment,
    /// so an auditor checking the stronger claim would be verifying something
    /// the code does not promise (C10, VII.4). Those paths are counted in
    /// [`crate::CheckpointStats::dropped`] instead.
    /// `default` is safe here only because `version` is checked first. An
    /// empty set is omitted from the JSON, so reading has to tolerate the key
    /// being absent — and without the version guard that same tolerance would
    /// silently read a record from a build that predated tombstones as "this
    /// capture looked for nothing", voiding every deletion it licensed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub absent: BTreeSet<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: crate::workspace::FORMAT_VERSION,
            entries: BTreeMap::new(),
            absent: BTreeSet::new(),
        }
    }
}

impl Manifest {
    /// Content-addressed id: SHA-256 of the canonical JSON serialization.
    pub fn id(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(BlobStore::hash_bytes(&bytes))
    }
}

pub struct ManifestStore {
    root: PathBuf,
}

impl ManifestStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| SnapshotError::io(&root, e))?;
        Ok(Self { root })
    }

    /// Persist `manifest`, returning its id. Idempotent.
    pub fn save(&self, manifest: &Manifest) -> Result<String> {
        let id = manifest.id()?;
        let path = self.manifest_path(&id)?;
        if path.exists() {
            // Manifest ids are content-addressed too, so this dedups exactly
            // as blobs do — and carries the same hazard, which the
            // constitution states for content only. A live session that
            // recaptures unchanged state re-derives an existing id and writes
            // nothing, leaving that manifest instantly collectable while it
            // is about to become live. Its log entry would then point at a
            // manifest that is gone and the session would stop capturing
            // entirely. See `BlobStore::store_bytes`.
            crate::sweep::freshen(&path);
            return Ok(id);
        }
        let tmp = crate::sweep::tmp_name(&path);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        fs::write(&tmp, bytes).map_err(|e| SnapshotError::io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| SnapshotError::io(&path, e))?;
        Ok(id)
    }

    pub fn load(&self, id: &str) -> Result<Manifest> {
        let path = self.manifest_path(id)?;
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SnapshotError::MissingManifest(id.to_string())
            } else {
                SnapshotError::io(&path, e)
            }
        })?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        // Refuse rather than guess. A record this build does not understand
        // may mean anything at all, and the one thing it must not do is look
        // like a record that means something else.
        if manifest.version != crate::workspace::FORMAT_VERSION {
            return Err(SnapshotError::UnknownRecordVersion {
                kind: "manifest",
                id: id.to_string(),
                found: manifest.version,
                supported: crate::workspace::FORMAT_VERSION,
            });
        }
        Ok(manifest)
    }

    /// Where `id` lives, so the sweep can ask how old it is.
    pub(crate) fn path_for(&self, id: &str) -> Result<PathBuf> {
        self.manifest_path(id)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.manifest_path(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::io(&path, e)),
        }
    }

    /// Enumerate stored manifest ids (for garbage collection sweeps).
    pub fn ids(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let entries = fs::read_dir(&self.root).map_err(|e| SnapshotError::io(&self.root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| SnapshotError::io(&self.root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json")
                && crate::id::is_object_name(id)
            {
                out.insert(id.to_string());
            }
        }
        Ok(out)
    }

    /// Where manifest `id` lives, once proven to be one. See
    /// [`crate::blob::BlobStore`]'s equivalent for why an internally minted
    /// id is still checked.
    fn manifest_path(&self, id: &str) -> Result<PathBuf> {
        crate::id::validate_object("manifest id", id)?;
        Ok(self.root.join(format!("{id}.json")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use pretty_assertions::assert_eq;

    fn entry(hash: &str) -> FileEntry {
        FileEntry {
            mode: Some(0o644),
            size: 1,
            mtime_secs: 10,
            mtime_nanos: 0,
            hash: hash.to_string(),
        }
    }

    #[test]
    fn id_is_stable_and_content_addressed() {
        let mut a = Manifest::default();
        a.entries.insert("/x".to_string(), entry("h1"));
        let mut b = Manifest::default();
        b.entries.insert("/x".to_string(), entry("h1"));
        assert_eq!(a.id().unwrap(), b.id().unwrap());

        b.entries.insert("/y".to_string(), entry("h2"));
        assert_ne!(a.id().unwrap(), b.id().unwrap());
    }

    #[test]
    fn save_load_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path().join("manifests")).unwrap();

        let mut m = Manifest::default();
        m.entries.insert("/a".to_string(), entry("abc"));

        let id1 = store.save(&m).unwrap();
        let id2 = store.save(&m).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(store.ids().unwrap().len(), 1);
        assert_eq!(store.load(&id1).unwrap(), m);
    }

    #[test]
    fn missing_manifest_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path().join("manifests")).unwrap();
        assert!(matches!(
            store.load(&"a".repeat(64)),
            Err(SnapshotError::MissingManifest(_))
        ));
    }

    /// A manifest id that is not the shape we mint is refused before it
    /// becomes a path. See the equivalent in `blob.rs`.
    #[test]
    fn a_malformed_manifest_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path().join("manifests")).unwrap();
        assert!(matches!(
            store.load("nope"),
            Err(SnapshotError::InvalidId { .. })
        ));
    }

    #[test]
    fn stat_matches_tracks_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"ab").unwrap();
        let meta = fs::metadata(&file).unwrap();
        let (secs, nanos) = mtime_parts(&meta);

        let e = FileEntry {
            mode: mode_of(&meta),
            size: 2,
            mtime_secs: secs,
            mtime_nanos: nanos,
            hash: "h".to_string(),
        };
        assert!(e.stat_matches(&meta));

        fs::write(&file, b"abc").unwrap();
        let meta2 = fs::metadata(&file).unwrap();
        assert!(!e.stat_matches(&meta2));

        // Both terms have to be able to decide alone. The rewrite above moves
        // size *and* mtime, so either one settles it — and with only that,
        // the two mtime terms could be deleted with the whole suite green,
        // while a same-length edit in place (`sed -i`, a flag flipped, one
        // character) hit the cache, kept the stale hash, and was never
        // captured.
        //
        // The *recorded* stamp is varied against an unchanged file rather
        // than the file against an unchanged entry: no dependence on the
        // filesystem's timestamp granularity, and no sleep. Neither variant
        // collides with the (0, 0) never-trustworthy sentinel, because `secs`
        // is a real wall-clock timestamp.
        let stale_secs = FileEntry {
            mtime_secs: secs - 1,
            ..e.clone()
        };
        assert!(
            !stale_secs.stat_matches(&meta),
            "a same-size file whose mtime seconds moved must miss the stat cache"
        );
        let stale_nanos = FileEntry {
            mtime_nanos: nanos ^ 1,
            ..e.clone()
        };
        assert!(
            !stale_nanos.stat_matches(&meta),
            "a same-size file whose mtime nanos moved must miss the stat cache"
        );

        // And the mirror, or size stops being load-bearing the moment mtime
        // becomes so. A file truncated or extended inside one timestamp tick
        // is the case this covers.
        let wrong_size = FileEntry { size: 3, ..e };
        assert!(
            !wrong_size.stat_matches(&meta),
            "a file whose size moved must miss the stat cache whatever its mtime"
        );
    }

    /// A record this build does not understand is refused, not guessed at.
    /// Reading it anyway is the failure versioning exists to prevent: the
    /// same bytes can mean different things in different formats, and the
    /// one thing an unknown record must not do is look like a known one.
    #[test]
    fn a_manifest_from_an_unknown_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path()).unwrap();

        let mut manifest = Manifest::default();
        manifest.entries.insert("/a".into(), entry("h"));
        let id = store.save(&manifest).unwrap();
        assert_eq!(store.load(&id).unwrap(), manifest);

        // Rewrite it as a record from a build that does not exist yet.
        let raw = fs::read(dir.path().join(format!("{id}.json"))).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        value["version"] = serde_json::json!(crate::workspace::FORMAT_VERSION + 1);
        fs::write(
            dir.path().join(format!("{id}.json")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();

        let err = store.load(&id).unwrap_err();
        assert!(
            matches!(
                err,
                SnapshotError::UnknownRecordVersion {
                    kind: "manifest",
                    ..
                }
            ),
            "expected a refusal naming the record, got {err:?}"
        );
    }

    /// The tombstone set is omitted from the JSON when empty, so reading has
    /// to tolerate the key being absent — and that tolerance is only safe
    /// because the version is checked first. Without it, a record from a
    /// build that predated tombstones would read as "this capture looked for
    /// nothing" and quietly void every deletion it had licensed.
    #[test]
    fn an_empty_tombstone_set_round_trips_without_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path()).unwrap();

        let mut manifest = Manifest::default();
        manifest.entries.insert("/a".into(), entry("h"));
        let id = store.save(&manifest).unwrap();

        let raw = fs::read(dir.path().join(format!("{id}.json"))).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(
            value.get("absent").is_none(),
            "an empty set is not written at all"
        );
        assert_eq!(store.load(&id).unwrap().absent, BTreeSet::new());
    }
}
