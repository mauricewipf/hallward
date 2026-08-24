use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    Imv,
    Nsxiv,
    Feh,
    Swayimg,
}

impl Viewer {
    pub fn bin(self) -> &'static str {
        match self {
            Viewer::Imv => "imv",
            Viewer::Nsxiv => "nsxiv",
            Viewer::Feh => "feh",
            Viewer::Swayimg => "swayimg",
        }
    }
}

pub fn detect() -> Option<Viewer> {
    for v in [Viewer::Imv, Viewer::Nsxiv, Viewer::Feh, Viewer::Swayimg] {
        if on_path(v.bin()) {
            return Some(v);
        }
    }
    None
}

fn on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Argument vector (including argv0) to open `files` starting at `start` (0-based).
/// Swayimg has no start-at flag: always starts at the first file.
pub fn argv(viewer: Viewer, files: &[PathBuf], start: usize) -> Vec<OsString> {
    let mut args = vec![OsString::from(viewer.bin())];
    if files.is_empty() {
        return args;
    }
    let start = start.min(files.len() - 1);
    match viewer {
        Viewer::Imv => {
            args.push("-n".into());
            args.push(files[start].clone().into());
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Nsxiv => {
            args.push("-n".into());
            args.push(OsString::from((start + 1).to_string()));
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Feh => {
            args.push("--start-at".into());
            args.push(files[start].clone().into());
            args.extend(files.iter().map(|p| p.clone().into()));
        }
        Viewer::Swayimg => {
            args.extend(files.iter().map(|p| p.clone().into()));
        }
    }
    args
}

pub fn open(files: &[PathBuf], start: usize) -> Result<()> {
    let Some(viewer) = detect() else {
        bail!("no image viewer found (tried imv, nsxiv, feh, swayimg)");
    };
    let args = argv(viewer, files, start);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.status()
        .with_context(|| format!("spawn {}", viewer.bin()))?;
    Ok(())
}

pub fn abs_files(root: &Path, rels: &[String]) -> Vec<PathBuf> {
    rels.iter().map(|r| root.join(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/lib/a.jpg"),
            PathBuf::from("/lib/b.jpg"),
            PathBuf::from("/lib/c.jpg"),
        ]
    }

    fn os(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn imv_starts_at_selected_path() {
        let a = os(&argv(Viewer::Imv, &files(), 1));
        assert_eq!(
            a,
            vec![
                "imv",
                "-n",
                "/lib/b.jpg",
                "/lib/a.jpg",
                "/lib/b.jpg",
                "/lib/c.jpg"
            ]
        );
    }

    #[test]
    fn nsxiv_uses_one_based_index() {
        let a = os(&argv(Viewer::Nsxiv, &files(), 2));
        assert_eq!(a[0], "nsxiv");
        assert_eq!(a[1], "-n");
        assert_eq!(a[2], "3");
    }

    #[test]
    fn feh_start_at() {
        let a = os(&argv(Viewer::Feh, &files(), 0));
        assert_eq!(a[1], "--start-at");
        assert_eq!(a[2], "/lib/a.jpg");
    }

    #[test]
    fn swayimg_starts_at_first_no_workaround() {
        let a = os(&argv(Viewer::Swayimg, &files(), 2));
        assert_eq!(a, vec!["swayimg", "/lib/a.jpg", "/lib/b.jpg", "/lib/c.jpg"]);
    }
}
