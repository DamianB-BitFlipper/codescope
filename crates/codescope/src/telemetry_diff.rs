//! Privacy-filtered, content-addressed telemetry snapshots of the dispatcher-owned comparison.
//!
//! This module consumes the exact unified sections retained while building `ChangeSet`; it never
//! invokes Git. Paths are filtered before payload construction, text then passes through the same
//! repository-root redaction and recognizable-secret scrubber used for provider traffic.

use std::collections::HashMap;

use camino::Utf8Path;
use codescope_core::{ChangeScope, ChangeSet, FileChange, HeadState, RepoContext};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{json, Value};

/// Store and activate the current parsed comparison, returning its content identity. `Ok(None)`
/// means telemetry was not initialized; malformed/missing captured comparison data is an error so
/// callers can explicitly clear correlation rather than accidentally retaining an older ID.
pub(crate) fn activate(
    context: &RepoContext,
    changeset: &ChangeSet,
    epoch: codescope_core::Epoch,
) -> Result<Option<String>, String> {
    let payload = build_payload(context, changeset, codescope_telemetry::repository_id())?;
    Ok(codescope_telemetry::activate_diff_snapshot(
        epoch.get(),
        payload,
    ))
}

fn build_payload(
    context: &RepoContext,
    changeset: &ChangeSet,
    repository_id: Option<String>,
) -> Result<Value, String> {
    let sections = changeset
        .diff_sections
        .as_ref()
        .ok_or_else(|| "parsed comparison did not retain unified diff sections".to_string())?;
    let exclusions = DiffExclusions::load(&context.toplevel)?;
    let mut by_path = HashMap::new();
    for section in sections {
        if by_path
            .insert(section.path.as_str(), section.text.as_str())
            .is_some()
        {
            return Err(format!(
                "parsed comparison contains duplicate diff sections for {}",
                section.path
            ));
        }
    }

    let mut canonical_diff = String::new();
    let mut files = Vec::new();
    for file in &changeset.files {
        if exclusions.excludes(&file.path)
            || file
                .old_path
                .as_ref()
                .is_some_and(|path| exclusions.excludes(path))
        {
            continue;
        }

        let raw_section = by_path.get(file.path.as_str()).copied();
        if raw_section.is_none()
            && !matches!(
                file.status,
                codescope_core::FileStatus::Untracked | codescope_core::FileStatus::Unmerged
            )
        {
            return Err(format!(
                "parsed comparison is missing the diff section for {}",
                file.path
            ));
        }
        let section = raw_section.map(|text| {
            codescope_ai::scrub_secrets(&codescope_ai::redact_repo_root(text, &context.toplevel))
        });
        let file_start = section.as_ref().map(|_| canonical_diff.len());
        if let Some(section) = &section {
            canonical_diff.push_str(section);
        }
        let file_end = section.as_ref().map(|_| canonical_diff.len());
        let hunk_ranges = section.as_deref().map(hunk_byte_ranges).unwrap_or_default();
        files.push(file_metadata(file, file_start.zip(file_end), &hunk_ranges));
    }

    let payload = scrub_value(json!({
        "repository": {
            "repository_id": repository_id,
        },
        "comparison": comparison_metadata(context, changeset),
        "canonical_diff": canonical_diff,
        "files": files,
    }));
    let serialized = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    if serialized.contains(context.toplevel.as_str()) {
        return Err("absolute repository path survived telemetry redaction".to_string());
    }
    Ok(payload)
}

