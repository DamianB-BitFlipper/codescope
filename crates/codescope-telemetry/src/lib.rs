//! Process-global, local-only JSONL telemetry for Codescope.
//!
//! The binary initializes one append-only session file in a private directory beside the global
//! configuration. Other workspace crates can then record UI and provider events without depending
//! on the binary.
//! Recording is deliberately best-effort after initialization: a local write failure never turns
//! a completed interaction or provider request into an application failure.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
static SINK: OnceLock<Mutex<TelemetrySink>> = OnceLock::new();
static SESSION_NONCE: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    /// Request-scoped correlation overrides the process-global active comparison. This prevents a
    /// response from an older in-flight LLM task from being attributed to a newer diff snapshot.
    static DIFF_SNAPSHOT_CONTEXT: Option<String>;
}

struct TelemetrySink {
    file: File,
    path: PathBuf,
    session_id: String,
    started: Instant,
    sequence: u64,
    repository: Option<String>,
    active_diff_snapshot_id: Option<String>,
    emitted_diff_snapshot_ids: HashSet<String>,
}

/// Explicit producer identity for one telemetry record.
///
/// This is deliberately independent of the event name so consumers never have to infer whether
/// an operation came from the human, Codescope's built-in model, or an external coding agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryOrigin {
    /// Codescope lifecycle, comparison, and derived UI state.
    Application,
    /// Direct keyboard, mouse, or terminal interaction.
    User,
    /// Codescope's built-in LLM and its tool loop.
    InternalAgent,
    /// A client using the local `codescope agent` protocol.
    ExternalAgent,
}

impl TelemetryOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::User => "user",
            Self::InternalAgent => "internal_agent",
            Self::ExternalAgent => "external_agent",
        }
    }
}

impl TelemetrySink {
    #[cfg(test)]
    fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_session(path, new_session_id(), false)
    }

    fn open_session(directory: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        for _ in 0..16 {
            let session_id = new_session_id();
            let path = directory.join(format!("{session_id}.jsonl"));
            match Self::open_with_session(&path, session_id, true) {
                Ok(sink) => return Ok(sink),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique telemetry session file",
        ))
    }

    fn open_with_session(path: &Path, session_id: String, create_new: bool) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.append(true);
        if create_new {
            options.create_new(true);
        } else {
            options.create(true);
        }
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
        Ok(Self {
            file,
            path: path.to_path_buf(),
            session_id,
            started: Instant::now(),
            sequence: 0,
            repository: None,
            active_diff_snapshot_id: None,
            emitted_diff_snapshot_ids: HashSet::new(),
        })
    }

    fn write(
        &mut self,
        event: &str,
        data: Value,
        origin: TelemetryOrigin,
        diff_snapshot_override: Option<Option<String>>,
    ) -> io::Result<()> {
        self.sequence = self.sequence.saturating_add(1);
        let mut record = json!({
            "schema_version": SCHEMA_VERSION,
            "timestamp_unix_ms": unix_millis(),
            "elapsed_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "session_id": self.session_id,
            "sequence": self.sequence,
            "repository": self.repository,
            "origin": origin.as_str(),
            "event": event,
            "data": data,
        });
        let diff_snapshot_id =
            diff_snapshot_override.unwrap_or_else(|| self.active_diff_snapshot_id.clone());
        if let (Some(id), Some(object)) = (diff_snapshot_id, record.as_object_mut()) {
            object.insert("diff_snapshot_id".to_string(), Value::String(id));
        }
        // Serialize first so a complete record reaches the append-only session file in one write
        // in the overwhelmingly common case, minimizing partial tail records after interruption.
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()
    }

    fn activate_diff_snapshot(&mut self, epoch: u64, payload: Value) -> io::Result<String> {
        let canonical = serde_json::to_vec(&payload).map_err(io::Error::other)?;
        let id = format!("sha256:{}", sha256_hex(&canonical));
        if !self.emitted_diff_snapshot_ids.contains(&id) {
            self.write(
                "diff.snapshot",
                json!({
                    "diff_snapshot_id": id,
                    "epoch": epoch,
                    "payload": payload,
                }),
                TelemetryOrigin::Application,
                Some(Some(id.clone())),
            )?;
            self.emitted_diff_snapshot_ids.insert(id.clone());
        }
        self.active_diff_snapshot_id = Some(id.clone());
        Ok(id)
    }
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn new_session_id() -> String {
    let timestamp = unix_millis();
    let nonce = SESSION_NONCE.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{}-{nonce}", std::process::id())
}

