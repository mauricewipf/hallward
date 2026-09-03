//! User-level Gemini API credentials (not stored in `.album/`).

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
}

/// Returned when a saved file key is rejected; the TUI reopens the overlay.
pub const INVALID_SAVED_KEY: &str = "HALLWARD_GEMINI_SAVED_KEY_REJECTED";

/// Returned when `GEMINI_API_KEY` is rejected; the overlay cannot help.
pub const INVALID_ENV_KEY: &str = "HALLWARD_GEMINI_ENV_KEY_REJECTED";

pub fn resolve() -> Option<ResolvedKey> {
    if let Some(key) = key_from_env() {
        return Some(ResolvedKey {
            key,
            source: CredentialSource::Environment,
        });
    }
    key_from_file().map(|key| ResolvedKey {
        key,
        source: CredentialSource::File,
    })
}

pub fn save_gemini_key(key: &str) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("Paste a Gemini API key.".into());
    }
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    write_credentials_file(&tmp, trimmed)?;
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
    "Gemini rejected GEMINI_API_KEY. Unset or replace it and try again.".into()
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
    match std::env::var("GEMINI_API_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => None,
    }
}

fn key_from_file() -> Option<String> {
    let path = credentials_path().ok()?;
    let text = fs::read_to_string(path).ok()?;
    parse_credentials_file(&text)
}

fn credentials_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("HALLWARD_CREDENTIALS_PATH") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "Could not find a home directory for Gemini credentials.".to_string())?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("hallward")
        .join("credentials"))
}

fn parse_credentials_file(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("gemini_api_key=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(unix)]
fn write_credentials_file(path: &Path, key: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    writeln!(file, "gemini_api_key={key}")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_credentials_file(path: &Path, key: &str) -> Result<(), String> {
    fs::write(path, format!("gemini_api_key={key}\n"))
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Serializes tests that mutate HOME / HALLWARD_CREDENTIALS_PATH /
    /// GEMINI_API_KEY so parallel cargo test runs don't race on env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce(PathBuf)>(f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let creds = home.join(".config/hallward/credentials");
        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_key = std::env::var_os("GEMINI_API_KEY");
        std::env::remove_var("HALLWARD_CREDENTIALS_PATH");
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
        match prev_key {
            Some(value) => std::env::set_var("GEMINI_API_KEY", value),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
    }

    #[test]
    fn env_key_wins_over_saved_file() {
        with_home(|path| {
            save_gemini_key("file-key").unwrap();
            std::env::set_var("GEMINI_API_KEY", "env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "env-key");
            assert_eq!(resolved.source, CredentialSource::Environment);
            assert!(path.is_file());
        });
    }

    #[test]
    fn saved_key_round_trip_is_mode_0600() {
        with_home(|path| {
            save_gemini_key("secret-key").unwrap();
            assert_eq!(
                parse_credentials_file(&fs::read_to_string(&path).unwrap()),
                Some("secret-key".into())
            );
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "secret-key");
            assert_eq!(resolved.source, CredentialSource::File);
        });
    }

    #[test]
    fn empty_save_is_rejected() {
        with_home(|_| {
            assert_eq!(
                save_gemini_key("   ").unwrap_err(),
                "Paste a Gemini API key."
            );
        });
    }

    #[test]
    fn explicit_credentials_path_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let prev = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_key = std::env::var_os("GEMINI_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
        save_gemini_key("override-key").unwrap();
        assert_eq!(resolve().unwrap().key, "override-key");
        match prev {
            Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
            None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
        }
        match prev_key {
            Some(value) => std::env::set_var("GEMINI_API_KEY", value),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
    }
}
