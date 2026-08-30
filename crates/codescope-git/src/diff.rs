//! Parser for `git diff` unified patch output into [`FileChange`]s.
//!
//! Verified grammar (research 02 + recovered experiments):
//! - Sections start at `diff --git a/<old> b/<new>`; unmerged paths emit `diff --cc <path>`
//!   (combined format, skipped — marked [`FileStatus::Unmerged`], no hunks) and
//!   `git diff --cached` emits `* Unmerged path <path>` lines instead.
//! - Extended headers: `old mode`/`new mode`, `new file mode`, `deleted file mode`,
//!   `similarity index NN%`, `rename from/to`, `copy from/to`, `index <a>..<b>[ <mode>]`,
//!   `Binary files ... differ`, then `--- a/<old>` / `+++ b/<new>` (or `/dev/null`).
//! - Hunk header `@@ -os[,ol] +ns[,nl] @@ [section]`: `,1` omitted, len 0 on the empty side.
//! - Body line first byte: ` `=context (counts on both sides), `-`=old only, `+`=new only,
//!   `\`=metadata (`\ No newline at end of file` — dropped, counts nothing). A hunk ends
//!   exactly when both counts are met; unmet counts at EOF are a hard error.
//! - Paths with special characters are C-quoted (`core.quotePath`); quoted paths are
//!   unescaped here. Mode `160000` marks a gitlink (submodule) — never hunk-parsed.

use crate::error::{GitError, Result};
use camino::Utf8PathBuf;
use codescope_core::{DiffLine, FileChange, FileStatus, Hunk};

/// Parse a full `git diff` patch into per-file changes, in git output order.
pub(crate) fn parse_unified_diff(text: &str) -> Result<Vec<FileChange>> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut parser = DiffParser { lines, pos: 0 };
    parser.parse_all()
}

struct DiffParser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

