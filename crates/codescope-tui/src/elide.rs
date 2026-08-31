//! Repo-relative path elision for narrow panes (docs/review/13 §"Paths").
//!
//! Pure helpers; the snapshot keeps the full path as identity — elision is display-only.
//! All width math is in terminal display cells (unicode-width), grapheme-safe.

use unicode_width::UnicodeWidthStr;

/// Shorten `paths` for display in `budget` cells.
///
/// 1. Strip a shared directory root (only when ≥2 files share ≥2 directory components and it
///    saves ≥8 cells); the root goes in the pane title, not here.
/// 2. Per path: keep the basename/unique suffix; middle-elide with a `…/` marker.
///
/// Returns display strings in input order. Never splits a grapheme or exceeds `budget`.
pub fn elide_paths(paths: &[&str], budget: usize) -> Vec<String> {
    if budget < 4 {
        return paths.iter().map(|_| "…".to_string()).collect();
    }
    let root = shared_root(paths);
    let rest: Vec<&str> = paths
        .iter()
        .map(|p| root.as_deref().map_or(*p, |r| p.strip_prefix(r).unwrap_or(p)))
        .collect();
    rest.iter().map(|p| elide_one(p, budget, root.is_some())).collect()
}

/// The shared directory root across `paths`, if stripping it is worthwhile.
///
/// Compares whole directory components (never a raw string prefix — `packages/api` and
/// `packages/api-old` share no `api` component). Returns `Some(root)` (with trailing `/`) only
/// when ≥2 files share ≥2 components AND stripping saves ≥8 cells.
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

/// Elide a single (root-stripped) path to fit `budget` cells.
/// `had_root` prepends the `…/` omission marker when a shared root was stripped.
fn elide_one(path: &str, budget: usize, had_root: bool) -> String {
    let prefix = if had_root { "…/" } else { "" };
    let full = format!("{prefix}{path}");
    if full.width() <= budget {
        return full;
    }
    let comps: Vec<&str> = path.split('/').collect();
    let basename = comps.last().copied().unwrap_or(path);
    // Preserve the first divergent component + basename, middle-elide the rest.
    if comps.len() >= 2 {
        let first = comps[0];
        let candidate = format!("{prefix}{first}/…/{basename}");
        if candidate.width() <= budget {
            return candidate;
        }
        let candidate = format!("{prefix}…/{basename}");
        if candidate.width() <= budget {
            return candidate;
        }
    }
    // Last resort: middle-elide the basename itself, keeping the extension.
    let base_with_prefix = format!("{prefix}{basename}");
    if base_with_prefix.width() <= budget {
        return base_with_prefix;
    }
    middle_elide(basename, budget.saturating_sub(prefix.width()))
}

/// Middle-elide a single component to fit `budget` cells, keeping head + tail + extension.
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
    for ch in s.chars() {
        let w = ch.to_string().width();
        if used + w > cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Take the last `cells` display cells of `s`.
fn take_cells_rev(s: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let w = ch.to_string().width();
        if used + w > cells {
            break;
        }
        out.insert(0, ch);
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
        // Only "packages" is shared (1 component) → no root.
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
    }

    #[test]
    fn unicode_path_never_splits_grapheme_or_exceeds_budget() {
        let out = elide_paths(&["日本語/ファイル名.go"], 12);
        assert!(out[0].width() <= 12);
    }

    #[test]
    fn tiny_budget_yields_ellipsis() {
        assert_eq!(elide_paths(&["anything.go"], 3), vec!["…"]);
    }

    #[test]
    fn root_marker_shows_omission() {
        let out = elide_paths(
            &[
                "sandbox/vm-sandboxes/packages/api/server.go",
                "sandbox/vm-sandboxes/packages/conduitd/config.go",
            ],
            24,
        );
        for o in &out {
            assert!(o.starts_with("…/"), "root strip is marked: {o}");
        }
    }
}
