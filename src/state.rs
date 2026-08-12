// Persistent state: change history, metadata cache, and rollback.
//
// Backed by Navidrome's per-plugin KVStore (SQLite) for history + metadata
// cache, and the storage dir for backup files (original nfo/tag snapshots).
// Every applied album is recorded as an `ApplyRecord`; a run can be rolled
// back by reversing those records (restore backups, rename files back, move
// folders back).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One file renamed during an album apply (names only; the dir is on the record).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileRename {
    pub from: String,
    pub to: String,
}

/// A complete album-apply action, stored so it can be audited and reversed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyRecord {
    pub seq: i64,
    pub ts: i64,
    pub run_id: String,
    pub library_id: i32,
    pub from_dir: String,
    pub to_dir: String,
    pub file_renames: Vec<FileRename>,
    pub dir_sidecars: Vec<String>,
    /// Path (relative to the library root) of the album.nfo that was written.
    pub nfo_written: Option<String>,
    /// KVStore key of the pre-write nfo backup content.
    pub nfo_backup: Option<String>,
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Stable FNV-1a 64-bit hash for cache keys (deterministic across restarts).
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1_0000_0000_01b3);
    }
    h
}

/// Reverse one applied album: restore file names, restore the nfo content from
/// its backup, then move the folder back to its original location.
pub fn rollback_record(root: &Path, rec: &ApplyRecord, nfo_content: Option<Vec<u8>>) -> Result<(), String> {
    // 1. Rename files back (new -> old) inside the target dir.
    for fr in rec.file_renames.iter().rev() {
        if fr.from.eq_ignore_ascii_case(&fr.to) {
            continue;
        }
        let to_path = root.join(&rec.to_dir).join(&fr.to);
        let from_path = root.join(&rec.to_dir).join(&fr.from);
        if to_path.exists() && !from_path.exists() {
            std::fs::rename(&to_path, &from_path).map_err(|e| {
                format!("restore file {} -> {}: {e}", fr.to, fr.from)
            })?;
        }
    }
    // 2. Restore the original nfo content.
    if let (Some(written), Some(content)) = (&rec.nfo_written, nfo_content) {
        let target = root.join(written);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, content)
            .map_err(|e| format!("restore nfo {written}: {e}"))?;
    }
    // 3. Move the folder back.
    if !rec.from_dir.eq_ignore_ascii_case(&rec.to_dir) {
        let src = root.join(&rec.to_dir);
        let dst = root.join(&rec.from_dir);
        if src.exists() && !dst.exists() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::rename(&src, &dst)
                .map_err(|e| format!("move {} -> {}: {e}", rec.to_dir, rec.from_dir))?;
        }
    }
    Ok(())
}

/// Host-backed persistence. Only available on the wasm target.
#[cfg(target_arch = "wasm32")]
pub mod host_state {
    use super::*;
    use nd_pdk::host;

    pub fn new_run_id() -> String {
        format!("run-{}", now_ts())
    }

    /// Per-run monotonically increasing sequence (concurrency is 1, so a
    /// read-increment-write is safe).
    pub fn next_seq(run_id: &str) -> Result<i64, String> {
        let key = format!("seq:{run_id}");
        let n = match host::kvstore::get(&key).map_err(|e| e.to_string())? {
            Some(v) => String::from_utf8_lossy(&v).parse::<i64>().unwrap_or(0),
            None => 0,
        };
        let n = n + 1;
        host::kvstore::set(&key, n.to_string().into_bytes()).map_err(|e| e.to_string())?;
        Ok(n)
    }