impl<'a> DiffParser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<&'a str> {
        let line = self.peek()?;
        self.pos += 1;
        Some(line)
    }

    fn parse_all(&mut self) -> Result<Vec<FileChange>> {
        let mut files = Vec::new();
        while let Some(line) = self.peek() {
            if line.starts_with("diff --git ") {
                files.push(self.parse_git_section()?);
            } else if let Some(rest) = line
                .strip_prefix("diff --cc ")
                .or_else(|| line.strip_prefix("diff --combined "))
            {
                files.push(self.parse_cc_section(rest)?);
            } else if let Some(path) = line.strip_prefix("* Unmerged path ") {
                self.pos += 1;
                files.push(unmerged_change(unquote_path(path)?));
            } else if line.is_empty() || line.starts_with("Submodule ") {
                // Stray blank line / "Submodule <p> contains modified content" notices.
                self.pos += 1;
            } else {
                return Err(GitError::ParseDiff {
                    detail: format!("unexpected top-level line: {line:?}"),
                });
            }
        }
        Ok(files)
    }

    /// `diff --cc <path>`: combined diff for an unmerged path. Skip the whole section.
    fn parse_cc_section(&mut self, path_part: &'a str) -> Result<FileChange> {
        let path = unquote_path(path_part)?;
        self.pos += 1;
        while let Some(line) = self.peek() {
            if line.starts_with("diff --") {
                break;
            }
            self.pos += 1;
        }
        Ok(unmerged_change(path))
    }

    fn parse_git_section(&mut self) -> Result<FileChange> {
        let git_line = self.bump().unwrap_or_default();
        let mut facts = SectionFacts {
            git_line,
            ..SectionFacts::default()
        };

        // Extended header lines until the first hunk / binary marker / next section.
        while let Some(line) = self.peek() {
            if line.starts_with("diff --") {
                break;
            }
            if line.starts_with("@@ ") {
                self.parse_hunks(&mut facts)?;
                break;
            }
            self.pos += 1;
            self.parse_header_line(line, &mut facts)?;
            if facts.binary {
                break;
            }
        }
        facts.finish()
    }

    fn parse_header_line(&mut self, line: &'a str, facts: &mut SectionFacts<'a>) -> Result<()> {
        if let Some(mode) = line.strip_prefix("old mode ") {
            facts.old_mode = Some(mode.trim().to_string());
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            facts.new_mode = Some(mode.trim().to_string());
        } else if let Some(mode) = line.strip_prefix("new file mode ") {
            facts.added = true;
            facts.new_mode = Some(mode.trim().to_string());
        } else if let Some(mode) = line.strip_prefix("deleted file mode ") {
            facts.deleted = true;
            facts.old_mode = Some(mode.trim().to_string());
        } else if let Some(score) = line.strip_prefix("similarity index ") {
            facts.similarity = Some(parse_score(score)?);
        } else if line.starts_with("dissimilarity index ") {
            // Emitted for complete rewrites with -B; no structural meaning here.
        } else if let Some(path) = line.strip_prefix("rename from ") {
            facts.rename_from = Some(unquote_path(path)?);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            facts.rename_to = Some(unquote_path(path)?);
        } else if let Some(path) = line.strip_prefix("copy from ") {
            facts.rename_from = Some(unquote_path(path)?);
            facts.copy = true;
        } else if let Some(path) = line.strip_prefix("copy to ") {
            facts.rename_to = Some(unquote_path(path)?);
            facts.copy = true;
        } else if let Some(rest) = line.strip_prefix("index ") {
            // `index <old>..<new>[ <mode>]` — the mode appears when unchanged on both sides.
            if let Some((_, mode)) = rest.rsplit_once(' ') {
                facts.index_mode = Some(mode.trim().to_string());
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            facts.old_file = strip_prefixed_path(path, "a/")?;
        } else if let Some(path) = line.strip_prefix("+++ ") {
            facts.new_file = strip_prefixed_path(path, "b/")?;
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            facts.binary = true;
        } else if line == "GIT binary patch" {
            // Defensive: we never pass --binary, but skip the payload if it appears.
            facts.binary = true;
            while let Some(next) = self.peek() {
                if next.starts_with("diff --") {
                    break;
                }
                self.pos += 1;
            }
        } else {
            return Err(GitError::ParseDiff {
                detail: format!("unexpected extended header line: {line:?}"),
            });
        }
        Ok(())
    }

    fn parse_hunks(&mut self, facts: &mut SectionFacts<'a>) -> Result<()> {
        while let Some(line) = self.peek() {
            if !line.starts_with("@@ ") {
                break;
            }
            self.pos += 1;
            facts.hunks.push(self.parse_hunk_body(line)?);
        }
        Ok(())
    }

    /// Parse one hunk: header line already consumed and passed in.
    fn parse_hunk_body(&mut self, header: &str) -> Result<Hunk> {
        let (old_start, old_len, new_start, new_len, section) = parse_hunk_header(header)?;
        let mut hunk = Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            section,
            lines: Vec::new(),
        };
        let (mut old_seen, mut new_seen) = (0u32, 0u32);
        let (mut old_ln, mut new_ln) = (old_start, new_start);

        while old_seen < old_len || new_seen < new_len {
            let line = self.bump().ok_or_else(|| GitError::ParseDiff {
                detail: format!(
                    "truncated hunk {header:?}: got {old_seen}/{old_len} old, {new_seen}/{new_len} new lines at EOF"
                ),
            })?;
            match line.as_bytes().first() {
                Some(b' ') => {
                    hunk.lines
                        .push(DiffLine::context(old_ln, new_ln, &line[1..]));
                    old_ln += 1;
                    new_ln += 1;
                    old_seen += 1;
                    new_seen += 1;
                }
                Some(b'-') => {
                    hunk.lines.push(DiffLine::del(old_ln, &line[1..]));
                    old_ln += 1;
                    old_seen += 1;
                }
                Some(b'+') => {
                    hunk.lines.push(DiffLine::add(new_ln, &line[1..]));
                    new_ln += 1;
                    new_seen += 1;
                }
                Some(b'\\') => {} // "\ No newline at end of file" — metadata, counts nothing
                _ => {
                    return Err(GitError::ParseDiff {
                        detail: format!("unexpected line inside hunk {header:?}: {line:?}"),
                    })
                }
            }
        }
        // A trailing "\ No newline..." can follow the final counted line.
        while self.peek().is_some_and(|l| l.starts_with('\\')) {
            self.pos += 1;
        }
        Ok(hunk)
    }
}

