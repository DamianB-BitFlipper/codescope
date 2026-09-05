//! Global, repository-independent Codescope configuration.
//!
//! The v1 file lives at `$XDG_CONFIG_HOME/codescope/config.toml` (falling back to
//! `$HOME/.config/codescope/config.toml`, or the platform config directory on Windows).
//! `CODESCOPE_CONFIG` overrides the whole path. Runtime writes are deliberately narrow:
//! only provider-specific AI choices and stable TUI preferences are patched, preserving
//! comments, unknown v1 keys, and manually maintained `[ai]` settings.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use codescope_ai::{AiConfig, AiError, AiFileConfig, ReasoningEffort};
use codescope_tui::{DividerId, UiPreferences};
use serde::Deserialize;
use toml_edit::{DocumentMut, value};

use crate::dispatcher::ConfigPersistence;

const SCHEMA_VERSION: u32 = 1;
const LOCK_WAIT: Duration = Duration::from_secs(2);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

/// Always-on local telemetry session files live in a directory beside the resolved global config.
/// Environments without a discoverable config directory use a process-independent temporary
/// directory.
pub(crate) fn telemetry_dir() -> PathBuf {
    telemetry_dir_for(resolve_config_path(
        |name| std::env::var(name).ok(),
        cfg!(windows),
    ))
}

fn telemetry_dir_for(config_path: Option<PathBuf>) -> PathBuf {
    config_path.map_or_else(
        || std::env::temp_dir().join("codescope").join("telemetry"),
        |path| path.with_file_name("telemetry"),
    )
}

/// Parsed v1 global configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct GlobalConfig {
    version: u32,
    ai: GlobalAiConfig,
    ui: StoredUiPreferences,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            ai: GlobalAiConfig::default(),
            ui: StoredUiPreferences::default(),
        }
    }
}

/// Existing AI file settings plus provider-specific model/reasoning selection slots.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct GlobalAiConfig {
    #[serde(flatten)]
    runtime: AiFileConfig,
    last_model: LastModels,
    last_reasoning_effort: LastReasoningEfforts,
}

/// The last reasoning budget explicitly picked for each provider family.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct LastReasoningEfforts {
    prime: Option<ReasoningEffort>,
    openai: Option<ReasoningEffort>,
    anthropic: Option<ReasoningEffort>,
    custom: Option<ReasoningEffort>,
}

impl LastReasoningEfforts {
    fn get(&self, provider: &str) -> Option<ReasoningEffort> {
        match provider {
            "prime" => self.prime,
            "openai" => self.openai,
            "anthropic" => self.anthropic,
            "custom" => self.custom,
            _ => None,
        }
    }
}

/// The last model explicitly picked for each provider family.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct LastModels {
    prime: Option<String>,
    openai: Option<String>,
    anthropic: Option<String>,
    custom: Option<String>,
}

impl LastModels {
    fn get(&self, provider: &str) -> Option<&str> {
        let value = match provider {
            "prime" => self.prime.as_deref(),
            "openai" => self.openai.as_deref(),
            "anthropic" => self.anthropic.as_deref(),
            "custom" => self.custom.as_deref(),
            _ => None,
        }?;
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

/// Optional-on-disk form so partial `[ui]` tables inherit application defaults.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct StoredUiPreferences {
    diff_wrap: Option<bool>,
    dividers: BTreeMap<String, u16>,
}

impl StoredUiPreferences {
    fn resolved(&self) -> UiPreferences {
        let defaults = UiPreferences::default();
        let mut dividers = defaults.dividers;
        for id in DividerId::ALL {
            if let Some(extent) = self.dividers.get(id.config_key()).copied() {
                dividers.set(id, extent);
            }
        }
        UiPreferences {
            diff_wrap: self.diff_wrap.unwrap_or(defaults.diff_wrap),
            dividers,
        }
    }
}

/// Loaded configuration plus the format-preserving persistence target.
#[derive(Debug, Clone)]
pub(crate) struct ConfigStore {
    path: Option<PathBuf>,
    writable: bool,
    config: GlobalConfig,
    warning: Option<String>,
}

impl ConfigStore {
    /// Load the process-global configuration path. Missing files are normal and writable.
    pub(crate) fn load() -> Self {
        let path = resolve_config_path(|name| std::env::var(name).ok(), cfg!(windows));
        match path {
            Some(path) => Self::load_path(path),
            None => Self {
                path: None,
                writable: false,
                config: GlobalConfig::default(),
                warning: Some(
                    "no global config directory found; preferences will not persist".into(),
                ),
            },
        }
    }

