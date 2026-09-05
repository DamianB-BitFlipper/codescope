//! Parser for `git status --porcelain=v2 --branch -z --untracked-files=all` output.
//!
//! Verified format (research 02):
//! - Headers: `# branch.oid <sha|(initial)>`, `# branch.head <name|(detached)>`,
//!   `# branch.upstream <name>` (only when set), `# branch.ab +A -B` (only when set).
//! - Ordinary: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`.
//! - Rename/copy: `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <R|C><score> <newPath>NUL<origPath>`
//!   (new path first, original second — the entry consumes **two** NUL-separated tokens).
//! - Unmerged: `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` (three stage modes/SHAs).
//! - Untracked: `? <path>`; ignored: `! <path>`.

use crate::error::{GitError, Result};
use camino::Utf8PathBuf;
use codescope_core::{HeadState, Oid};

/// Staged (`X`) / unstaged (`Y`) state pair from a porcelain v2 record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct XY {
    /// Index vs HEAD.
    pub staged: char,
    /// Worktree vs index.
    pub unstaged: char,
}

impl XY {
    fn parse(field: &str) -> Result<XY> {
        let mut chars = field.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some(x), Some(y), None) => Ok(XY {
                staged: x,
                unstaged: y,
            }),
            _ => Err(GitError::ParseStatus {
                detail: format!("bad XY field: {field:?}"),
            }),
        }
    }
}

/// One parsed status record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusEntry {
    /// `1` — ordinary changed entry.
    Ordinary {
        /// Staged/unstaged pair.
        xy: XY,
        /// `true` when the submodule field is `S...` (gitlink).
        submodule: bool,
        /// Repo-relative path.
        path: Utf8PathBuf,
    },
    /// `2` — rename or copy (index-only).
    RenamedOrCopied {
        /// Staged/unstaged pair.
        xy: XY,
        /// `true` for `C<score>`, `false` for `R<score>`.
        copy: bool,
        /// Similarity score 0–100.
        score: u8,
        /// New path.
        path: Utf8PathBuf,
        /// Original path.
        orig_path: Utf8PathBuf,
    },
    /// `u` — unmerged (conflicted) entry.
    Unmerged {
        /// Repo-relative path.
        path: Utf8PathBuf,
    },
    /// `?` — untracked file.
    Untracked {
        /// Repo-relative path.
        path: Utf8PathBuf,
    },
}

/// Parsed `--branch` headers plus all records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusSnapshot {
    /// `# branch.oid`; `None` when `(initial)` (unborn HEAD).
    pub oid: Option<Oid>,
    /// `# branch.head`; `None` when `(detached)`.
    pub branch: Option<String>,
    /// `# branch.upstream`, when set.
    pub upstream: Option<String>,
    /// `# branch.ab` as `(ahead, behind)`, when reported.
    pub ahead_behind: Option<(u32, u32)>,
    /// All file records in git output order.
    pub entries: Vec<StatusEntry>,
}

impl StatusSnapshot {
    /// [`HeadState`] following the porcelain v2 header semantics: unborn wins over the
    /// branch name (git still prints the branch name for an unborn HEAD).
    pub(crate) fn head_state(&self) -> HeadState {
        match (&self.oid, &self.branch) {
            (None, _) => HeadState::Unborn,
            (Some(_), Some(name)) => HeadState::Branch(name.clone()),
            (Some(oid), None) => HeadState::Detached(oid.clone()),
        }
    }

    /// Paths of `?` records.
    pub(crate) fn untracked_paths(&self) -> impl Iterator<Item = &Utf8PathBuf> {
        self.entries.iter().filter_map(|e| match e {
            StatusEntry::Untracked { path } => Some(path),
            _ => None,
        })
    }

    /// Paths of `u` records.
    pub(crate) fn unmerged_paths(&self) -> impl Iterator<Item = &Utf8PathBuf> {
        self.entries.iter().filter_map(|e| match e {
            StatusEntry::Unmerged { path } => Some(path),
            _ => None,
        })
    }
}

fn utf8_token<'a>(token: &'a [u8], what: &str) -> Result<&'a str> {
    std::str::from_utf8(token).map_err(|_| GitError::NonUtf8 {
        context: format!("status record ({what})"),
    })
}

fn parse_u32(field: &str, what: &str) -> Result<u32> {
    field.parse().map_err(|_| GitError::ParseStatus {
        detail: format!("bad {what}: {field:?}"),
    })
}

