//! User-level OpenRouter API credentials (not stored in `.album/`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub key: String,
    pub source: CredentialSource,
    pub ask_model: String,
    pub edit_model: String,
}

impl ResolvedKey {
    pub fn new(key: impl Into<String>, source: CredentialSource) -> Self {
        Self {
            key: key.into(),
            source,
            ask_model: DEFAULT_ASK_MODEL.to_string(),
            edit_model: DEFAULT_EDIT_MODEL.to_string(),
        }
    }
}

/// Default vision Q&A model written into new credential files.
pub const DEFAULT_ASK_MODEL: &str = "google/gemini-3.8-flash";
/// Default image-edit model written into new credential files.
pub const DEFAULT_EDIT_MODEL: &str = "google/gemini-3.1-flash-image";

/// Returned when a saved file key is rejected; the TUI reopens the overlay.
pub const INVALID_SAVED_KEY: &str = "HALLWARD_GEMINI_SAVED_KEY_REJECTED";

/// Returned when the environment key is rejected; the overlay cannot help.
pub const INVALID_ENV_KEY: &str = "HALLWARD_GEMINI_ENV_KEY_REJECTED";

const CREDENTIALS_KEY: &str = "OPENROUTER_API_KEY";
const CREDENTIALS_PREFIX: &str = "OPENROUTER_API_KEY=";
const ASK_MODEL_KEY: &str = "ASK_MODEL";
const ASK_MODEL_PREFIX: &str = "ASK_MODEL=";
const EDIT_MODEL_KEY: &str = "EDIT_MODEL";
const EDIT_MODEL_PREFIX: &str = "EDIT_MODEL=";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedCredentials {
    openrouter_key: Option<String>,
    legacy_gemini_key: Option<String>,
    legacy_lowercase_key: Option<String>,
    ask_model: Option<String>,
    edit_model: Option<String>,
}

impl ParsedCredentials {
    fn resolved_key(&self) -> Option<String> {
        self.openrouter_key
            .clone()
            .or_else(|| self.legacy_gemini_key.clone())
            .or_else(|| self.legacy_lowercase_key.clone())
    }

    fn ask_model(&self) -> String {
        self.ask_model
            .clone()
            .unwrap_or_else(|| DEFAULT_ASK_MODEL.to_string())
    }

    fn edit_model(&self) -> String {
        self.edit_model
            .clone()
            .unwrap_or_else(|| DEFAULT_EDIT_MODEL.to_string())
    }
}

pub fn resolve() -> Option<ResolvedKey> {
    let parsed = load_parsed();
    let models = parsed.as_ref();
    if let Some(key) = key_from_env() {
        return Some(ResolvedKey {
            key,
            source: CredentialSource::Environment,
            ask_model: models.map(ParsedCredentials::ask_model).unwrap_or_else(|| {
                DEFAULT_ASK_MODEL.to_string()
            }),
            edit_model: models
                .map(ParsedCredentials::edit_model)
                .unwrap_or_else(|| DEFAULT_EDIT_MODEL.to_string()),
        });
    }
    let parsed = parsed?;
    let key = parsed.resolved_key()?;
    Some(ResolvedKey {
        key,
        source: CredentialSource::File,
        ask_model: parsed.ask_model(),
        edit_model: parsed.edit_model(),
    })
}

/// Models from `~/.config/hallward/credentials`, or the built-in defaults.
pub fn effective_models() -> (String, String) {
    match load_parsed() {
        Some(parsed) => (parsed.ask_model(), parsed.edit_model()),
        None => (
            DEFAULT_ASK_MODEL.to_string(),
            DEFAULT_EDIT_MODEL.to_string(),
        ),
    }
}

pub fn save_api_key(key: &str) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("Paste an OpenRouter API key.".into());
    }
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    write_credentials_file(&tmp, trimmed, DEFAULT_ASK_MODEL, DEFAULT_EDIT_MODEL)?;
    fs::rename(&tmp, &path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("Could not save {}: {error}", path.display())
    })?;
    Ok(())
}

pub fn clear_saved_key() -> Result<(), String> {
    let path = credentials_path()?;
    if path.is_file() {
        fs::remove_file(&path)
            .map_err(|error| format!("Could not remove {}: {error}", path.display()))?;
    }
    Ok(())
}

pub fn invalid_env_key_message() -> String {
    "OpenRouter rejected OPENROUTER_API_KEY. Unset or replace it and try again.".into()
}

pub fn invalid_saved_key_message() -> String {
    INVALID_SAVED_KEY.into()
}

pub fn is_invalid_saved_key_error(text: &str) -> bool {
    text == INVALID_SAVED_KEY
}

pub fn is_invalid_env_key_error(text: &str) -> bool {
    text == INVALID_ENV_KEY
}

fn key_from_env() -> Option<String> {
    for name in ["OPENROUTER_API_KEY", "GEMINI_API_KEY"] {
        if let Ok(key) = std::env::var(name) {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn load_parsed() -> Option<ParsedCredentials> {
    let path = credentials_path().ok()?;
    let text = fs::read_to_string(path).ok()?;
    Some(parse_credentials_text(&text))
}

fn credentials_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("HALLWARD_CREDENTIALS_PATH") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "Could not find a home directory for API credentials.".to_string())?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("hallward")
        .join("credentials"))
}