    fn load_path(path: PathBuf) -> Self {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    path: Some(path),
                    writable: true,
                    config: GlobalConfig::default(),
                    warning: None,
                };
            }
            Err(e) => {
                return Self::read_only(path, format!("could not read global config: {e}"));
            }
        };
        let config: GlobalConfig = match toml_edit::de::from_str(&text) {
            Ok(config) => config,
            Err(e) => {
                return Self::read_only(
                    path,
                    format!("global config is malformed and will not be overwritten: {e}"),
                );
            }
        };
        if config.version != SCHEMA_VERSION {
            return Self::read_only(
                path,
                format!(
                    "unsupported global config version {}; expected {SCHEMA_VERSION}; file is read-only",
                    config.version
                ),
            );
        }
        Self {
            path: Some(path),
            writable: true,
            config,
            warning: None,
        }
    }

    fn read_only(path: PathBuf, warning: String) -> Self {
        tracing::warn!(path = %path.display(), %warning);
        Self {
            path: Some(path),
            writable: false,
            config: GlobalConfig::default(),
            warning: Some(warning),
        }
    }

    /// A startup warning suitable for logs or a future first-frame status message.
    pub(crate) fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// Resolve AI settings using CLI overrides, then stored provider choices, then `[ai]`
    /// values, then built-in defaults.
    pub(crate) fn resolve_ai_config(
        &self,
        model_override: Option<&str>,
        reasoning_effort_override: Option<ReasoningEffort>,
    ) -> Result<AiConfig, AiError> {
        self.resolve_ai_with(
            |name| std::env::var(name).ok(),
            model_override,
            reasoning_effort_override,
        )
    }

    fn resolve_ai_with<F>(
        &self,
        env: F,
        model_override: Option<&str>,
        reasoning_effort_override: Option<ReasoningEffort>,
    ) -> Result<AiConfig, AiError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut resolved = AiConfig::resolve(Some(&self.config.ai.runtime), |name| env(name))?;
        if let Some(model) = model_override
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            resolved.model = model.to_string();
        } else if let Some(model) = self.config.ai.last_model.get(resolved.provider_label()) {
            resolved.model = model.to_string();
        }
        if let Some(effort) = reasoning_effort_override {
            resolved.reasoning_effort = effort;
        } else if let Some(effort) = self
            .config
            .ai
            .last_reasoning_effort
            .get(resolved.provider_label())
        {
            resolved.reasoning_effort = effort;
        }
        Ok(resolved)
    }

    /// Stable preferences restored into a fresh TUI app.
    pub(crate) fn ui_preferences(&self) -> UiPreferences {
        self.config.ui.resolved()
    }

    fn patch(&self, update: ConfigUpdate<'_>) -> Result<(), String> {
        if !self.writable {
            return Err(self
                .warning
                .clone()
                .unwrap_or_else(|| "global config is read-only".into()));
        }
        let path = self
            .path
            .as_deref()
            .ok_or_else(|| "no global config path is available".to_string())?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create config directory {}: {e}", parent.display()))?;
        let _lock = ConfigLock::acquire(path)?;

        // Re-read under the cross-process lock so model and UI updates cannot clobber one
        // another. A file that became malformed/newer after startup is never overwritten.
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let mut doc = if text.is_empty() {
            DocumentMut::new()
        } else {
            DocumentMut::from_str(&text)
                .map_err(|e| format!("refusing to overwrite malformed config: {e}"))?
        };
        let version = document_version(&doc)?;
        if version != SCHEMA_VERSION {
            return Err(format!(
                "refusing to overwrite config version {version}; expected {SCHEMA_VERSION}"
            ));
        }
        doc["version"] = value(i64::from(SCHEMA_VERSION));
        match update {
            ConfigUpdate::Model { provider, model } => {
                let slot = provider_slot(provider)?;
                doc["ai"]["last_model"][slot] = value(model);
            }
            ConfigUpdate::ReasoningEffort { provider, effort } => {
                let slot = provider_slot(provider)?;
                doc["ai"]["last_reasoning_effort"][slot] = value(effort.as_str());
            }
            ConfigUpdate::Ui(preferences) => {
                doc["ui"]["diff_wrap"] = value(preferences.diff_wrap);
                for id in DividerId::ALL {
                    doc["ui"]["dividers"][id.config_key()] =
                        value(i64::from(preferences.dividers.get(id)));
                }
            }
        }
        atomic_write(path, doc.to_string().as_bytes())
    }
}

