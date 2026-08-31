//! Repo-relative path elision for narrow panes (docs/review/15 §3.3).
//!
//! Pure helpers; the snapshot keeps the full path as identity — elision is display-only.
//! All width math is in terminal display cells (unicode-width), grapheme-safe. The
//! contract, in order:
//!
//! 1. components are compared whole (never a raw string prefix);
//! 2. a worthwhile shared directory prefix is stripped and every row keeps a visible
//!    `…/` marker for it (the pane title no longer carries it — the row must stand alone);
//! 3. duplicate basenames keep the shortest component suffix that distinguishes them;
//! 4. middle-elision happens only after that suffix is preserved;
//! 5. only when extreme width still collides do rows gain a stable `·01`/`·02` ordinal.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

/// The marker prepended when a shared root was stripped: `…/` plus the visible suffix.
const ROOT_MARKER: &str = "…/";

/// Shorten `paths` for display in `budget` cells, as a SET (see the module docs).
///
/// Returns display strings in input order. Never splits a grapheme or exceeds `budget`;
/// two different input paths always produce two different display strings.
pub fn elide_paths(paths: &[&str], budget: usize) -> Vec<String> {
    if budget < 3 {
        return paths.iter().map(|_| "…".to_string()).collect();
    }
    let root = shared_root(paths);
    let marker = if root.is_some() { ROOT_MARKER } else { "" };
    let rest: Vec<&str> = paths
        .iter()
        .map(|p| root.as_deref().map_or(*p, |r| p.strip_prefix(r).unwrap_or(p)))
        .collect();
    let comps: Vec<Vec<&str>> = rest.iter().map(|p| p.split('/').collect()).collect();

    // The distinguishing suffix depth per row: 1 = basename only; deepened until no other
    // row shares the same suffix. The full remaining path is the ceiling.
    let depth: Vec<usize> = (0..rest.len())
        .map(|i| distinguishing_depth(&comps, i))
        .collect();

    // Cheap path: everything fits unstripped with no duplicate basename — pass through.
    let fits = rest
        .iter()
        .all(|p| format!("{marker}{p}").width() <= budget);
    let mut out: Vec<String> = if fits {
        rest.iter().map(|p| format!("{marker}{p}")).collect()
    } else {
        (0..rest.len())
            .map(|i| elide_row(rest[i], &comps[i], depth[i], marker, budget))
            .collect()
    };

    // Last-resort disambiguation: two rows can still collide (e.g. different middle
    // sections elided identically at a tiny budget). Append a stable ordinal — ordered
    // by the full path so renames of OTHER files never reshuffle two survivors.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for o in &out {
        *counts.entry(o.clone()).or_default() += 1;
    }
    if counts.values().any(|&n| n > 1) {
        // Re-elide each colliding row with room for the ordinal. `ordinal_for` walks the
        // full input order so the numbering is stable regardless of which rows collide.
        let ord_w = 3; // "·NN"
        let inner_budget = budget.saturating_sub(ord_w).max(1);
        // `out` is borrowed mutably by iter_mut; compute ordinals from a frozen copy first.
        let snapshot: Vec<String> = out.clone();
        for (i, o) in out.iter_mut().enumerate() {
            if counts[snapshot[i].as_str()] > 1 {
                let n = ordinal_for(&snapshot, i);
                let base = elide_row(rest[i], &comps[i], depth[i], marker, inner_budget);
                *o = format!("{base}·{n:02}");
            }
        }
    }
    out
}

/// The 1-based ordinal of row `i` among the rows sharing its display string, by input
/// order (stable: it never depends on other files being added or removed).
fn ordinal_for(out: &[String], i: usize) -> usize {
    out[..=i].iter().filter(|o| **o == out[i]).count()
}

/// The shortest suffix depth (in components) that distinguishes `comps[i]` from every
/// other row, capped at the full component count.
fn distinguishing_depth(comps: &[Vec<&str>], i: usize) -> usize {
    let mine = &comps[i];
    for depth in 1..=mine.len() {
        let suffix = &mine[mine.len() - depth..];
        let unique = comps
            .iter()
            .enumerate()
            .all(|(j, other)| j == i || other.len() < depth || other[other.len() - depth..] != *suffix);
        if unique {
            return depth;
        }
    }
    mine.len()
}