fn parse_credentials_text(text: &str) -> ParsedCredentials {
    let mut parsed = ParsedCredentials::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(CREDENTIALS_PREFIX) {
            let value = value.trim();
            if !value.is_empty() {
                parsed.openrouter_key = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix(ASK_MODEL_PREFIX) {
            let value = value.trim();
            if !value.is_empty() {
                parsed.ask_model = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix(EDIT_MODEL_PREFIX) {
            let value = value.trim();
            if !value.is_empty() {
                parsed.edit_model = Some(value.to_string());
            }
            continue;
        }
        if parsed.legacy_gemini_key.is_none() {
            if let Some(value) = line.strip_prefix("GEMINI_API_KEY=") {
                let value = value.trim();
                if !value.is_empty() {
                    parsed.legacy_gemini_key = Some(value.to_string());
                }
            }
        }
        if parsed.legacy_lowercase_key.is_none() {
            if let Some(value) = line.strip_prefix("gemini_api_key=") {
                let value = value.trim();
                if !value.is_empty() {
                    parsed.legacy_lowercase_key = Some(value.to_string());
                }
            }
        }
    }
    parsed
}

#[cfg(unix)]
fn write_credentials_file(
    path: &Path,
    key: &str,
    ask_model: &str,
    edit_model: &str,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    writeln!(file, "{CREDENTIALS_KEY}={key}")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    writeln!(file, "{ASK_MODEL_KEY}={ask_model}")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    writeln!(file, "{EDIT_MODEL_KEY}={edit_model}")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_credentials_file(
    path: &Path,
    key: &str,
    ask_model: &str,
    edit_model: &str,
) -> Result<(), String> {
    fs::write(
        path,
        format!(
            "{CREDENTIALS_KEY}={key}\n{ASK_MODEL_KEY}={ask_model}\n{EDIT_MODEL_KEY}={edit_model}\n"
        ),
    )
    .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Serializes tests that mutate HOME / HALLWARD_CREDENTIALS_PATH /
    /// OPENROUTER_API_KEY so parallel cargo test runs don't race on env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce(PathBuf)>(f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let creds = home.join(".config/hallward/credentials");
        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
        let prev_gemini = std::env::var_os("GEMINI_API_KEY");
        std::env::remove_var("HALLWARD_CREDENTIALS_PATH");
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("HOME", &home);
        f(creds);
        match prev_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match prev_path {
            Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
            None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
        }
        match prev_openrouter {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
        match prev_gemini {
            Some(value) => std::env::set_var("GEMINI_API_KEY", value),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
    }

    #[test]
    fn env_key_wins_over_saved_file() {
        with_home(|path| {
            save_api_key("file-key").unwrap();
            std::env::set_var("OPENROUTER_API_KEY", "env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "env-key");
            assert_eq!(resolved.source, CredentialSource::Environment);
            assert_eq!(resolved.ask_model, DEFAULT_ASK_MODEL);
            assert!(path.is_file());
        });
    }

    #[test]
    fn saved_key_round_trip_is_mode_0600() {
        with_home(|path| {
            save_api_key("secret-key").unwrap();
            let contents = fs::read_to_string(&path).unwrap();
            assert_eq!(
                contents,
                format!(
                    "OPENROUTER_API_KEY=secret-key\nASK_MODEL={DEFAULT_ASK_MODEL}\nEDIT_MODEL={DEFAULT_EDIT_MODEL}\n"
                )
            );
            let parsed = parse_credentials_text(&contents);
            assert_eq!(parsed.resolved_key(), Some("secret-key".into()));
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "secret-key");
            assert_eq!(resolved.source, CredentialSource::File);
            assert_eq!(resolved.ask_model, DEFAULT_ASK_MODEL);
            assert_eq!(resolved.edit_model, DEFAULT_EDIT_MODEL);
        });
    }

    #[test]
    fn custom_models_in_file_are_used() {
        with_home(|path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                "OPENROUTER_API_KEY=secret\nASK_MODEL=custom/ask\nEDIT_MODEL=custom/edit\n",
            )
            .unwrap();
            let resolved = resolve().unwrap();
            assert_eq!(resolved.ask_model, "custom/ask");
            assert_eq!(resolved.edit_model, "custom/edit");
            assert_eq!(
                effective_models(),
                ("custom/ask".into(), "custom/edit".into())
            );
        });
    }

    #[test]
    fn env_key_uses_models_from_credentials_file() {
        with_home(|path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                "OPENROUTER_API_KEY=ignored\nASK_MODEL=custom/ask\nEDIT_MODEL=custom/edit\n",
            )
            .unwrap();
            std::env::set_var("OPENROUTER_API_KEY", "env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "env-key");
            assert_eq!(resolved.ask_model, "custom/ask");
            assert_eq!(resolved.edit_model, "custom/edit");
        });
    }

    #[test]
    fn empty_save_is_rejected() {
        with_home(|_| {
            assert_eq!(
                save_api_key("   ").unwrap_err(),
                "Paste an OpenRouter API key."
            );
        });
    }

    #[test]
    fn explicit_credentials_path_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let prev = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
        let prev_gemini = std::env::var_os("GEMINI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
        save_api_key("override-key").unwrap();
        assert_eq!(resolve().unwrap().key, "override-key");
        match prev {
            Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
            None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
        }
        match prev_openrouter {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
        match prev_gemini {
            Some(value) => std::env::set_var("GEMINI_API_KEY", value),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
    }

    #[test]
    fn legacy_gemini_env_key_still_loads() {
        with_home(|_| {
            std::env::set_var("GEMINI_API_KEY", "legacy-env");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "legacy-env");
            assert_eq!(resolved.ask_model, DEFAULT_ASK_MODEL);
        });
    }

    #[test]
    fn legacy_gemini_file_key_still_loads() {
        with_home(|path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "GEMINI_API_KEY=legacy-file\n").unwrap();
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "legacy-file");
            assert_eq!(resolved.ask_model, DEFAULT_ASK_MODEL);
        });
    }

    #[test]
    fn legacy_lowercase_key_still_loads() {
        with_home(|path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "gemini_api_key=legacy-key\n").unwrap();
            assert_eq!(resolve().unwrap().key, "legacy-key");
        });
    }
}
