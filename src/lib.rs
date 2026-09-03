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
}
