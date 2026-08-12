// Run report generation. Persistence (file + KVStore) lives in `store.rs`.
//
// The report is written for a novice user: plain words ("move", "keep",
// "skip"), clear "from/to" lines, and a summary. No cryptic abbreviations.

use crate::organizer::AlbumPlan;

pub fn build_report(
    mode: &str,
    root_label: &str,
    plans: &[AlbumPlan],
    run_id: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("===== nd-organizer run report =====\n");
    out.push_str(&format!("Mode: {mode}\n"));
    out.push_str(&format!("Library: {root_label}\n"));
    out.push_str(&format!("Albums checked: {}\n", plans.len()));
    if let Some(run) = run_id {
        out.push_str(&format!(
            "Run ID: {run}   (copy this into the 'Rollback run ID' setting to undo this run)\n"
        ));
    }
    out.push('\n');

    let mut albums_to_move = 0usize;
    let mut moves = 0usize;
    let mut skipped = 0usize;
    let mut keeps = 0usize;

    for (i, p) in plans.iter().enumerate() {
        albums_to_move += usize::from(!p.moves.is_empty());
        moves += p.moves.len();
        skipped += p.skipped.len();
        keeps += p.keeps;

        let classification = match p.bucket {
            crate::organizer::Bucket::Soundtrack => "Soundtrack",
            crate::organizer::Bucket::Various => "Various artists (compilation)",
            crate::organizer::Bucket::Singles => "Single / incomplete",
            crate::organizer::Bucket::Normal => "Normal album",
        };

        out.push_str(&format!(
            "--- Album {}/{}  ({classification}) ---\n",
            i + 1,
            plans.len()
        ));
        out.push_str(&format!(
            "  Current folder: {}\n",
            display_path(&p.current_dir)
        ));
        if p.current_dir.eq_ignore_ascii_case(&p.target_dir) {
            out.push_str("  Target folder:  (unchanged)\n");
        } else {
            out.push_str(&format!(
                "  Target folder:  {}\n",
                display_path(&p.target_dir)
            ));
        }

        if !p.moves.is_empty() {
            out.push_str(&format!("  Files to move ({}):\n", p.moves.len()));
            for m in &p.moves {
                out.push_str(&format!("    - {}\n", file_name(&m.to)));
                out.push_str(&format!("      from: {}\n", display_path(&m.from)));
                out.push_str(&format!("      to:   {}\n", display_path(&m.to)));
                let mut extras: Vec<String> = p.dir_sidecars.clone();
                extras.extend(m.sidecars.clone());
                if !extras.is_empty() {
                    out.push_str(&format!("      also moves: {}\n", extras.join(", ")));
                }
            }
        } else {
            out.push_str("  No files to move.\n");
        }

        if !p.skipped.is_empty() {
            out.push_str(&format!("  Could not move ({}):\n", p.skipped.len()));
            for (path, reason) in &p.skipped {
                out.push_str(&format!("    - {}  --  {reason}\n", display_path(path)));
            }
        }

        out.push('\n');
    }

    let mut summary = String::new();
    summary.push_str("===== Summary =====\n");
    summary.push_str(&format!(
        "Albums that need changes: {albums_to_move} of {}\n",
        plans.len()
    ));
    summary.push_str(&format!("Files to move: {moves}\n"));
    summary.push_str(&format!("Files unchanged: {keeps}\n"));
    summary.push_str(&format!("Files skipped: {skipped}\n"));
    if mode == "dryRun" {
        summary.push_str("This was a dry run: nothing was written or renamed.\n");
    } else {
        summary.push_str("Changes above were applied.\n");
        if let Some(run) = run_id {
            summary.push_str(&format!(
                "To undo this run, set the 'Rollback run ID' setting to: {run}\n"
            ));
        }
    }

    out.push_str(&summary);
    out
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn display_path(rel: &str) -> String {
    if rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::organizer::{AlbumPlan, Bucket, FileMove};

    #[test]
    fn report_uses_plain_language() {
        let plan = AlbumPlan {
            bucket: Bucket::Normal,
            current_dir: "Artist A/Album One".into(),
            target_dir: "Artist A/Album One (2020)".into(),
            moves: vec![FileMove {
                from: "Artist A/Album One/01 - Song.flac".into(),
                to: "Artist A/Album One (2020)/01 - Song.flac".into(),
                sidecars: vec!["01 - Song.lrc".into()],
            }],
            dir_sidecars: vec!["album.nfo".into()],
            keeps: 2,
            skipped: vec![(
                "Artist A/Album One/02 - Dup.flac".into(),
                "target already exists".into(),
            )],
        };
        let report = build_report("dryRun", "Music", &[plan], Some("run-123"));
        assert!(report.contains("Mode: dryRun"), "header shows mode");
        assert!(report.contains("Run ID: run-123"), "run id shown");
        assert!(
            report.contains("copy this into the 'Rollback run ID' setting"),
            "rollback hint shown"
        );
        assert!(report.contains("Current folder"), "clear folder label");
        assert!(report.contains("Target folder"), "clear target label");
        assert!(report.contains("Files to move"), "clear move wording");
        assert!(report.contains("from:"), "from line");
        assert!(report.contains("to:"), "to line");
        assert!(report.contains("also moves"), "sidecar wording");
        assert!(report.contains("Could not move"), "skip wording");
        assert!(
            report.contains("Albums that need changes"),
            "summary wording"
        );
        assert!(
            report.contains("nothing was written or renamed"),
            "dry-run notice"
        );
        assert!(!report.contains("mv "), "no cryptic mv");
        assert!(!report.contains("->"), "no cryptic arrow");
    }
}