    pub fn record_apply(rec: &mut ApplyRecord) -> Result<(), String> {
        rec.seq = next_seq(&rec.run_id)?;
        let key = format!("apply:{}:{}", rec.run_id, rec.seq);
        let json = serde_json::to_vec(rec).map_err(|e| e.to_string())?;
        host::kvstore::set(&key, json).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_run_applies(run_id: &str) -> Result<Vec<ApplyRecord>, String> {
        let keys = host::kvstore::list(&format!("apply:{run_id}:")).map_err(|e| e.to_string())?;
        let values = host::kvstore::get_many(keys).map_err(|e| e.to_string())?;
        let mut recs: Vec<ApplyRecord> = values
            .values()
            .filter_map(|v| serde_json::from_slice(v).ok())
            .collect();
        recs.sort_by_key(|r| r.seq);
        Ok(recs)
    }

    /// Cache a fetched-metadata/artwork-URL value for a source+query for 7 days.
    pub fn cache_meta(source: &str, query: &str, value: &str) -> Result<(), String> {
        let key = format!("meta:{}:{}", source, fnv1a64(query));
        host::kvstore::set_with_ttl(&key, value.as_bytes().to_vec(), 7 * 24 * 3600)
            .map_err(|e| e.to_string())
    }

    pub fn get_cached_meta(source: &str, query: &str) -> Result<Option<String>, String> {
        let key = format!("meta:{}:{}", source, fnv1a64(query));
        Ok(host::kvstore::get(&key)
            .map_err(|e| e.to_string())?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    pub fn mark_rollback_done(run_id: &str) -> Result<(), String> {
        host::kvstore::set(&format!("rollback:done:{run_id}"), b"1".to_vec())
            .map_err(|e| e.to_string())
    }

    pub fn rollback_done(run_id: &str) -> bool {
        host::kvstore::get(&format!("rollback:done:{run_id}"))
            .map(|o| o.is_some())
            .unwrap_or(false)
    }

    /// Roll back a run's applies in reverse order. Backups are loaded from the
    /// KVStore (nfo/tag content) so no separate file store is required.
    pub fn run_rollback(root: &Path, recs: &[ApplyRecord]) -> Result<(), String> {
        let mut errors = Vec::new();
        for rec in recs.iter().rev() {
            let nfo_content = rec
                .nfo_backup
                .as_deref()
                .and_then(|key| host::kvstore::get(key).ok().flatten());
            if let Err(e) = rollback_record(root, rec, nfo_content) {
                errors.push(format!("seq {}: {e}", rec.seq));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Helper for `host_state`: build a backup key for a run+seq snapshot.
pub fn backup_key(run_id: &str, seq: i64) -> String {
    format!("backup:{run_id}:{seq}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("nd-organizer-state-{tag}-{}", std::process::id()));
        let storage = std::env::temp_dir().join(format!("nd-organizer-state-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&storage);
        fs::create_dir_all(root.join("Album")).unwrap();
        fs::create_dir_all(storage.join("backups")).unwrap();
        (root, storage)
    }

    #[test]
    fn apply_record_round_trips() {
        let rec = ApplyRecord {
            seq: 1,
            ts: 123,
            run_id: "run-1".into(),
            library_id: 1,
            from_dir: "Artist/Album".into(),
            to_dir: "Artist/Album (2020)".into(),
            file_renames: vec![FileRename { from: "01 - Old.flac".into(), to: "01 - New.flac".into() }],
            dir_sidecars: vec!["album.nfo".into()],
            nfo_written: Some("Artist/Album (2020)/album.nfo".into()),
            nfo_backup: Some("backups/run-1-1-album.nfo".into()),
        };
        let json = serde_json::to_vec(&rec).unwrap();
        let back: ApplyRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn fnv1a64_is_stable() {
        assert_eq!(fnv1a64("musicbrainz|Pink Floyd|The Wall"), fnv1a64("musicbrainz|Pink Floyd|The Wall"));
        assert_ne!(fnv1a64("a"), fnv1a64("b"));
    }

    #[test]
    fn rollback_restores_files_nfo_and_folder() {
        let (root, storage) = fixture("rb");
        fs::write(root.join("Album/01 - Old.flac"), b"audio").unwrap();
        fs::write(root.join("Album/album.nfo"), b"<album>NEW</album>").unwrap();
        fs::write(storage.join("backups/run-1-1-album.nfo"), b"<album>ORIGINAL</album>").unwrap();

        // Simulate: dir moved, file renamed, nfo rewritten.
        let rec = ApplyRecord {
            seq: 1,
            ts: 0,
            run_id: "run-1".into(),
            library_id: 1,
            from_dir: "Album".into(),
            to_dir: "Artist/Album (2020)".into(),
            file_renames: vec![FileRename { from: "01 - Old.flac".into(), to: "01 - New.flac".into() }],
            dir_sidecars: vec!["album.nfo".into()],
            nfo_written: Some("Artist/Album (2020)/album.nfo".into()),
            nfo_backup: Some("backup:run-1:1".into()),
        };
        // Build the "after apply" state: files moved+renamed to the new dir and
        // the old dir pruned (as apply_plan would do).
        fs::create_dir_all(root.join("Artist/Album (2020)")).unwrap();
        fs::write(root.join("Artist/Album (2020)/01 - New.flac"), b"audio").unwrap();
        fs::write(root.join("Artist/Album (2020)/album.nfo"), b"<album>NEW</album>").unwrap();
        fs::remove_dir_all(root.join("Album")).unwrap();

        rollback_record(&root, &rec, Some(b"<album>ORIGINAL</album>".to_vec())).unwrap();

        assert!(root.join("Album/01 - Old.flac").exists(), "file renamed back");
        assert_eq!(fs::read_to_string(root.join("Album/album.nfo")).unwrap(), "<album>ORIGINAL</album>");
        assert!(!root.join("Artist/Album (2020)").exists(), "target dir moved back");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&storage);
    }

    #[test]
    fn rollback_is_idempotent_when_already_undone() {
        let (root, storage) = fixture("idem");
        let rec = ApplyRecord {
            seq: 1,
            ts: 0,
            run_id: "run-1".into(),
            library_id: 1,
            from_dir: "Album".into(),
            to_dir: "Album (2020)".into(),
            file_renames: vec![FileRename { from: "a.flac".into(), to: "b.flac".into() }],
            dir_sidecars: vec![],
            nfo_written: None,
            nfo_backup: None,
        };
        // "After apply" then already rolled back:
        fs::write(root.join("Album/a.flac"), b"x").unwrap();
        rollback_record(&root, &rec, None).unwrap(); // no-op, nothing to undo
        assert!(root.join("Album/a.flac").exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&storage);
    }
}
