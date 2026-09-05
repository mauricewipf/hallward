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
pub const DEFAULT_ASK_MODEL: &str = "google/gemini-3.5-flash-lite";
/// Default image-edit model written into new credential files.
pub const DEFAULT_EDIT_MODEL: &str = "google/gemini-3.1-flash-lite-image";

/// Returned when a saved file key is rejected; the TUI reopens the setup overlay.
pub const INVALID_SAVED_KEY: &str = "HALLWARD_OPENROUTER_SAVED_KEY_REJECTED";

/// Returned when the environment key is rejected.
pub const INVALID_ENV_KEY: &str = "HALLWARD_OPENROUTER_ENV_KEY_REJECTED";

const CREDENTIALS_KEY: &str = "OPENROUTER_API_KEY";
const CREDENTIALS_PREFIX: &str = "OPENROUTER_API_KEY=";
const ASK_MODEL_KEY: &str = "ASK_MODEL";
const ASK_MODEL_PREFIX: &str = "ASK_MODEL=";
const EDIT_MODEL_KEY: &str = "EDIT_MODEL";
const EDIT_MODEL_PREFIX: &str = "EDIT_MODEL=";

const ENV_KEY: &str = "HALLWARD_OPENROUTER_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialIssue {
    Missing,
    InsecurePermissions { chmod_hint: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ParsedCredentials {
    openrouter_key: Option<String>,
    ask_model: Option<String>,
    edit_model: Option<String>,
}

impl ParsedCredentials {
    fn resolved_key(&self) -> Option<String> {
        self.openrouter_key.clone()
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

/// Why Ask AI / image edit cannot resolve a key (for the TUI setup overlay).
pub fn credential_issue() -> Option<CredentialIssue> {
    if key_from_env().is_some() {
        return None;
    }
    let path = credentials_path().ok()?;
    if !path.is_file() {
        return Some(CredentialIssue::Missing);
    }
    if let Err(hint) = check_secure_permissions(&path) {
        return Some(CredentialIssue::InsecurePermissions { chmod_hint: hint });
    }
    let text = fs::read_to_string(&path).ok()?;
    let parsed = parse_credentials_text(&text);
    if parsed.resolved_key().is_some() {
        None
    } else {
        Some(CredentialIssue::Missing)
    }
}

pub fn resolve() -> Option<ResolvedKey> {
    let parsed = load_parsed();
    let models = parsed.as_ref();
    if let Some(key) = key_from_env() {
        return Some(ResolvedKey {
            key,
            source: CredentialSource::Environment,
            ask_model: models
                .map(ParsedCredentials::ask_model)
                .unwrap_or_else(|| DEFAULT_ASK_MODEL.to_string()),
            edit_model: models
                .map(ParsedCredentials::edit_model)
                .unwrap_or_else(|| DEFAULT_EDIT_MODEL.to_string()),
        });
    }
    let path = credentials_path().ok()?;
    if !path.is_file() {
        return None;
    }
    check_secure_permissions(&path).ok()?;
    let parsed = parsed?;
    let key = parsed.resolved_key()?;
    Some(ResolvedKey {
        key,
        source: CredentialSource::File,
        ask_model: parsed.ask_model(),
        edit_model: parsed.edit_model(),
    })
}

/// Models from the credentials file, or the built-in defaults.
pub fn effective_models() -> (String, String) {
    match load_parsed() {
        Some(parsed) => (parsed.ask_model(), parsed.edit_model()),
        None => (
            DEFAULT_ASK_MODEL.to_string(),
            DEFAULT_EDIT_MODEL.to_string(),
        ),
    }
}

/// Path to the credentials file (honors `HALLWARD_CREDENTIALS_PATH` and XDG).
pub fn credentials_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("HALLWARD_CREDENTIALS_PATH") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    Ok(config_dir()?.join("credentials"))
}

/// Hallward config directory (`~/.config/hallward` or `$XDG_CONFIG_HOME/hallward`).
pub fn config_dir() -> Result<PathBuf, String> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed).join("hallward"));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "Could not find a home directory for API credentials.".to_string())?;
    Ok(PathBuf::from(home).join(".config").join("hallward"))
}