impl ConfigPersistence for ConfigStore {
    fn persist_model(&self, provider: &str, model: &str) -> Result<(), String> {
        if model.trim().is_empty() {
            return Err("refusing to persist an empty model name".into());
        }
        self.patch(ConfigUpdate::Model { provider, model })
    }

    fn persist_reasoning_effort(
        &self,
        provider: &str,
        effort: ReasoningEffort,
    ) -> Result<(), String> {
        self.patch(ConfigUpdate::ReasoningEffort { provider, effort })
    }

    fn persist_ui(&self, preferences: UiPreferences) -> Result<(), String> {
        self.patch(ConfigUpdate::Ui(preferences))
    }
}

enum ConfigUpdate<'a> {
    Model {
        provider: &'a str,
        model: &'a str,
    },
    ReasoningEffort {
        provider: &'a str,
        effort: ReasoningEffort,
    },
    Ui(UiPreferences),
}

fn provider_slot(provider: &str) -> Result<&'static str, String> {
    match provider {
        "prime" => Ok("prime"),
        "openai" => Ok("openai"),
        "anthropic" => Ok("anthropic"),
        "custom" => Ok("custom"),
        other => Err(format!("unknown AI provider slot {other:?}")),
    }
}

fn document_version(doc: &DocumentMut) -> Result<u32, String> {
    let Some(item) = doc.get("version") else {
        return Ok(SCHEMA_VERSION); // partial pre-version v1 files are accepted
    };
    let version = item
        .as_integer()
        .ok_or_else(|| "config version must be an integer".to_string())?;
    u32::try_from(version).map_err(|_| format!("invalid config version {version}"))
}

/// Resolve the global path with an injectable environment/platform for unit tests.
fn resolve_config_path<F>(env: F, windows: bool) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let get = |name: &str| env(name).filter(|value| !value.trim().is_empty());
    if let Some(path) = get("CODESCOPE_CONFIG") {
        return Some(PathBuf::from(path));
    }
    if let Some(root) = get("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(root).join("codescope").join("config.toml"));
    }
    if windows {
        if let Some(root) = get("APPDATA") {
            return Some(PathBuf::from(root).join("codescope").join("config.toml"));
        }
    }
    get("HOME").map(|root| {
        PathBuf::from(root)
            .join(".config")
            .join("codescope")
            .join("config.toml")
    })
}

/// A small cross-process sibling lock. It is intentionally dependency-free: exclusive
/// `create_new` is the lock primitive, and a crashed writer's old lock is recoverable.
struct ConfigLock {
    path: PathBuf,
}

