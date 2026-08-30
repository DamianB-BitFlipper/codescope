//! File identity: repo-relative UTF-8 paths.

use crate::error::CoreError;
use std::fmt;

// Re-exported so consumers can name path types without taking their own camino dependency.
pub use camino::{Utf8Path, Utf8PathBuf};

/// A repo-relative UTF-8 path identifying one file in the repository.
///
/// Used by the semantic, diagnostic, impact-graph and plan domains. Git-domain types keep
/// plain [`Utf8PathBuf`] because they mirror `git` output verbatim; convert with
/// [`FileId::new`] (checked, rejects absolute paths) at the git→analysis boundary.
///
/// Equality and ordering are by path string.
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct FileId(Utf8PathBuf);

impl FileId {
    /// Create a file id, rejecting absolute paths.
    ///
    /// Note: `Deserialize` does **not** perform this check — plan/AI JSON is validated by
    /// the dedicated validation boundary (research 05 §3), not by serde.
    pub fn new(path: impl Into<Utf8PathBuf>) -> Result<Self, CoreError> {
        let path = path.into();
        if path.is_absolute() {
            return Err(CoreError::AbsolutePath(path.into_string()));
        }
        Ok(FileId(path))
    }

    /// Create a file id without checking for absoluteness (producer is trusted).
    #[must_use]
    pub fn new_unchecked(path: impl Into<Utf8PathBuf>) -> Self {
        FileId(path.into())
    }

    /// Borrow the path.
    #[must_use]
    pub fn as_path(&self) -> &Utf8Path {
        &self.0
    }

    /// Consume into the inner path.
    #[must_use]
    pub fn into_path_buf(self) -> Utf8PathBuf {
        self.0
    }

    /// Final path component, if any.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.0.file_name()
    }

    /// Parent directory path, if any.
    #[must_use]
    pub fn parent(&self) -> Option<&Utf8Path> {
        self.0.parent()
    }

    /// File extension, if any.
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        self.0.extension()
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl AsRef<Utf8Path> for FileId {
    fn as_ref(&self) -> &Utf8Path {
        &self.0
    }
}

impl From<FileId> for Utf8PathBuf {
    fn from(id: FileId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_paths() {
        assert!(FileId::new("/abs/path.go").is_err());
        assert!(FileId::new("rel/path.go").is_ok());
    }

    #[test]
    fn accessors() {
        let id = FileId::new("pkg/sub/thing.go").unwrap();
        assert_eq!(id.file_name(), Some("thing.go"));
        assert_eq!(id.extension(), Some("go"));
        assert_eq!(id.parent().unwrap(), Utf8Path::new("pkg/sub"));
        assert_eq!(id.to_string(), "pkg/sub/thing.go");
    }

    #[test]
    fn serializes_as_plain_string() {
        let id = FileId::new("a/b.go").unwrap();
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            serde_json::json!("a/b.go")
        );
        let back: FileId = serde_json::from_value(serde_json::json!("a/b.go")).unwrap();
        assert_eq!(back, id);
    }
}