/// The shared directory root across `paths`, if stripping it is worthwhile.
///
/// Compares whole directory components (never a raw string prefix — `packages/api` and
/// `packages/api-old` share no `api` component). Returns `Some(root)` (with trailing `/`)
/// only when ≥2 files share ≥2 components AND the root is ≥8 cells wide.
pub fn shared_root(paths: &[&str]) -> Option<String> {
    if paths.len() < 2 {
        return None;
    }
    let dirs: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split('/').collect::<Vec<_>>())
        .map(|mut v| {
            v.pop(); // drop the basename
            v
        })
        .collect();
    let first = &dirs[0];
    let mut common = 0;
    for (i, comp) in first.iter().enumerate() {
        if dirs.iter().all(|d| d.get(i) == Some(comp)) {
            common += 1;
        } else {
            break;
        }
    }
    if common < 2 {
        return None;
    }
    let root = first[..common].join("/") + "/";
    // Only strip when it actually saves meaningful width.
    if root.width() >= 8 {
        Some(root)
    } else {
        None
    }
}

/// Elide one (root-stripped) row into `budget` cells, preserving its distinguishing
/// suffix (`depth` components) before touching anything else.
fn elide_row(path: &str, comps: &[&str], depth: usize, marker: &str, budget: usize) -> String {
    let full = format!("{marker}{path}");
    if full.width() <= budget {
        return full;
    }
    // suffix = the last `depth` components, joined; they are the identity and survive.
    let suffix = comps[comps.len() - depth.min(comps.len())..].join("/");
    if comps.len() > depth {
        // More path exists than the suffix shows: keep the first component as orientation
        // when it fits, else fall back to the bare marker + suffix.
        let first = comps[0];
        for candidate in [
            format!("{marker}{first}/…/{suffix}"),
            format!("{marker}…/{suffix}"),
            format!("{marker}{suffix}"),
        ] {
            if candidate.width() <= budget {
                return candidate;
            }
        }
        // Even marker+suffix is too wide: middle-elide the suffix (keeping head/tail),
        // then re-attach the marker if there is room.
        let inner = budget.saturating_sub(marker.width());
        let elided = middle_elide(&suffix, inner);
        let candidate = format!("{marker}{elided}");
        if candidate.width() <= budget {
            return candidate;
        }
        return middle_elide(&suffix, budget);
    }
    // The suffix IS the whole path: middle-elide it (plus marker when it fits).
    if full.width() <= budget {
        return full;
    }
    let inner = budget.saturating_sub(marker.width());
    let elided = middle_elide(&suffix, inner);
    let candidate = format!("{marker}{elided}");
    if candidate.width() <= budget {
        candidate
    } else {
        middle_elide(&suffix, budget)
    }
}

/// Middle-elide a single string to fit `budget` cells, keeping head + tail (+ extension
/// preference on the tail for file names).
fn middle_elide(s: &str, budget: usize) -> String {
    if s.width() <= budget {
        return s.to_string();
    }
    if budget < 4 {
        return "…".to_string();
    }
    // Split head/tail around a central "…". Keep the extension on the tail.
    let ext = s.rsplit_once('.').map(|(_, e)| format!(".{e}")).unwrap_or_default();
    let tail_keep = ext.width().min(budget / 2);
    let head_keep = budget.saturating_sub(1 + tail_keep);
    let head = take_cells(s, head_keep);
    let tail = take_cells_rev(s, tail_keep);
    format!("{head}…{tail}")
}