/// Initialize a process-global append-only telemetry session file inside `directory`.
///
/// Calling this more than once is harmless when the sink is already installed. The first
/// successful directory owns the process for the remainder of its lifetime. Every process gets a
/// distinct `<timestamp>-<pid>-<nonce>.jsonl` file so each stream is scoped to one session instead
/// of extending one global file indefinitely.
pub fn init(directory: impl AsRef<Path>) -> io::Result<PathBuf> {
    if let Some(sink) = SINK.get() {
        return sink
            .lock()
            .map(|sink| sink.path.clone())
            .map_err(|_| io::Error::other("telemetry lock poisoned"));
    }
    let sink = TelemetrySink::open_session(directory.as_ref())?;
    let actual = sink.path.clone();
    let _ = SINK.set(Mutex::new(sink));
    Ok(SINK
        .get()
        .and_then(|sink| sink.lock().ok().map(|sink| sink.path.clone()))
        .unwrap_or(actual))
}

/// Return the active session-file path, if initialization succeeded.
#[must_use]
pub fn path() -> Option<PathBuf> {
    SINK.get()
        .and_then(|sink| sink.lock().ok().map(|sink| sink.path.clone()))
}

/// Return the opaque identity of the active process/session stream.
///
/// Local protocol clients use this as one component of cross-process command correlation; it is
/// already present on every record and contains no repository or credential material.
#[must_use]
pub fn session_id() -> Option<String> {
    SINK.get()
        .and_then(|sink| sink.lock().ok().map(|sink| sink.session_id.clone()))
}

/// Attach a stable opaque repository identity to subsequent events and emit a context event.
/// The canonical root is hashed immediately and is never retained or written.
pub fn set_repository(root: impl Into<String>) {
    let root = root.into();
    let repository_id = format!("sha256:{}", sha256_hex(root.as_bytes()));
    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };
    if sink.repository.as_deref() == Some(repository_id.as_str()) {
        return;
    }
    sink.repository = Some(repository_id.clone());
    sink.active_diff_snapshot_id = None;
    let _ = sink.write(
        "session.repository",
        json!({ "repository_id": repository_id }),
        TelemetryOrigin::Application,
        None,
    );
}

/// Return the active opaque repository identity. The original absolute path is never retained.
#[must_use]
pub fn repository_id() -> Option<String> {
    SINK.get()
        .and_then(|sink| sink.lock().ok().and_then(|sink| sink.repository.clone()))
}

/// Store and activate one canonical, already privacy-filtered diff payload. The returned ID is the
/// SHA-256 of the exact JSON value stored under `data.payload`. A payload is written at most once
/// per telemetry stream; activating an identical comparison simply reuses its existing ID.
pub fn activate_diff_snapshot(epoch: u64, payload: Value) -> Option<String> {
    let sink = SINK.get()?;
    let mut sink = sink.lock().ok()?;
    sink.activate_diff_snapshot(epoch, payload).ok()
}

/// Clear comparison correlation before a refresh or when no valid comparison exists. Subsequent
/// records omit `diff_snapshot_id` until a current parsed comparison is activated.
pub fn mark_diff_snapshot_unavailable(epoch: u64, reason: &str) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if let Ok(mut sink) = sink.lock() {
        sink.active_diff_snapshot_id = None;
        let _ = sink.write(
            "diff.snapshot_unavailable",
            json!({ "epoch": epoch, "reason": reason }),
            TelemetryOrigin::Application,
            Some(None),
        );
    }
}

/// Run an asynchronous operation with an immutable diff correlation. This is used for provider
/// requests so late telemetry remains tied to the comparison that launched the request rather
/// than whichever comparison happens to be globally active when the response arrives.
pub async fn scope_diff_snapshot<F>(diff_snapshot_id: Option<String>, future: F) -> F::Output
where
    F: Future,
{
    DIFF_SNAPSHOT_CONTEXT.scope(diff_snapshot_id, future).await
}

/// Append one structured event to the active process telemetry file.
///
/// Calls made before successful initialization are no-ops. Post-initialization write errors
/// are intentionally isolated from the user operation being observed.
pub fn record(event: &str, data: Value) {
    record_with_origin(TelemetryOrigin::Application, event, data);
}

