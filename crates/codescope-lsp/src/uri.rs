//! Path ⇄ `file://` URI conversion.
//!
//! `lsp-types` 0.97 models URIs as [`lsp_types::Uri`] (a `fluent-uri` newtype)
//! without file-path helpers, so the percent-encoding-correct conversions go
//! through the `url` crate. Paths are absolute [`camino::Utf8Path`]s.

use std::str::FromStr;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::LspError;

/// Build a `file://` URI for an absolute path.
pub fn uri_from_path(path: &Utf8Path) -> Result<lsp_types::Uri, LspError> {
    let url = url::Url::from_file_path(path.as_std_path())
        .map_err(|()| LspError::InvalidUri(format!("not an absolute path: {path}")))?;
    lsp_types::Uri::from_str(url.as_str())
        .map_err(|e| LspError::InvalidUri(format!("{url}: {e}")))
}

/// Resolve a `file://` URI back to an absolute UTF-8 path.
pub fn path_from_uri(uri: &lsp_types::Uri) -> Result<Utf8PathBuf, LspError> {
    let url = url::Url::parse(uri.as_str())
        .map_err(|e| LspError::InvalidUri(format!("{}: {e}", uri.as_str())))?;
    if url.scheme() != "file" {
        return Err(LspError::InvalidUri(format!(
            "not a file URI: {}",
            uri.as_str()
        )));
    }
    let path = url
        .to_file_path()
        .map_err(|()| LspError::InvalidUri(format!("no file path in {}", uri.as_str())))?;
    Utf8PathBuf::from_path_buf(path)
        .map_err(|p| LspError::InvalidUri(format!("non-UTF-8 path {}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_plain_path() {
        let path = Utf8Path::new("/tmp/project/main.go");
        let uri = uri_from_path(path).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/project/main.go");
        assert_eq!(path_from_uri(&uri).unwrap(), path);
    }

    #[test]
    fn round_trips_path_with_spaces_and_unicode() {
        let path = Utf8Path::new("/tmp/my project/sub dir/héllo 😀.go");
        let uri = uri_from_path(path).unwrap();
        assert!(uri.as_str().starts_with("file:///tmp/my%20project/"));
        assert_eq!(path_from_uri(&uri).unwrap(), path);
    }

    #[test]
    fn rejects_relative_path() {
        assert!(matches!(
            uri_from_path(Utf8Path::new("relative/main.go")),
            Err(LspError::InvalidUri(_))
        ));
    }

    #[test]
    fn rejects_non_file_uri() {
        let uri = lsp_types::Uri::from_str("https://example.com/x.go").unwrap();
        assert!(matches!(path_from_uri(&uri), Err(LspError::InvalidUri(_))));
    }
}
