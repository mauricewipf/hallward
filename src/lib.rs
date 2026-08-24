mod catalog;
mod index;
mod library;
mod media;
mod meta;
mod search;
mod thumbs;
mod tui;
mod viewer;

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "hallward", about = "Terminal photo library")]
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
            let stats = index::init_library(&start)?;
            println!(
                "initialized {} (indexed {}, skipped {}, removed {})",
                start.display(),
                stats.total,
                stats.skipped,
                stats.removed
            );
        }
        Some(Cmd::Index) => {
            let root = library::find_library_root(&start).unwrap_or(start);
            let stats = index::index_library(&root)?;
            println!(
                "indexed {} files (updated {}, skipped {}, removed {})",
                stats.total, stats.added_or_updated, stats.skipped, stats.removed
            );
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
                let stats = index::init_library(&root)?;
                eprintln!(
                    "indexed {} photos ({} updated, {} skipped)",
                    stats.total, stats.added_or_updated, stats.skipped
                );
            }
            tui::run(root)?;
        }
    }
    Ok(())
}