fn comparison_metadata(context: &RepoContext, changeset: &ChangeSet) -> Value {
    let head_commit = context.head_oid.as_ref().map(ToString::to_string);
    let committed_base = head_commit.as_ref().map_or_else(
        || {
            json!({
                "kind": "empty_tree",
                "oid": "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            })
        },
        |oid| json!({ "kind": "head_commit", "oid": oid }),
    );
    let resolved_base = match changeset.scope {
        ChangeScope::Branch => context.base.as_ref().map(|base| {
            json!({
                "kind": "merge_base",
                "ref": base.ref_name,
                "oid": base.merge_base,
                "source": base.source,
            })
        }),
        ChangeScope::Staged | ChangeScope::Working => Some(committed_base),
        ChangeScope::Unstaged => Some(json!({ "kind": "index" })),
    };
    let resolved_head = match changeset.scope {
        ChangeScope::Branch => head_commit.as_ref().map_or_else(
            || json!({ "kind": "unborn" }),
            |oid| json!({ "kind": "head_commit", "oid": oid }),
        ),
        ChangeScope::Staged => json!({ "kind": "index" }),
        ChangeScope::Unstaged | ChangeScope::Working => json!({ "kind": "worktree" }),
    };
    let head = match &context.head {
        HeadState::Branch(name) => json!({ "state": "branch", "name": name }),
        HeadState::Detached(oid) => json!({ "state": "detached", "oid": oid }),
        HeadState::Unborn => json!({ "state": "unborn" }),
    };
    json!({
        "scope": changeset.scope,
        "fallback": changeset.fallback,
        "head": head,
        "head_oid": context.head_oid,
        "resolved_base": resolved_base,
        "resolved_head": resolved_head,
    })
}

fn file_metadata(
    file: &FileChange,
    diff_range: Option<(usize, usize)>,
    hunk_ranges: &[(usize, usize)],
) -> Value {
    let hunks = file
        .hunks
        .iter()
        .enumerate()
        .map(|(index, hunk)| {
            let range = hunk_ranges.get(index).copied().map(|(start, end)| {
                let base = diff_range.map_or(0, |(file_start, _)| file_start);
                json!({ "start": base + start, "end": base + end })
            });
            json!({
                "hunk_index": index,
                "old_start": hunk.old_start,
                "old_len": hunk.old_len,
                "new_start": hunk.new_start,
                "new_len": hunk.new_len,
                "section": hunk.section,
                "diff_byte_range": range,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "path": file.path,
        "old_path": file.old_path,
        "status": file.status,
        "binary": file.binary,
        "diff_byte_range": diff_range.map(|(start, end)| json!({ "start": start, "end": end })),
        "hunks": hunks,
    })
}

/// Byte ranges of each `@@` hunk inside one complete unified section. The offsets target the
/// post-scrub text that is actually stored, including Unicode's variable-width UTF-8 encoding.
fn hunk_byte_ranges(section: &str) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in section.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts.get(index + 1).copied().unwrap_or(section.len()),
            )
        })
        .collect()
}

fn scrub_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(codescope_ai::scrub_secrets(&text)),
        Value::Array(values) => Value::Array(values.into_iter().map(scrub_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, scrub_value(value)))
                .collect(),
        ),
        value => value,
    }
}

struct DiffExclusions {
    matcher: Gitignore,
}

impl DiffExclusions {
    fn load(root: &Utf8Path) -> Result<Self, String> {
        let mut builder = GitignoreBuilder::new(root);
        for name in [".gitignore", ".codescopeignore"] {
            let path = root.join(name);
            if path.exists() {
                if let Some(error) = builder.add(path) {
                    return Err(format!("cannot parse {name}: {error}"));
                }
            }
        }
        Ok(Self {
            matcher: builder.build().map_err(|error| error.to_string())?,
        })
    }

    fn excludes(&self, path: &Utf8Path) -> bool {
        built_in_secret_path(path)
            || self
                .matcher
                .matched_path_or_any_parents(path.as_std_path(), false)
                .is_ignore()
    }
}