/// Parse NUL-separated porcelain v2 output (with `--branch` headers).
pub(crate) fn parse_status_z(bytes: &[u8]) -> Result<StatusSnapshot> {
    let mut snapshot = StatusSnapshot::default();
    let mut tokens = bytes.split(|b| *b == 0).filter(|token| !token.is_empty());

    while let Some(token) = tokens.next() {
        let record = utf8_token(token, "record")?;
        match record.as_bytes().first() {
            Some(b'#') => parse_header(record, &mut snapshot)?,
            Some(b'1') => snapshot.entries.push(parse_ordinary(record)?),
            Some(b'2') => {
                let orig = tokens.next().ok_or_else(|| GitError::ParseStatus {
                    detail: format!("rename record without original path: {record:?}"),
                })?;
                let orig = utf8_token(orig, "rename original path")?;
                snapshot.entries.push(parse_rename(record, orig)?);
            }
            Some(b'u') => snapshot.entries.push(parse_unmerged(record)?),
            Some(b'?') => snapshot.entries.push(StatusEntry::Untracked {
                path: strip_tag(record)?.into(),
            }),
            Some(b'!') => {} // ignored entries (only with --ignored; skip defensively)
            _ => {
                return Err(GitError::ParseStatus {
                    detail: format!("unknown record: {record:?}"),
                });
            }
        }
    }
    Ok(snapshot)
}

/// Strip the `<tag> ` prefix of a two-field record (`? <path>`, `! <path>`).
fn strip_tag(record: &str) -> Result<&str> {
    record
        .get(2..)
        .filter(|rest| !rest.is_empty())
        .ok_or_else(|| GitError::ParseStatus {
            detail: format!("record too short: {record:?}"),
        })
}

fn parse_header(record: &str, snapshot: &mut StatusSnapshot) -> Result<()> {
    let Some(rest) = record.strip_prefix("# ") else {
        return Err(GitError::ParseStatus {
            detail: format!("bad header: {record:?}"),
        });
    };
    if let Some(oid) = rest.strip_prefix("branch.oid ") {
        snapshot.oid = (oid != "(initial)").then(|| Oid::new(oid));
    } else if let Some(head) = rest.strip_prefix("branch.head ") {
        snapshot.branch = (head != "(detached)").then(|| head.to_string());
    } else if let Some(upstream) = rest.strip_prefix("branch.upstream ") {
        snapshot.upstream = Some(upstream.to_string());
    } else if let Some(ab) = rest.strip_prefix("branch.ab ") {
        let (a, b) = ab.split_once(' ').ok_or_else(|| GitError::ParseStatus {
            detail: format!("bad branch.ab: {ab:?}"),
        })?;
        let ahead = parse_u32(a.trim_start_matches('+'), "branch.ab ahead")?;
        let behind = parse_u32(b.trim_start_matches('-'), "branch.ab behind")?;
        snapshot.ahead_behind = Some((ahead, behind));
    }
    // Unknown headers (e.g. future `# stash <N>`) are skipped.
    Ok(())
}

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — 8 fixed fields, then the path.
fn parse_ordinary(record: &str) -> Result<StatusEntry> {
    let mut fields = record.splitn(9, ' ');
    let mut next = |what: &str| {
        fields.next().ok_or_else(|| GitError::ParseStatus {
            detail: format!("ordinary record missing {what}: {record:?}"),
        })
    };
    let _tag = next("tag")?;
    let xy = XY::parse(next("XY")?)?;
    let submodule = next("sub")?.starts_with('S');
    for f in ["mH", "mI", "mW", "hH", "hI"] {
        next(f)?;
    }
    let path = next("path")?;
    Ok(StatusEntry::Ordinary {
        xy,
        submodule,
        path: path.into(),
    })
}

/// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <newPath>` + separate `<origPath>` token.
fn parse_rename(record: &str, orig_path: &str) -> Result<StatusEntry> {
    let mut fields = record.splitn(10, ' ');
    let mut next = |what: &str| {
        fields.next().ok_or_else(|| GitError::ParseStatus {
            detail: format!("rename record missing {what}: {record:?}"),
        })
    };
    let _tag = next("tag")?;
    let xy = XY::parse(next("XY")?)?;
    for f in ["sub", "mH", "mI", "mW", "hH", "hI"] {
        next(f)?;
    }
    let xscore = next("Xscore")?;
    let copy = match xscore.as_bytes().first() {
        Some(b'R') => false,
        Some(b'C') => true,
        _ => {
            return Err(GitError::ParseStatus {
                detail: format!("bad rename/copy score field {xscore:?} in {record:?}"),
            });
        }
    };
    let score: u8 = xscore[1..].parse().map_err(|_| GitError::ParseStatus {
        detail: format!("bad similarity score in {xscore:?}"),
    })?;
    let path = next("newPath")?;
    Ok(StatusEntry::RenamedOrCopied {
        xy,
        copy,
        score,
        path: path.into(),
        orig_path: orig_path.into(),
    })
}

/// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` — 10 fixed fields, then the path.
fn parse_unmerged(record: &str) -> Result<StatusEntry> {
    let mut fields = record.splitn(11, ' ');
    let path = fields.nth(10).ok_or_else(|| GitError::ParseStatus {
        detail: format!("unmerged record too short: {record:?}"),
    })?;
    Ok(StatusEntry::Unmerged { path: path.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in records {
            out.extend_from_slice(r.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn headers_branch_with_upstream() {
        let bytes = z(&[
            "# branch.oid 1111111111111111111111111111111111111111",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]);
        let s = parse_status_z(&bytes).unwrap();
        assert_eq!(s.head_state(), HeadState::Branch("main".into()));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.ahead_behind, Some((2, 1)));
        assert!(s.entries.is_empty());
    }

    #[test]
    fn headers_unborn_and_detached() {
        let s = parse_status_z(&z(&["# branch.oid (initial)", "# branch.head main"])).unwrap();
        assert_eq!(s.head_state(), HeadState::Unborn);

        let s = parse_status_z(&z(&["# branch.oid abcd123", "# branch.head (detached)"])).unwrap();
        assert_eq!(s.head_state(), HeadState::Detached(Oid::new("abcd123")));
    }

    #[test]
    fn ordinary_entry_with_spaces_in_path() {
        let s = parse_status_z(&z(&[
            "1 .M N... 100644 100644 100644 1234567 1234567 dir/my file.go",
        ]))
        .unwrap();
        match &s.entries[0] {
            StatusEntry::Ordinary {
                xy,
                submodule,
                path,
            } => {
                assert_eq!((xy.staged, xy.unstaged), ('.', 'M'));
                assert!(!submodule);
                assert_eq!(path, "dir/my file.go");
            }
            other => panic!("wrong entry: {other:?}"),
        }
    }

    #[test]
    fn submodule_flag() {
        let s = parse_status_z(&z(&[
            "1 M. SC.. 160000 160000 160000 1234567 7654321 vendor/sub",
        ]))
        .unwrap();
        assert!(matches!(
            &s.entries[0],
            StatusEntry::Ordinary {
                submodule: true,
                ..
            }
        ));
    }

    #[test]
    fn rename_entry_consumes_two_tokens() {
        let s = parse_status_z(&z(&[
            "2 R. N... 100644 100644 100644 1234567 1234567 R100 new name.go",
            "old name.go",
            "1 .M N... 100644 100644 100644 1234567 1234567 other.go",
        ]))
        .unwrap();
        assert_eq!(s.entries.len(), 2);
        match &s.entries[0] {
            StatusEntry::RenamedOrCopied {
                xy,
                copy,
                score,
                path,
                orig_path,
            } => {
                assert_eq!(xy.staged, 'R');
                assert!(!copy);
                assert_eq!(*score, 100);
                assert_eq!(path, "new name.go");
                assert_eq!(orig_path, "old name.go");
            }
            other => panic!("wrong entry: {other:?}"),
        }
    }

    #[test]
    fn copy_entry() {
        let s = parse_status_z(&z(&[
            "2 C. N... 100644 100644 100644 1234567 1234567 C87 copy.go",
            "orig.go",
        ]))
        .unwrap();
        assert!(matches!(
            &s.entries[0],
            StatusEntry::RenamedOrCopied {
                copy: true,
                score: 87,
                ..
            }
        ));
    }

    #[test]
    fn unmerged_entry_has_three_stage_columns() {
        let s = parse_status_z(&z(&[
            "u UU N... 100644 100644 100644 100644 111 222 333 conflicted.go",
        ]))
        .unwrap();
        assert_eq!(
            s.unmerged_paths().collect::<Vec<_>>(),
            vec!["conflicted.go"]
        );
    }

    #[test]
    fn untracked_and_ignored() {
        let s = parse_status_z(&z(&["? new.go", "! target/junk"])).unwrap();
        assert_eq!(s.untracked_paths().collect::<Vec<_>>(), vec!["new.go"]);
        assert_eq!(s.entries.len(), 1); // ignored dropped
    }

    #[test]
    fn truncated_rename_errors() {
        let err = parse_status_z(&z(&[
            "2 R. N... 100644 100644 100644 1234567 1234567 R100 new.go",
        ]))
        .unwrap_err();
        assert!(matches!(err, GitError::ParseStatus { .. }), "{err}");
    }

    #[test]
    fn unknown_record_errors() {
        let err = parse_status_z(&z(&["z bogus"])).unwrap_err();
        assert!(matches!(err, GitError::ParseStatus { .. }));
    }

    #[test]
    fn empty_output_is_clean() {
        let s = parse_status_z(b"").unwrap();
        assert!(s.entries.is_empty());
        assert_eq!(s.head_state(), HeadState::Unborn); // no headers at all
    }
}