/// Facts collected from one `diff --git` section.
#[derive(Default)]
struct SectionFacts<'a> {
    git_line: &'a str,
    old_mode: Option<String>,
    new_mode: Option<String>,
    index_mode: Option<String>,
    similarity: Option<u8>,
    rename_from: Option<Utf8PathBuf>,
    rename_to: Option<Utf8PathBuf>,
    copy: bool,
    added: bool,
    deleted: bool,
    binary: bool,
    old_file: Option<Utf8PathBuf>,
    new_file: Option<Utf8PathBuf>,
    hunks: Vec<Hunk>,
}

impl SectionFacts<'_> {
    fn is_gitlink(&self) -> bool {
        [&self.old_mode, &self.new_mode, &self.index_mode]
            .into_iter()
            .flatten()
            .any(|m| m == "160000")
    }

    fn finish(self) -> Result<FileChange> {
        let gitlink = self.is_gitlink();
        let (path, old_path, status) =
            if let (Some(from), Some(to)) = (self.rename_from, self.rename_to) {
                let score = self.similarity.unwrap_or(100);
                let status = if self.copy {
                    FileStatus::Copied { score }
                } else {
                    FileStatus::Renamed { score }
                };
                (to, Some(from), status)
            } else {
                let path = match (&self.new_file, &self.old_file) {
                    (Some(new), _) => new.clone(),
                    (None, Some(old)) => old.clone(),
                    (None, None) => parse_git_line_path(self.git_line)?,
                };
                let status = if self.added {
                    FileStatus::Added
                } else if self.deleted {
                    FileStatus::Deleted
                } else if type_changed(self.old_mode.as_deref(), self.new_mode.as_deref()) {
                    FileStatus::TypeChanged
                } else {
                    FileStatus::Modified
                };
                (path, None, status)
            };

        let status = if gitlink { FileStatus::Gitlink } else { status };
        let hunks = if gitlink || self.binary {
            Vec::new()
        } else {
            self.hunks
        };
        Ok(FileChange {
            path,
            old_path,
            status,
            hunks,
            binary: self.binary,
        })
    }
}

pub(crate) fn unmerged_change(path: Utf8PathBuf) -> FileChange {
    FileChange {
        path,
        old_path: None,
        status: FileStatus::Unmerged,
        hunks: Vec::new(),
        binary: false,
    }
}

fn type_changed(old_mode: Option<&str>, new_mode: Option<&str>) -> bool {
    match (old_mode, new_mode) {
        (Some(old), Some(new)) => (old == "120000") != (new == "120000"),
        _ => false,
    }
}

fn parse_score(field: &str) -> Result<u8> {
    field
        .trim()
        .trim_end_matches('%')
        .parse()
        .map_err(|_| GitError::ParseDiff {
            detail: format!("bad similarity score: {field:?}"),
        })
}

/// `@@ -os[,ol] +ns[,nl] @@ [section]` — `,1` omitted; len 0 on the empty side.
fn parse_hunk_header(line: &str) -> Result<(u32, u32, u32, u32, Option<String>)> {
    let err = || GitError::ParseDiff {
        detail: format!("bad hunk header: {line:?}"),
    };
    let rest = line.strip_prefix("@@ -").ok_or_else(err)?;
    let (old_part, rest) = rest.split_once(" +").ok_or_else(err)?;
    let (new_part, rest) = rest.split_once(" @@").ok_or_else(err)?;
    let (old_start, old_len) = parse_start_len(old_part).ok_or_else(err)?;
    let (new_start, new_len) = parse_start_len(new_part).ok_or_else(err)?;
    let section = rest
        .strip_prefix(' ')
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    Ok((old_start, old_len, new_start, new_len, section))
}

