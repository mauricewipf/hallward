pub mod ai;
pub mod catalog;
pub mod clipboard;
pub mod credentials;
pub mod delete;
pub mod image_edit;
pub mod index;
pub mod library;
pub mod media;
pub mod meta;
pub mod openrouter;
pub mod search;
pub mod thumbs;
pub mod tui;
pub mod viewer;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "hallward", version, about = "Terminal photo library")]
struct Cli {
    /// Library root (folder of albums). Default: cwd, or a parent that contains .album/
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create .album/ and index images
    Init,
    /// Re-scan files and refresh thumbnails
    Index,
    /// Manage OpenRouter API credentials
    #[command(visible_alias = "credential")]
    Credentials {
        #[command(subcommand)]
        cmd: CredentialsCmd,
    },
}

#[derive(Subcommand, Debug)]
enum CredentialsCmd {
    /// Write OpenRouter API key to ~/.config/hallward/credentials
    Set {
        /// OpenRouter API key. If omitted, read from stdin (echo off).
        #[arg(value_name = "KEY")]
        key: Option<String>,
        /// Copy HALLWARD_OPENROUTER_API_KEY into the credentials file
        #[arg(long)]
        from_env: bool,
    },
    /// Remove the credentials file
    Clear,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let start = cli.root.clone().unwrap_or(cwd);

    match cli.cmd {
        Some(Cmd::Init) => {
            let stats = init_with_progress(&start)?;
            println!("initialized {} ({})", start.display(), stats.summary());
        }
        Some(Cmd::Index) => {
            let root = library::find_library_root(&start).unwrap_or(start);
            let stats = index_with_progress(&root)?;
            println!("{}", stats.summary());
        }
        Some(Cmd::Credentials { cmd }) => run_credentials(cmd)?,
        None => {
            let root = if cli.root.is_some() {
                start
            } else {
                library::find_library_root(&start).unwrap_or(start)
            };
            if !library::has_album_dir(&root) {
                eprint!("Initialize Hallward library at {}? [y/N] ", root.display());
                io::stderr().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
                    anyhow::bail!("not a library (no .album/). Run `hallward init` first.");
                }
                let stats = init_with_progress(&root)?;
                eprintln!("{}", stats.summary());
            }
            tui::run(root)?;
        }
    }
    Ok(())
}

fn init_with_progress(root: &std::path::Path) -> Result<index::IndexStats> {
    let progress = index::CliProgress::new()?;
    match index::init_library_with_progress(root, &progress) {
        Ok(stats) => Ok(stats),
        Err(_) if progress.cancelled() => {
            progress.finish();
            eprintln!("Indexing cancelled");
            Err(anyhow::anyhow!("indexing cancelled"))
        }
        Err(err) => {
            progress.finish();
            Err(err)
        }
    }
}

fn run_credentials(cmd: CredentialsCmd) -> Result<()> {
    match cmd {
        CredentialsCmd::Set { key, from_env } => {
            let key = credential_value_to_save(key, from_env)?;
            credentials::set_key(&key).map_err(anyhow::Error::msg)?;
            let path = credentials::credentials_path().map_err(anyhow::Error::msg)?;
            eprintln!("Saved OpenRouter credentials to {}.", path.display());
        }
        CredentialsCmd::Clear => {
            credentials::clear().map_err(anyhow::Error::msg)?;
            eprintln!("Removed OpenRouter credentials.");
        }
    }
    Ok(())
}

fn credential_value_to_save(key: Option<String>, from_env: bool) -> Result<String> {
    if from_env && key.as_deref().is_some_and(|value| !value.trim().is_empty()) {
        anyhow::bail!("Pass the key or --from-env, not both.");
    }
    if from_env {
        return std::env::var("HALLWARD_OPENROUTER_API_KEY")
            .map_err(|_| anyhow::anyhow!("HALLWARD_OPENROUTER_API_KEY is not set."));
    }
    match key.as_deref().map(str::trim) {
        None | Some("") => credentials::read_secret_from_stdin().map_err(anyhow::Error::msg),
        Some(credentials::CREDENTIALS_KEY) => {
            credentials::read_secret_from_stdin().map_err(anyhow::Error::msg)
        }
        Some(value) => Ok(value.to_string()),
    }
}

fn index_with_progress(root: &std::path::Path) -> Result<index::IndexStats> {
    let progress = index::CliProgress::new()?;
    match index::index_library_with_progress(root, &progress) {
        Ok(stats) => Ok(stats),
        Err(_) if progress.cancelled() => {
            progress.finish();
            eprintln!("Indexing cancelled");
            Err(anyhow::anyhow!("indexing cancelled"))
        }
        Err(err) => {
            progress.finish();
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_exposes_crate_version() {
        let err = Cli::try_parse_from(["hallward", "--version"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(env!("CARGO_PKG_VERSION")),
            "expected crate version in --version output: {msg}"
        );
    }

    #[test]
    fn credentials_set_key_argument_parses() {
        let cli = Cli::try_parse_from(["hallward", "credentials", "set", "sk-test"]).unwrap();
        match cli.cmd {
            Some(Cmd::Credentials {
                cmd: CredentialsCmd::Set { key, from_env },
            }) => {
                assert_eq!(key.as_deref(), Some("sk-test"));
                assert!(!from_env);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn credential_alias_parses() {
        let cli = Cli::try_parse_from(["hallward", "credential", "set"]).unwrap();
        match cli.cmd {
            Some(Cmd::Credentials {
                cmd: CredentialsCmd::Set { key, from_env },
            }) => {
                assert!(key.is_none());
                assert!(!from_env);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn credential_value_from_argument_is_saved_verbatim() {
        let value = credential_value_to_save(Some("sk-test-key".into()), false).unwrap();
        assert_eq!(value, "sk-test-key");
    }

    #[test]
    fn credential_value_rejects_key_and_from_env_together() {
        let err = credential_value_to_save(Some("sk-test-key".into()), true).unwrap_err();
        assert!(err.to_string().contains("--from-env"), "{err}");
    }
}