/// Append one structured event with an explicit producer identity.
///
/// Prefer this for human input, internal-model activity, and local agent-protocol activity. Calls
/// made before successful initialization and post-initialization write failures remain no-ops.
pub fn record_with_origin(origin: TelemetryOrigin, event: &str, data: Value) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if let Ok(mut sink) = sink.lock() {
        let scoped = DIFF_SNAPSHOT_CONTEXT.try_with(Clone::clone).ok();
        let _ = sink.write(event, data, origin, scoped);
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
        sink.repository = Some("sha256:repo-id".into());
        sink.write(
            "input.key",
            json!({"key": "j"}),
            TelemetryOrigin::User,
            None,
        )
        .unwrap();
        sink.write(
            "llm.response",
            json!({"body": {"ok": true}}),
            TelemetryOrigin::InternalAgent,
            None,
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let records = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["schema_version"], 1);
        assert_eq!(records[0]["sequence"], 1);
        assert_eq!(records[0]["repository"], "sha256:repo-id");
        assert_eq!(records[0]["origin"], "user");
        assert_eq!(records[0]["event"], "input.key");
        assert_eq!(records[1]["sequence"], 2);
        assert_eq!(records[1]["origin"], "internal_agent");
        assert_eq!(records[1]["data"]["body"]["ok"], true);
    }

    #[test]
    fn snapshots_are_content_addressed_deduplicated_and_correlate_later_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let mut sink = TelemetrySink::open(&path).unwrap();
        let first_payload = json!({
            "comparison": { "scope": "working" },
            "canonical_diff": "@@ -1 +1 @@\n-café\n+雪\n\\ No newline at end of file\n"
        });
        let first_id = sink
            .activate_diff_snapshot(1, first_payload.clone())
            .unwrap();
        let expected_id = format!(
            "sha256:{}",
            sha256_hex(&serde_json::to_vec(&first_payload).unwrap())
        );
        assert_eq!(first_id, expected_id, "the ID hashes the stored payload");
        sink.write(
            "input.key",
            json!({"key": "j"}),
            TelemetryOrigin::User,
            None,
        )
        .unwrap();
        assert_eq!(
            sink.activate_diff_snapshot(2, first_payload.clone())
                .unwrap(),
            first_id,
            "an unchanged refresh reuses the content identity"
        );
        sink.write(
            "llm.request",
            json!({"body": "prompt"}),
            TelemetryOrigin::InternalAgent,
            None,
        )
        .unwrap();

        let second_payload = json!({
            "comparison": { "scope": "working" },
            "canonical_diff": "@@ -1 +1 @@\n-old\n+new\n"
        });
        let second_id = sink
            .activate_diff_snapshot(3, second_payload.clone())
            .unwrap();
        assert_ne!(first_id, second_id);
        sink.write(
            "input.mouse",
            json!({"row": 4}),
            TelemetryOrigin::User,
            None,
        )
        .unwrap();
        let third_payload = json!({
            "comparison": { "scope": "staged" },
            "canonical_diff": second_payload["canonical_diff"],
        });
        let third_id = sink.activate_diff_snapshot(4, third_payload).unwrap();
        assert_ne!(second_id, third_id);
        sink.write(
            "ui.snapshot",
            json!({"epoch": 4}),
            TelemetryOrigin::Application,
            None,
        )
        .unwrap();
        sink.active_diff_snapshot_id = None;
        sink.write(
            "diff.snapshot_unavailable",
            json!({"epoch": 5}),
            TelemetryOrigin::Application,
            Some(None),
        )
        .unwrap();
        sink.write(
            "input.key",
            json!({"key": "k"}),
            TelemetryOrigin::User,
            None,
        )
        .unwrap();

        let records = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("every line is JSON"))
            .collect::<Vec<_>>();
        let snapshots = records
            .iter()
            .filter(|record| record["event"] == "diff.snapshot")
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 3, "the unchanged payload was not repeated");
        assert_eq!(snapshots[0]["data"]["payload"], first_payload);
        assert_eq!(snapshots[0]["diff_snapshot_id"], first_id);
        assert_eq!(records[1]["diff_snapshot_id"], first_id);
        assert_eq!(records[2]["diff_snapshot_id"], first_id);
        assert_eq!(records[4]["diff_snapshot_id"], second_id);
        assert_eq!(records[6]["diff_snapshot_id"], third_id);
        assert!(records.last().unwrap().get("diff_snapshot_id").is_none());
    }

    #[test]
    fn session_directory_creates_distinct_owner_only_jsonl_streams() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry_dir = dir.path().join("telemetry");
        let first = TelemetrySink::open_session(&telemetry_dir).unwrap();
        let second = TelemetrySink::open_session(&telemetry_dir).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(
            first.path.extension().and_then(|value| value.to_str()),
            Some("jsonl")
        );
        assert_eq!(
            first.path.file_stem().and_then(|value| value.to_str()),
            Some(first.session_id.as_str())
        );
        assert_eq!(first.path.parent(), Some(telemetry_dir.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&telemetry_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn request_override_cannot_inherit_a_newer_active_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let mut sink = TelemetrySink::open(&path).unwrap();
        sink.active_diff_snapshot_id = Some("new".into());
        sink.write(
            "llm.response",
            json!({"done": true}),
            TelemetryOrigin::InternalAgent,
            Some(Some("old".into())),
        )
        .unwrap();
        sink.write(
            "llm.response",
            json!({"unavailable": true}),
            TelemetryOrigin::InternalAgent,
            Some(None),
        )
        .unwrap();

        let records = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records[0]["diff_snapshot_id"], "old");
        assert!(records[1].get("diff_snapshot_id").is_none());
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