fn parse_start_len(part: &str) -> Option<(u32, u32)> {
    match part.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((part.parse().ok()?, 1)),
    }
}

/// `--- a/<path>` / `+++ b/<path>` payload → repo-relative path; `None` for `/dev/null`.
fn strip_prefixed_path(payload: &str, prefix: &str) -> Result<Option<Utf8PathBuf>> {
    if payload == "/dev/null" {
        return Ok(None);
    }
    let path = unquote_path(payload)?;
    let stripped = path
        .as_str()
        .strip_prefix(prefix)
        .ok_or_else(|| GitError::ParseDiff {
            detail: format!("expected {prefix:?} prefix on path: {payload:?}"),
        })?;
    Ok(Some(Utf8PathBuf::from(stripped)))
}

/// Best-effort path recovery from `diff --git a/<p> b/<p>` for sections without
/// `---`/`+++` lines (mode-only changes, some binary cases). Renames always carry
/// explicit `rename from/to` lines and never reach this.
fn parse_git_line_path(git_line: &str) -> Result<Utf8PathBuf> {
    let err = || GitError::ParseDiff {
        detail: format!("cannot extract path from: {git_line:?}"),
    };
    let rest = git_line.strip_prefix("diff --git ").ok_or_else(err)?;

    if rest.starts_with('"') {
        // Quoted form: `diff --git "a/we ird" "b/we ird"` — take the first quoted token.
        let token = quoted_token(rest).ok_or_else(err)?;
        let path = unquote_path(token)?;
        return Ok(strip_ab_prefix(&path).unwrap_or(path));
    }

    // Unquoted: try every ` b/` split point and prefer one where both halves agree.
    let mut fallback = None;
    for (idx, _) in rest.match_indices(" b/") {
        let (a_part, b_part) = (&rest[..idx], &rest[idx + 1..]);
        if let (Some(a), Some(b)) = (a_part.strip_prefix("a/"), b_part.strip_prefix("b/")) {
            if a == b {
                return Ok(Utf8PathBuf::from(b));
            }
            fallback = Some(Utf8PathBuf::from(b));
        }
    }
    fallback.ok_or_else(err)
}

/// The leading C-quoted token of `input` (including both quotes), honoring escapes.
fn quoted_token(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut idx = 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\\' => idx += 2, // skip the escaped byte
            b'"' => return input.get(..=idx),
            _ => idx += 1,
        }
    }
    None
}

fn strip_ab_prefix(path: &Utf8PathBuf) -> Option<Utf8PathBuf> {
    path.as_str()
        .strip_prefix("a/")
        .or_else(|| path.as_str().strip_prefix("b/"))
        .map(Utf8PathBuf::from)
}