/// Write the OpenRouter key to the credentials file (CLI-only writer).
pub fn set_key(key: &str) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("Provide a non-empty OpenRouter API key.".into());
    }
    let path = credentials_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| "Could not determine credentials directory.".to_string())?;
    ensure_config_dir(dir)?;
    let existing = if path.is_file() {
        fs::read_to_string(&path)
            .ok()
            .map(|text| parse_credentials_text(&text))
    } else {
        None
    };
    let ask_model = existing
        .as_ref()
        .map(ParsedCredentials::ask_model)
        .unwrap_or_else(|| DEFAULT_ASK_MODEL.to_string());
    let edit_model = existing
        .as_ref()
        .map(ParsedCredentials::edit_model)
        .unwrap_or_else(|| DEFAULT_EDIT_MODEL.to_string());
    let tmp = path.with_extension("tmp");
    write_credentials_file(&tmp, trimmed, &ask_model, &edit_model)?;
    fs::rename(&tmp, &path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("Could not save {}: {error}", path.display())
    })?;
    Ok(())
}

/// Remove the credentials file.
pub fn clear() -> Result<(), String> {
    let path = credentials_path()?;
    if path.is_file() {
        fs::remove_file(&path)
            .map_err(|error| format!("Could not remove {}: {error}", path.display()))?;
    }
    Ok(())
}