fn built_in_secret_path(path: &Utf8Path) -> bool {
    let path = path.as_str().replace('\\', "/").to_ascii_lowercase();
    let components = path.split('/').collect::<Vec<_>>();
    let name = components.last().copied().unwrap_or_default();
    let has_private_directory = components
        .iter()
        .any(|component| matches!(*component, ".secrets" | ".ssh" | ".gnupg"));
    let sensitive_config = path == ".docker/config.json"
        || path.ends_with("/.docker/config.json")
        || path == ".kube/config"
        || path.ends_with("/.kube/config");
    name == ".codescopeignore"
        || name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".env")
        || name == ".netrc"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == ".terraformrc"
        || name.starts_with("credentials")
        || name.ends_with("credentials.json")
        || name.starts_with("secrets.")
        || name.starts_with("kubeconfig")
        || name.starts_with("id_rsa")
        || name.starts_with("id_dsa")
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_ed25519")
        || [
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".ppk",
            ".asc",
            ".tfvars",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || has_private_directory
        || sensitive_config
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use codescope_core::{BaseInfo, BaseSource, FileStatus, Hunk, Oid, UnifiedDiffSection};

    fn hunk(old_start: u32, old_len: u32, new_start: u32, new_len: u32) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            section: None,
            lines: Vec::new(),
        }
    }

    fn file(
        path: &str,
        old_path: Option<&str>,
        status: FileStatus,
        hunks: Vec<Hunk>,
    ) -> FileChange {
        FileChange {
            path: Utf8PathBuf::from(path),
            old_path: old_path.map(Utf8PathBuf::from),
            status,
            hunks,
            binary: false,
        }
    }

    fn section(path: &str, text: &str) -> UnifiedDiffSection {
        UnifiedDiffSection {
            path: Utf8PathBuf::from(path),
            text: text.to_string(),
        }
    }

    #[test]
    fn hunk_ranges_are_byte_exact_with_unicode_and_no_newline_markers() {
        let section = "diff --git a/a b/a\n@@ -1 +1 @@\n-café\n+雪\n\\ No newline at end of file\n@@ -4 +4 @@\n-x\n+y\n";
        let ranges = hunk_byte_ranges(section);
        assert_eq!(ranges.len(), 2);
        assert_eq!(
            &section[ranges[0].0..ranges[0].1],
            "@@ -1 +1 @@\n-café\n+雪\n\\ No newline at end of file\n"
        );
        assert_eq!(&section[ranges[1].0..ranges[1].1], "@@ -4 +4 @@\n-x\n+y\n");
    }

    #[test]
    fn built_in_secret_paths_are_denied_without_hiding_normal_source() {
        for path in [
            ".env",
            "development.env",
            "keys/prod.pem",
            "keys/prod.keystore",
            "secrets/id_ed25519",
            ".ssh/config",
            ".gnupg/private-keys-v1.d/key",
            ".docker/config.json",
            ".kube/config",
            "ops/prod.tfvars",
            "service_credentials.json",
            ".codescopeignore",
        ] {
            assert!(built_in_secret_path(Utf8Path::new(path)), "{path}");
        }
        assert!(!built_in_secret_path(Utf8Path::new("src/tokenizer.rs")));
    }

    #[test]
    fn snapshot_is_complete_mappable_and_privacy_filtered() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join(".codescopeignore"), "secrets/\n").unwrap();

        let renamed = concat!(
            "diff --git a/old.txt b/src/renamed.txt\n",
            "similarity index 80%\n",
            "rename from old.txt\n",
            "rename to src/renamed.txt\n",
            "index 1111111..2222222 100644\n",
            "--- a/old.txt\n",
            "+++ b/src/renamed.txt\n",
            "@@ -1 +1 @@\n",
            "-café\n",
            "+雪\n",
            "\\ No newline at end of file\n",
            "@@ -4 +4 @@\n",
            "-ordinary\n",
            "+OPENAI_API_KEY=sk-abcdef0123456789 Authorization: Bearer abcdef0123456789xyz\n",
        );
        let deleted = concat!(
            "diff --git a/gone.rs b/gone.rs\n",
            "deleted file mode 100644\n",
            "index 3333333..0000000\n",
            "--- a/gone.rs\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-goodbye\n",
        );
        let excluded = concat!(
            "diff --git a/secrets/private.txt b/secrets/private.txt\n",
            "--- a/secrets/private.txt\n",
            "+++ b/secrets/private.txt\n",
            "@@ -1 +1 @@\n",
            "-hidden-old\n",
            "+hidden-new\n",
        );
        let gitignored = concat!(
            "diff --git a/ignored/generated.txt b/ignored/generated.txt\n",
            "--- a/ignored/generated.txt\n",
            "+++ b/ignored/generated.txt\n",
            "@@ -1 +1 @@\n",
            "-ignored-old\n",
            "+ignored-new\n",
        );
        let env_file = concat!(
            "diff --git a/.env b/.env\n",
            "--- a/.env\n",
            "+++ b/.env\n",
            "@@ -1 +1 @@\n",
            "-PASSWORD=abcdefgh\n",
            "+PASSWORD=ijklmnop\n",
        );
        let changeset = ChangeSet::new(
            ChangeScope::Branch,
            vec![
                file(
                    "src/renamed.txt",
                    Some("old.txt"),
                    FileStatus::Renamed { score: 80 },
                    vec![hunk(1, 1, 1, 1), hunk(4, 1, 4, 1)],
                ),
                file("gone.rs", None, FileStatus::Deleted, vec![hunk(1, 1, 0, 0)]),
                file(
                    "secrets/private.txt",
                    None,
                    FileStatus::Modified,
                    vec![hunk(1, 1, 1, 1)],
                ),
                file(
                    "ignored/generated.txt",
                    None,
                    FileStatus::Modified,
                    vec![hunk(1, 1, 1, 1)],
                ),
                file(".env", None, FileStatus::Modified, vec![hunk(1, 1, 1, 1)]),
            ],
        )
        .with_diff_sections(vec![
            section("src/renamed.txt", renamed),
            section("gone.rs", deleted),
            section("secrets/private.txt", excluded),
            section("ignored/generated.txt", gitignored),
            section(".env", env_file),
        ]);
        let context = RepoContext {
            toplevel: root.clone(),
            head: HeadState::Branch("feature/unicode".into()),
            head_oid: Some(Oid::new("2222222222222222222222222222222222222222")),
            upstream: None,
            base: Some(BaseInfo {
                source: BaseSource::Override,
                ref_name: "main".into(),
                merge_base: Oid::new("1111111111111111111111111111111111111111"),
            }),
        };

        let payload = build_payload(&context, &changeset, Some("sha256:repo".into())).unwrap();
        let round_trip: Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(
            round_trip, payload,
            "multiline Unicode survives JSONL encoding"
        );
        let diff = payload["canonical_diff"].as_str().unwrap();
        assert!(diff.contains("-café\n+雪\n\\ No newline at end of file\n"));
        assert!(diff.contains(deleted));
        assert!(diff.contains(codescope_ai::REDACTED));
        assert!(!diff.contains("sk-abcdef0123456789"));
        assert!(!diff.contains("abcdef0123456789xyz"));
        for hidden in [
            "secrets/private.txt",
            "hidden-new",
            "ignored/generated.txt",
            "ignored-new",
            "PASSWORD=ijklmnop",
            ".env",
        ] {
            assert!(
                !serde_json::to_string(&payload).unwrap().contains(hidden),
                "leaked {hidden}"
            );
        }
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains(root.as_str()));

        let files = payload["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let rename = files
            .iter()
            .find(|file| file["path"] == "src/renamed.txt")
            .unwrap();
        assert_eq!(rename["old_path"], "old.txt");
        assert_eq!(rename["hunks"].as_array().unwrap().len(), 2);
        for hunk in rename["hunks"].as_array().unwrap() {
            let range = &hunk["diff_byte_range"];
            let start = range["start"].as_u64().unwrap() as usize;
            let end = range["end"].as_u64().unwrap() as usize;
            assert!(diff[start..end].starts_with("@@ "));
        }
        let deletion = files.iter().find(|file| file["path"] == "gone.rs").unwrap();
        assert_eq!(deletion["status"], "deleted");
        assert_eq!(deletion["hunks"][0]["hunk_index"], 0);
        let range = &deletion["diff_byte_range"];
        let start = range["start"].as_u64().unwrap() as usize;
        let end = range["end"].as_u64().unwrap() as usize;
        assert_eq!(&diff[start..end], deleted);
        assert_eq!(
            payload["comparison"]["resolved_base"]["oid"],
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            payload["comparison"]["resolved_head"]["oid"],
            "2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn missing_parsed_sections_is_unavailable_instead_of_reusing_old_data() {
        let temp = tempfile::tempdir().unwrap();
        let context = RepoContext {
            toplevel: Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
            head: HeadState::Unborn,
            head_oid: None,
            upstream: None,
            base: None,
        };
        let changeset = ChangeSet::new(ChangeScope::Working, Vec::new());
        assert!(build_payload(&context, &changeset, Some("sha256:repo".into())).is_err());
    }

    #[test]
    fn empty_captured_comparison_is_valid_but_uncaptured_data_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let context = RepoContext {
            toplevel: Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap(),
            head: HeadState::Branch("clean".into()),
            head_oid: Some(Oid::new("1111111111111111111111111111111111111111")),
            upstream: None,
            base: None,
        };
        let changeset =
            ChangeSet::new(ChangeScope::Working, Vec::new()).with_diff_sections(Vec::new());
        let payload = build_payload(&context, &changeset, Some("sha256:repo".into())).unwrap();
        assert_eq!(payload["canonical_diff"], "");
        assert_eq!(payload["files"], json!([]));
    }
}