/// Undo git's C-style quoting (`core.quotePath`): `"path\twith\303\251scapes"`.
/// Unquoted input is returned as-is.
fn unquote_path(raw: &str) -> Result<Utf8PathBuf> {
    let raw = raw.trim_end_matches('\t'); // defensive: some formats append a tab
    if !raw.starts_with('"') {
        return Ok(Utf8PathBuf::from(raw));
    }
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| GitError::ParseDiff {
            detail: format!("unterminated quoted path: {raw:?}"),
        })?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut input = inner.as_bytes().iter().copied().peekable();
    while let Some(b) = input.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        let esc = input.next().ok_or_else(|| GitError::ParseDiff {
            detail: format!("dangling escape in quoted path: {raw:?}"),
        })?;
        match esc {
            b'"' | b'\\' => bytes.push(esc),
            b'a' => bytes.push(0x07),
            b'b' => bytes.push(0x08),
            b'f' => bytes.push(0x0c),
            b'n' => bytes.push(b'\n'),
            b'r' => bytes.push(b'\r'),
            b't' => bytes.push(b'\t'),
            b'v' => bytes.push(0x0b),
            b'0'..=b'7' => {
                let mut value = u32::from(esc - b'0');
                for _ in 0..2 {
                    match input.peek() {
                        Some(d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(*d - b'0');
                            input.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(value).map_err(|_| GitError::ParseDiff {
                    detail: format!("octal escape out of range in: {raw:?}"),
                })?);
            }
            other => {
                return Err(GitError::ParseDiff {
                    detail: format!("unknown escape \\{} in quoted path: {raw:?}", other as char),
                })
            }
        }
    }
    let text = String::from_utf8(bytes).map_err(|_| GitError::NonUtf8 {
        context: format!("quoted path {raw:?}"),
    })?;
    Ok(Utf8PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codescope_core::DiffLineKind;

    #[test]
    fn modified_file_basic_hunk() {
        let text = "\
diff --git a/pkg/a.go b/pkg/a.go
index 1234567..89abcde 100644
--- a/pkg/a.go
+++ b/pkg/a.go
@@ -1,3 +1,4 @@ func main() {
 package main
-old()
+new()
+newer()
 tail
";
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "pkg/a.go");
        assert_eq!(f.status, FileStatus::Modified);
        assert!(!f.binary);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 3, 1, 4)
        );
        assert_eq!(h.section.as_deref(), Some("func main() {"));
        assert_eq!(h.lines.len(), 5);
        assert_eq!(h.lines[0].kind, DiffLineKind::Context);
        assert_eq!(h.lines[0].old_ln, Some(1));
        assert_eq!(h.lines[0].new_ln, Some(1));
        assert_eq!(h.lines[1].kind, DiffLineKind::Del);
        assert_eq!(h.lines[1].old_ln, Some(2));
        assert_eq!(h.lines[1].text, "old()");
        assert_eq!(h.lines[2].kind, DiffLineKind::Add);
        assert_eq!(h.lines[2].new_ln, Some(2));
        assert_eq!(h.lines[3].new_ln, Some(3));
    }

    #[test]
    fn comma_one_omitted_in_header() {
        let text = "\
diff --git a/one.txt b/one.txt
index 1234567..89abcde 100644
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-old
+new
";
        let f = &parse_unified_diff(text).unwrap()[0];
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 1, 1, 1)
        );
        assert_eq!(h.section, None);
    }

    #[test]
    fn pure_add_new_file_zero_old_len() {
        let text = "\
diff --git a/new.go b/new.go
new file mode 100644
index 0000000..89abcde
--- /dev/null
+++ b/new.go
@@ -0,0 +1,2 @@
+package x
+func F() {}
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.path, "new.go");
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (0, 0, 1, 2)
        );
        assert!(h.is_pure_addition());
    }

    #[test]
    fn pure_delete_zero_new_len() {
        let text = "\
diff --git a/gone.go b/gone.go
deleted file mode 100644
index 89abcde..0000000
--- a/gone.go
+++ /dev/null
@@ -1,2 +0,0 @@
-package x
-func F() {}
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Deleted);
        assert_eq!(f.path, "gone.go");
        let h = &f.hunks[0];
        assert!(h.is_pure_deletion());
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 2, 0, 0)
        );
    }

    #[test]
    fn no_newline_marker_dropped_mid_and_end() {
        let text = "\
diff --git a/x.txt b/x.txt
index 1234567..89abcde 100644
--- a/x.txt
+++ b/x.txt
@@ -1 +1 @@
-old line
\\ No newline at end of file
+new line
\\ No newline at end of file
";
        let f = &parse_unified_diff(text).unwrap()[0];
        let h = &f.hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.count_added(), 1);
        assert_eq!(h.count_deleted(), 1);
    }

    #[test]
    fn truncated_hunk_errors() {
        let text = "\
diff --git a/x.txt b/x.txt
index 1234567..89abcde 100644
--- a/x.txt
+++ b/x.txt
@@ -1,2 +1,2 @@
 ctx
-old
";
        let err = parse_unified_diff(text).unwrap_err();
        assert!(matches!(err, GitError::ParseDiff { .. }), "{err}");
        assert!(err.to_string().contains("truncated"));
    }

    #[test]
    fn rename_with_edit() {
        let text = "\
diff --git a/old.go b/new.go
similarity index 90%
rename from old.go
rename to new.go
index 1234567..89abcde 100644
--- a/old.go
+++ b/new.go
@@ -1 +1 @@
-package a
+package b
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Renamed { score: 90 });
        assert_eq!(f.path, "new.go");
        assert_eq!(
            f.old_path.as_deref().map(camino::Utf8Path::as_str),
            Some("old.go")
        );
        assert_eq!(f.hunks.len(), 1);
    }

    #[test]
    fn pure_rename_no_hunks() {
        let text = "\
diff --git a/old.go b/sub/new.go
similarity index 100%
rename from old.go
rename to sub/new.go
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Renamed { score: 100 });
        assert_eq!(f.path, "sub/new.go");
        assert_eq!(
            f.old_path.as_deref().map(camino::Utf8Path::as_str),
            Some("old.go")
        );
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn copy_detected() {
        let text = "\
diff --git a/orig.go b/copy.go
similarity index 95%
copy from orig.go
copy to copy.go
index 1234567..89abcde 100644
--- a/orig.go
+++ b/copy.go
@@ -1 +1 @@
-x
+y
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Copied { score: 95 });
        assert_eq!(
            f.old_path.as_deref().map(camino::Utf8Path::as_str),
            Some("orig.go")
        );
    }

    #[test]
    fn binary_file_marked_no_hunks() {
        let text = "\
diff --git a/img.png b/img.png
new file mode 100644
index 0000000..89abcde
Binary files /dev/null and b/img.png differ
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.path, "img.png");
        assert_eq!(f.status, FileStatus::Added);
        assert!(f.binary);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn submodule_gitlink_no_hunks() {
        let text = "\
diff --git a/vendor/sub b/vendor/sub
index 1234567..89abcde 160000
--- a/vendor/sub
+++ b/vendor/sub
@@ -1 +1 @@
-Subproject commit 1234567890123456789012345678901234567890
+Subproject commit 0987654321098765432109876543210987654321
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Gitlink);
        assert!(f.hunks.is_empty(), "gitlink hunks must be dropped");
    }

    #[test]
    fn new_gitlink_via_new_file_mode() {
        let text = "\
diff --git a/vendor/sub b/vendor/sub
new file mode 160000
index 0000000..89abcde
--- /dev/null
+++ b/vendor/sub
@@ -0,0 +1 @@
+Subproject commit 1234567890123456789012345678901234567890
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::Gitlink);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn combined_cc_section_skipped_as_unmerged() {
        let text = "\
diff --cc conflicted.go
index 1111111,2222222..0000000
--- a/conflicted.go
+++ b/conflicted.go
@@@ -1,3 -1,3 +1,7 @@@
  package x
++<<<<<<< HEAD
 +ours()
++=======
+ theirs()
++>>>>>>> other
diff --git a/normal.go b/normal.go
index 1234567..89abcde 100644
--- a/normal.go
+++ b/normal.go
@@ -1 +1 @@
-a
+b
";
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, FileStatus::Unmerged);
        assert_eq!(files[0].path, "conflicted.go");
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[1].status, FileStatus::Modified);
    }

    #[test]
    fn unmerged_path_line_from_cached_diff() {
        let text = "* Unmerged path conflicted.go\n";
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files[0].status, FileStatus::Unmerged);
        assert_eq!(files[0].path, "conflicted.go");
    }

    #[test]
    fn mode_only_change_path_from_git_line() {
        let text = "\
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.path, "script.sh");
        assert_eq!(f.status, FileStatus::Modified);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn mode_only_change_path_with_spaces() {
        let text = "\
diff --git a/my dir/my file.sh b/my dir/my file.sh
old mode 100644
new mode 100755
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.path, "my dir/my file.sh");
    }

    #[test]
    fn type_change_symlink() {
        let text = "\
diff --git a/link b/link
old mode 100644
new mode 120000
index 1234567..89abcde
--- a/link
+++ b/link
@@ -1,2 +1 @@
-real content
-more
+target/path
\\ No newline at end of file
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.status, FileStatus::TypeChanged);
    }

    #[test]
    fn quoted_paths_unescaped() {
        let text = "\
diff --git \"a/sp\\303\\244ter.go\" \"b/sp\\303\\244ter.go\"
index 1234567..89abcde 100644
--- \"a/sp\\303\\244ter.go\"
+++ \"b/sp\\303\\244ter.go\"
@@ -1 +1 @@
-x
+y
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.path, "später.go");
    }

    #[test]
    fn multiple_hunks_one_file() {
        let text = "\
diff --git a/big.go b/big.go
index 1234567..89abcde 100644
--- a/big.go
+++ b/big.go
@@ -1,2 +1,2 @@
 a
-b
+B
@@ -10,2 +10,3 @@ func ten() {
 x
+X2
 y
";
        let f = &parse_unified_diff(text).unwrap()[0];
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[1].section.as_deref(), Some("func ten() {"));
        assert_eq!(f.hunks[1].lines[1].new_ln, Some(11));
    }

    #[test]
    fn empty_diff_is_empty() {
        assert!(parse_unified_diff("").unwrap().is_empty());
        assert!(parse_unified_diff("\n").unwrap().is_empty());
    }

    #[test]
    fn submodule_dirty_notice_skipped() {
        let text = "Submodule vendor/sub contains modified content\n";
        assert!(parse_unified_diff(text).unwrap().is_empty());
    }

    #[test]
    fn unexpected_top_level_line_errors() {
        let err = parse_unified_diff("garbage here\n").unwrap_err();
        assert!(matches!(err, GitError::ParseDiff { .. }));
    }

    #[test]
    fn unquote_octal_and_specials() {
        assert_eq!(unquote_path("plain.go").unwrap(), "plain.go");
        assert_eq!(unquote_path("\"tab\\there\"").unwrap(), "tab\there");
        assert_eq!(unquote_path("\"q\\\"uote\"").unwrap(), "q\"uote");
        assert_eq!(unquote_path("\"sp\\303\\244t\"").unwrap(), "spät");
        assert!(unquote_path("\"unterminated").is_err());
    }

    #[test]
    fn context_line_numbering_across_del_add() {
        let text = "\
diff --git a/n.txt b/n.txt
index 1234567..89abcde 100644
--- a/n.txt
+++ b/n.txt
@@ -5,4 +5,4 @@
 five
-six
+SIX
 seven
 eight
";
        let f = &parse_unified_diff(text).unwrap()[0];
        let lines = &f.hunks[0].lines;
        assert_eq!(lines[0].old_ln, Some(5));
        assert_eq!(lines[1].old_ln, Some(6));
        assert_eq!(lines[1].new_ln, None);
        assert_eq!(lines[2].new_ln, Some(6));
        assert_eq!(lines[2].old_ln, None);
        assert_eq!(lines[3].old_ln, Some(7));
        assert_eq!(lines[3].new_ln, Some(7));
        assert_eq!(lines[4].old_ln, Some(8));
        assert_eq!(lines[4].new_ln, Some(8));
    }
}
