//! Process-global, local-only JSONL telemetry for Codescope.
//!
//! The binary initializes one append-only sink beside the global configuration. Other
//! workspace crates can then record UI and provider events without depending on the binary.
//! Recording is deliberately best-effort after initialization: a local write failure never turns
//! a completed interaction or provider request into an application failure.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const SCHEMA_VERSION: u32 = 1;
static SINK: OnceLock<Mutex<TelemetrySink>> = OnceLock::new();
static SESSION_NONCE: AtomicU64 = AtomicU64::new(1);

struct TelemetrySink {
    file: File,
    path: PathBuf,
    session_id: String,
    started: Instant,
    sequence: u64,
    repository: Option<String>,
}

impl TelemetrySink {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let timestamp = unix_millis();
        let nonce = SESSION_NONCE.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            file,
            path: path.to_path_buf(),
            session_id: format!("{timestamp}-{}-{nonce}", std::process::id()),
            started: Instant::now(),
            sequence: 0,
            repository: None,
        })
    }

    fn write(&mut self, event: &str, data: Value) -> io::Result<()> {
        self.sequence = self.sequence.saturating_add(1);
        let record = json!({
            "schema_version": SCHEMA_VERSION,
            "timestamp_unix_ms": unix_millis(),
            "elapsed_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "session_id": self.session_id,
            "sequence": self.sequence,
            "repository": self.repository,
            "event": event,
            "data": data,
        });
        // Serialize first so a record reaches the append-only file in one write in the
        // overwhelmingly common case, minimizing cross-process interleaving.
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Initialize the process-global append-only telemetry file.
///
/// Calling this more than once is harmless when the sink is already installed. The first
/// successful path owns the process for the remainder of its lifetime.
pub fn init(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    if let Some(sink) = SINK.get() {
        return sink
            .lock()
            .map(|sink| sink.path.clone())
            .map_err(|_| io::Error::other("telemetry lock poisoned"));
    }
    let sink = TelemetrySink::open(path.as_ref())?;
    let actual = sink.path.clone();
    let _ = SINK.set(Mutex::new(sink));
    Ok(SINK
        .get()
        .and_then(|sink| sink.lock().ok().map(|sink| sink.path.clone()))
        .unwrap_or(actual))
}

/// Return the active telemetry path, if initialization succeeded.
#[must_use]
pub fn path() -> Option<PathBuf> {
    SINK.get()
        .and_then(|sink| sink.lock().ok().map(|sink| sink.path.clone()))
}

/// Attach the canonical repository root to subsequent events and emit a context event.
pub fn set_repository(root: impl Into<String>) {
    let root = root.into();
    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };
    if sink.repository.as_deref() == Some(root.as_str()) {
        return;
    }
    sink.repository = Some(root.clone());
    let _ = sink.write("session.repository", json!({ "root": root }));
}

/// Append one structured event to the active process telemetry file.
///
/// Calls made before successful initialization are no-ops. Post-initialization write errors
/// are intentionally isolated from the user operation being observed.
pub fn record(event: &str, data: Value) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if let Ok(mut sink) = sink.lock() {
        let _ = sink.write(event, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_appends_one_valid_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let mut sink = TelemetrySink::open(&path).unwrap();
        sink.repository = Some("/repo".into());
        sink.write("input.key", json!({"key": "j"})).unwrap();
        sink.write("llm.response", json!({"body": {"ok": true}}))
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let records = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["schema_version"], 1);
        assert_eq!(records[0]["sequence"], 1);
        assert_eq!(records[0]["repository"], "/repo");
        assert_eq!(records[0]["event"], "input.key");
        assert_eq!(records[1]["sequence"], 2);
        assert_eq!(records[1]["data"]["body"]["ok"], true);
    }

    #[cfg(unix)]
    #[test]
    fn sink_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let _sink = TelemetrySink::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