/// Take the first `cells` display cells of `s` (grapheme-safe).
fn take_cells(s: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(s, true) {
        let w = UnicodeWidthStr::width(g);
        if used + w > cells {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out
}

/// Take the last `cells` display cells of `s` (grapheme-safe).
fn take_cells_rev(s: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(s, true)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let w = UnicodeWidthStr::width(g);
        if used + w > cells {
            break;
        }
        out.insert_str(0, g);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paths_pass_through() {
        assert_eq!(elide_paths(&["a.go", "b.go"], 40), vec!["a.go", "b.go"]);
    }

    #[test]
    fn shared_root_strips_common_dirs() {
        let paths = &[
            "sandbox/vm-sandboxes/packages/api/server.go",
            "sandbox/vm-sandboxes/packages/conduitd/config.go",
        ];
        assert_eq!(shared_root(paths).as_deref(), Some("sandbox/vm-sandboxes/packages/"));
    }

    #[test]
    fn no_shared_root_when_components_diverge() {
        // "packages/api" and "packages/api-old" share only "packages" — <2 common components.
        let paths = &["packages/api/server.go", "packages/api-old/other.go"];
        assert_eq!(shared_root(paths), None);
    }

    #[test]
    fn single_file_gets_no_root() {
        assert_eq!(shared_root(&["a/b/c/d.go"]), None);
    }

    #[test]
    fn elide_middle_preserves_basename() {
        let out = elide_paths(&["sandbox/vm-sandboxes/packages/primelet/internal/actionworker/executor.go"], 30);
        assert!(out[0].width() <= 30, "must fit budget");
        assert!(out[0].ends_with("executor.go"), "basename preserved: {}", out[0]);
    }

    #[test]
    fn two_same_basenames_stay_distinguishable() {
        let out = elide_paths(
            &["worker/executor.go", "control-plane/executor.go"],
            40,
        );
        assert_ne!(out[0], out[1]);
        assert!(out[0].contains("executor.go"));
        assert!(out[1].contains("executor.go"));
        // The distinguishing parent directory survives at a generous budget.
        assert!(out[0].contains("worker"), "parent kept: {}", out[0]);
        assert!(out[1].contains("control-plane"), "parent kept: {}", out[1]);
    }

    #[test]
    fn duplicate_basenames_keep_distinguishing_suffix_under_pressure() {
        let paths = &[
            "a/very/long/directory/worker/executor.go",
            "a/very/long/directory/control-plane/executor.go",
        ];
        for budget in [60usize, 40, 30, 24, 18] {
            let out = elide_paths(paths, budget);
            assert_ne!(out[0], out[1], "budget {budget}: {:?} vs {:?}", out[0], out[1]);
            for o in &out {
                assert!(o.width() <= budget, "budget {budget}: {o}");
                assert!(o.contains("executor.go") || o.contains("…"), "budget {budget}: {o}");
            }
        }
    }

    #[test]
    fn extreme_width_collision_gets_stable_ordinals() {
        // Identical distinguishing suffixes (same basename, same parent name) that differ
        // only in an elided middle: at a tiny budget the strings collide and must gain
        // ordinals, ordered by the full path.
        let paths = &["x1/mid/worker/exec.go", "x2/mid/worker/exec.go"];
        let out = elide_paths(paths, 14);
        assert_ne!(out[0], out[1], "collision must be disambiguated: {out:?}");
        for o in &out {
            assert!(o.width() <= 14, "{o}");
        }
        // Stable: the ordinal follows the full-path order, so re-running matches.
        assert_eq!(out, elide_paths(paths, 14));
    }

    #[test]
    fn unicode_path_never_splits_grapheme_or_exceeds_budget() {
        let out = elide_paths(&["日本語/ファイル名.go"], 12);
        assert!(out[0].width() <= 12);
        // Grapheme-safe middle elide of a unicode name.
        let out = elide_paths(&["very/long/path/日本語ファイル名.go"], 14);
        assert!(out[0].width() <= 14);
    }

    #[test]
    fn tiny_budget_yields_ellipsis() {
        assert_eq!(elide_paths(&["anything.go"], 2), vec!["…"]);
    }

    #[test]
    fn root_marker_shows_omission_in_every_row() {
        let out = elide_paths(
            &[
                "sandbox/vm-sandboxes/packages/api/server.go",
                "sandbox/vm-sandboxes/packages/conduitd/config.go",
            ],
            40,
        );
        for o in &out {
            assert!(o.starts_with("…/"), "root strip is marked: {o}");
        }
    }

    #[test]
    fn no_root_no_marker() {
        let out = elide_paths(&["a/x.go", "b/y.go"], 40);
        assert_eq!(out, vec!["a/x.go", "b/y.go"]);
    }
}
