// Run report generation. Persistence (file + KVStore) lives in `store.rs`.

use crate::organizer::AlbumPlan;

pub fn build_report(mode: &str, root_label: &str, plans: &[AlbumPlan]) -> String {
    let mut out = String::new();
    out.push_str("===== nd-organizer run report =====\n");
    out.push_str(&format!("mode:     {mode}\n"));
    out.push_str(&format!("library:  {root_label}\n"));
    out.push_str(&format!("albums:   {}\n\n", plans.len()));

    let mut moved = 0usize;
    let mut moves = 0usize;
    let mut skipped = 0usize;
    let mut keeps = 0usize;

    for (i, p) in plans.iter().enumerate() {
        let marker = if p.moves.is_empty() { "ok " } else { "-> " };
        moved += usize::from(!p.moves.is_empty());
        moves += p.moves.len();
        skipped += p.skipped.len();
        keeps += p.keeps;
        out.push_str(&format!(
            "[{:02}/{:02}] {}  [{:?}]  {} -> {}\n",
            i + 1,
            plans.len(),
            marker,
            p.bucket,
            display_path(&p.current_dir),
            display_path(&p.target_dir),
        ));
        for m in &p.moves {
            out.push_str(&format!("        mv  {} -> {}\n", m.from, m.to));
            for sc in &m.sidecars {
                out.push_str(&format!("            sidecar: {sc}\n"));
            }
        }
        for sc in &p.dir_sidecars {
            out.push_str(&format!("        folder-sidecar: {sc}\n"));
        }
        for (path, reason) in &p.skipped {
            out.push_str(&format!("        !!  {path} (skipped: {reason})\n"));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "===== summary: {moved}/{plans} albums to move, {moves} file moves, {keeps} kept, {skipped} skipped\n",
        plans = plans.len()
    ));
    out
}

fn display_path(rel: &str) -> String {
    if rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel}")
    }
}