impl ConfigLock {
    fn acquire(config_path: &Path) -> Result<Self, String> {
        let lock_path = sibling_path(config_path, "lock");
        let start = std::time::Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path: lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock_path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > STALE_LOCK_AGE);
                    if stale {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if start.elapsed() >= LOCK_WAIT {
                        return Err(format!(
                            "timed out waiting for config lock {}",
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(format!("create config lock {}: {e}", lock_path.display())),
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(".{name}.{suffix}"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".config.toml.tmp-")
        .tempfile_in(parent)
        .map_err(|e| format!("create temporary config in {}: {e}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("set temporary config permissions: {e}"))?;
    }
    temp.write_all(bytes)
        .map_err(|e| format!("write temporary config: {e}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|e| format!("flush temporary config: {e}"))?;
    let persisted = temp
        .persist(path)
        .map_err(|e| format!("replace config {}: {}", path.display(), e.error))?;
    persisted
        .sync_all()
        .map_err(|e| format!("flush persisted config {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut directory = OpenOptions::new();
        directory.read(true).custom_flags(0);
        if let Ok(dir) = directory.open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name| values.get(name).cloned()
    }

    #[test]
    fn path_precedence_and_platform_fallbacks() {
        let p = resolve_config_path(
            env(&[
                ("CODESCOPE_CONFIG", "/override/c.toml"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ]),
            false,
        );
        assert_eq!(p, Some(PathBuf::from("/override/c.toml")));
        let p = resolve_config_path(
            env(&[("CODESCOPE_CONFIG", "  "), ("XDG_CONFIG_HOME", "/xdg")]),
            false,
        );
        assert_eq!(p, Some(PathBuf::from("/xdg/codescope/config.toml")));
        let p = resolve_config_path(env(&[("HOME", "/home/u")]), false);
        assert_eq!(
            p,
            Some(PathBuf::from("/home/u/.config/codescope/config.toml"))
        );
        let p = resolve_config_path(env(&[("APPDATA", "C:/Users/u/AppData")]), true);
        assert_eq!(
            p,
            Some(PathBuf::from("C:/Users/u/AppData/codescope/config.toml"))
        );
    }

    #[test]
    fn telemetry_directory_is_a_sibling_of_the_resolved_config() {
        assert_eq!(
            telemetry_dir_for(Some(PathBuf::from("/xdg/codescope/custom.toml"))),
            PathBuf::from("/xdg/codescope/telemetry")
        );
        assert_eq!(
            telemetry_dir_for(None),
            std::env::temp_dir().join("codescope").join("telemetry")
        );
    }

    #[test]
    fn partial_config_and_model_precedence_are_provider_specific() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"version = 1
[ai]
model = "file/fallback"
[ai.last_model]
prime = "prime/remembered"
openai = "openai-remembered"
custom = "custom-remembered"
[ai.last_reasoning_effort]
prime = "medium"
openai = "low"
[ui]
diff_wrap = true
"#,
        )
        .unwrap();
        let store = ConfigStore::load_path(path);
        let prime = store
            .resolve_ai_with(env(&[("PRIME_API_KEY", "sk-prime")]), None, None)
            .unwrap();
        assert_eq!(prime.model, "prime/remembered");
        assert_eq!(prime.reasoning_effort, ReasoningEffort::Medium);
        assert!(prime.api_key.is_some());
        let openai = store
            .resolve_ai_with(env(&[("OPENAI_API_KEY", "sk-openai")]), None, None)
            .unwrap();
        assert_eq!(openai.model, "openai-remembered");
        assert_eq!(openai.reasoning_effort, ReasoningEffort::Low);
        let cli_override = store
            .resolve_ai_with(
                env(&[("PRIME_API_KEY", "sk-prime")]),
                Some("cli/model"),
                Some(ReasoningEffort::High),
            )
            .unwrap();
        assert_eq!(cli_override.model, "cli/model");
        assert_eq!(cli_override.reasoning_effort, ReasoningEffort::High);
        let anthropic_fallback = store
            .resolve_ai_with(env(&[("ANTHROPIC_API_KEY", "sk-anthropic")]), None, None)
            .unwrap();
        assert_eq!(
            anthropic_fallback.model, "file/fallback",
            "[ai].model remains the fallback when that provider has no remembered choice"
        );
        let custom = store
            .resolve_ai_with(
                env(&[
                    ("OPENAI_API_KEY", "sk-openai"),
                    ("CODESCOPE_AI_BASE_URL", "http://127.0.0.1:11434/v1"),
                ]),
                None,
                None,
            )
            .unwrap();
        assert_eq!(custom.provider_label(), "custom");
        assert_eq!(custom.model, "custom-remembered");
        assert!(store.ui_preferences().diff_wrap);
    }

    #[test]
    fn named_key_env_uses_the_matching_provider_model_slot() {
        for (name, provider, slot) in [
            ("PRIME_API_KEY", "prime", "prime"),
            ("OPENAI_API_KEY", "openai", "openai"),
            ("ANTHROPIC_API_KEY", "anthropic", "anthropic"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(
                &path,
                format!(
                    "version = 1\n[ai]\napi_key_env = {name:?}\n[ai.last_model]\n{slot} = \"remembered/{provider}\"\n"
                ),
            )
            .unwrap();
            let config = ConfigStore::load_path(path)
                .resolve_ai_with(env(&[(name, "secret")]), None, None)
                .unwrap();
            assert_eq!(config.provider_label(), provider, "{name}");
            assert_eq!(config.model, format!("remembered/{provider}"), "{name}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "version = 1\n[ai]\napi_key_env = \"LOCAL_KEY\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n[ai.last_model]\ncustom = \"local/remembered\"\n",
        )
        .unwrap();
        let custom = ConfigStore::load_path(path)
            .resolve_ai_with(env(&[("LOCAL_KEY", "secret")]), None, None)
            .unwrap();
        assert_eq!(custom.provider_label(), "custom");
        assert_eq!(custom.model, "local/remembered");
    }

    #[test]
    fn literal_api_key_in_global_file_is_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "version = 1\n[ai]\napi_key = \"do-not-store-this\"\n",
        )
        .unwrap();
        let store = ConfigStore::load_path(path);
        assert!(matches!(
            store.resolve_ai_with(env(&[]), None, None),
            Err(AiError::LiteralApiKeyInConfig)
        ));
    }

    #[test]
    fn malformed_and_future_files_are_defaults_and_read_only() {
        for (name, text) in [
            ("malformed", "[ai\nmodel = nope"),
            ("future", "version = 99\n[ui]\ndiff_wrap = true\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("{name}.toml"));
            std::fs::write(&path, text).unwrap();
            let store = ConfigStore::load_path(path.clone());
            assert!(store.warning().is_some());
            assert!(!store.ui_preferences().diff_wrap);
            assert!(store.persist_model("prime", "new").is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        }
    }

    #[test]
    fn updates_preserve_comments_unknown_keys_and_never_add_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# keep me\nversion = 1\nunknown = \"yes\"\n[ai]\napi_key_env = \"OPENAI_API_KEY\"\n[ui]\ncustom_ui_key = \"keep\"\n",
        )
        .unwrap();
        let store = ConfigStore::load_path(path.clone());
        store.persist_model("openai", "gpt-test").unwrap();
        store
            .persist_reasoning_effort("openai", ReasoningEffort::XHigh)
            .unwrap();
        let mut preferences = UiPreferences {
            diff_wrap: true,
            ..UiPreferences::default()
        };
        preferences.dividers.set(DividerId::FilesDiff, 51);
        preferences.dividers.set(DividerId::WorkReview, 12);
        preferences
            .dividers
            .set(DividerId::RelationshipsGenerated, 61);
        preferences.dividers.set(DividerId::SelectedCallers, 6);
        preferences.dividers.set(DividerId::CallersDownstream, 3);
        store.persist_ui(preferences).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert!(text.contains("unknown = \"yes\""));
        assert!(text.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(text.contains("custom_ui_key = \"keep\""));
        assert!(!text.contains("sk-"));
        let reloaded = ConfigStore::load_path(path);
        assert_eq!(
            reloaded.config.ai.last_model.openai.as_deref(),
            Some("gpt-test")
        );
        assert_eq!(
            reloaded.config.ai.last_reasoning_effort.openai,
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            reloaded.ui_preferences().dividers.get(DividerId::FilesDiff),
            51
        );
        assert_eq!(
            reloaded
                .ui_preferences()
                .dividers
                .get(DividerId::RelationshipsGenerated),
            61
        );
        assert!(text.contains("dividers = {"), "generic divider map: {text}");
        assert_eq!(
            reloaded
                .ui_preferences()
                .dividers
                .get(DividerId::SelectedCallers),
            6
        );
        assert_eq!(
            reloaded
                .ui_preferences()
                .dividers
                .get(DividerId::CallersDownstream),
            3
        );
    }

    #[test]
    fn concurrent_model_and_ui_updates_merge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let store = Arc::new(ConfigStore::load_path(path.clone()));
        let a = {
            let store = store.clone();
            std::thread::spawn(move || store.persist_model("anthropic", "claude-test"))
        };
        let b = {
            let store = store.clone();
            std::thread::spawn(move || {
                store.persist_ui(UiPreferences {
                    diff_wrap: true,
                    ..UiPreferences::default()
                })
            })
        };
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        let loaded = ConfigStore::load_path(path);
        assert_eq!(
            loaded.config.ai.last_model.anthropic.as_deref(),
            Some("claude-test")
        );
        assert!(loaded.ui_preferences().diff_wrap);
    }

    #[cfg(unix)]
    #[test]
    fn new_config_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let store = ConfigStore::load_path(path.clone());
        store.persist_model("prime", "test").unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