pub fn invalid_env_key_message() -> String {
    format!("OpenRouter rejected {ENV_KEY}. Unset or replace it and try again.")
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

/// Read a secret line from stdin with echo disabled (Unix TTY). Never prints the value.
pub fn read_secret_from_stdin() -> Result<String, String> {
    #[cfg(unix)]
    {
        use crossterm::event::{self, Event, KeyCode, KeyModifiers};
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

        enable_raw_mode().map_err(|error| format!("Could not read from terminal: {error}"))?;
        let result = (|| {
            let mut secret = String::new();
            loop {
                match event::read()
                    .map_err(|error| format!("Could not read from terminal: {error}"))?
                {
                    Event::Key(key) => match key.code {
                        KeyCode::Enter if key.modifiers.is_empty() => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Err("Cancelled.".into());
                        }
                        KeyCode::Char(c)
                            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                        {
                            secret.push(c);
                        }
                        KeyCode::Backspace if key.modifiers.is_empty() => {
                            secret.pop();
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            Ok(secret.trim().to_string())
        })();
        let _ = disable_raw_mode();
        eprintln!();
        result
    }
    #[cfg(not(unix))]
    {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| format!("Could not read from stdin: {error}"))?;
        Ok(line.trim().to_string())
    }
}

fn key_from_env() -> Option<String> {
    if let Ok(key) = std::env::var(ENV_KEY) {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn load_parsed() -> Option<ParsedCredentials> {
    let path = credentials_path().ok()?;
    if !path.is_file() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    Some(parse_credentials_text(&text))
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
    }
    parsed
}

fn ensure_config_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        #[cfg(unix)]
        fix_dir_mode(dir)?;
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
            .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn fix_dir_mode(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(dir)
        .map_err(|error| format!("Could not read {}: {error}", dir.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not set {} to mode 0700: {error}", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn check_secure_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(dir) = path.parent() {
        if dir.is_dir() {
            let mode = fs::metadata(dir)
                .map_err(|error| format!("Could not read {}: {error}", dir.display()))?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o700 {
                return Err(chmod_hint(path));
            }
        }
    }
    let mode = fs::metadata(path)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(chmod_hint(path));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_secure_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn chmod_hint(path: &Path) -> String {
    let path_display = path.display();
    if let Some(parent) = path.parent() {
        format!("chmod 700 {}\nchmod 600 {}", parent.display(), path_display)
    } else {
        format!("chmod 600 {path_display}")
    }
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
    /// HALLWARD_OPENROUTER_API_KEY so parallel cargo test runs don't race on env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce(PathBuf)>(f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let creds = home.join(".config/hallward/credentials");
        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_hallward = std::env::var_os(ENV_KEY);
        let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
        std::env::remove_var("HALLWARD_CREDENTIALS_PATH");
        std::env::remove_var(ENV_KEY);
        std::env::remove_var("OPENROUTER_API_KEY");
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
        match prev_hallward {
            Some(value) => std::env::set_var(ENV_KEY, value),
            None => std::env::remove_var(ENV_KEY),
        }
        match prev_openrouter {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[test]
    fn hallward_env_key_wins_over_saved_file() {
        with_home(|path| {
            set_key("file-key").unwrap();
            std::env::set_var(ENV_KEY, "env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "env-key");
            assert_eq!(resolved.source, CredentialSource::Environment);
            assert_eq!(resolved.ask_model, DEFAULT_ASK_MODEL);
            assert!(path.is_file());
        });
    }

    #[test]
    fn bare_openrouter_env_does_not_unlock_resolve() {
        with_home(|path| {
            set_key("file-key").unwrap();
            std::env::set_var("OPENROUTER_API_KEY", "shared-env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "file-key");
            assert_eq!(resolved.source, CredentialSource::File);
            assert!(path.is_file());
        });
    }

    #[test]
    fn saved_key_round_trip_is_mode_0600() {
        with_home(|path| {
            set_key("secret-key").unwrap();
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
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
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
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
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
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            std::env::set_var(ENV_KEY, "env-key");
            let resolved = resolve().unwrap();
            assert_eq!(resolved.key, "env-key");
            assert_eq!(resolved.ask_model, "custom/ask");
            assert_eq!(resolved.edit_model, "custom/edit");
        });
    }

    #[test]
    fn empty_set_is_rejected() {
        with_home(|_| {
            assert_eq!(
                set_key("   ").unwrap_err(),
                "Provide a non-empty OpenRouter API key."
            );
        });
    }

    #[test]
    fn explicit_credentials_path_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let prev = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_hallward = std::env::var_os(ENV_KEY);
        let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
        std::env::remove_var(ENV_KEY);
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
        set_key("override-key").unwrap();
        assert_eq!(resolve().unwrap().key, "override-key");
        match prev {
            Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
            None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
        }
        match prev_hallward {
            Some(value) => std::env::set_var(ENV_KEY, value),
            None => std::env::remove_var(ENV_KEY),
        }
        match prev_openrouter {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }

    #[test]
    fn set_preserves_existing_models() {
        with_home(|path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                "OPENROUTER_API_KEY=old\nASK_MODEL=custom/ask\nEDIT_MODEL=custom/edit\n",
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
            set_key("new-key").unwrap();
            let contents = fs::read_to_string(&path).unwrap();
            assert!(contents.contains("OPENROUTER_API_KEY=new-key"));
            assert!(contents.contains("ASK_MODEL=custom/ask"));
            assert!(contents.contains("EDIT_MODEL=custom/edit"));
        });
    }

    #[test]
    fn insecure_file_permissions_block_resolve() {
        with_home(|path| {
            set_key("secret-key").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(resolve().is_none());
            assert!(matches!(
                credential_issue(),
                Some(CredentialIssue::InsecurePermissions { .. })
            ));
        });
    }

    #[test]
    fn clear_removes_credentials_file() {
        with_home(|path| {
            set_key("secret-key").unwrap();
            assert!(path.is_file());
            clear().unwrap();
            assert!(!path.is_file());
            assert!(resolve().is_none());
        });
    }

    #[test]
    fn xdg_config_home_is_honored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let xdg = dir.path().join("xdg");
        let creds = xdg.join("hallward/credentials");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::remove_var("HALLWARD_CREDENTIALS_PATH");
        std::env::remove_var(ENV_KEY);
        set_key("xdg-key").unwrap();
        assert!(creds.is_file());
        assert_eq!(resolve().unwrap().key, "xdg-key");
        match prev_xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
