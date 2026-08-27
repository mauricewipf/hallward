use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::media::{is_hidden, is_media_ext};

pub const ALBUM_DIR: &str = ".album";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Collection,
    Album,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    pub relpath: PathBuf,
    pub kind: Kind,
    pub children: Vec<Folder>,
}

impl Folder {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            "."
        } else {
            &self.name
        }
    }
}

/// Walk up from `start` looking for a `.album` directory.
pub fn find_library_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(ALBUM_DIR).is_dir() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

pub fn album_paths(root: &Path) -> (PathBuf, PathBuf) {
    let album = root.join(ALBUM_DIR);
    let db = album.join("catalog.sqlite");
    (album, db)
}

pub fn has_album_dir(root: &Path) -> bool {
    root.join(ALBUM_DIR).is_dir()
}

/// Scan the library folder tree. Direct media files ⇒ album (child dirs ignored).
/// Only subfolders ⇒ collection.
pub fn scan_tree(root: &Path) -> Result<Folder> {
    scan_dir(root, PathBuf::new(), String::new()).context("scan library tree")
}

fn scan_dir(abs: &Path, relpath: PathBuf, name: String) -> Result<Folder> {
    let mut dirs = Vec::new();
    let mut has_media = false;

    let rd = match fs::read_dir(abs) {
        Ok(rd) => rd,
        Err(_) => {
            return Ok(Folder {
                name,
                relpath,
                kind: Kind::Collection,
                children: Vec::new(),
            });
        }
    };

    for entry in rd {
        let entry = entry?;
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if is_hidden(&fname) {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            dirs.push((fname.into_owned(), path));
        } else if ft.is_file() && is_media_ext(&path) {
            has_media = true;
        }
    }

    if has_media {
        return Ok(Folder {
            name,
            relpath,
            kind: Kind::Album,
            children: Vec::new(),
        });
    }

    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut children = Vec::new();
    for (child_name, child_abs) in dirs {
        let child_rel = if relpath.as_os_str().is_empty() {
            PathBuf::from(&child_name)
        } else {
            relpath.join(&child_name)
        };
        children.push(scan_dir(&child_abs, child_rel, child_name)?);
    }

    Ok(Folder {
        name,
        relpath,
        kind: Kind::Collection,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn album_vs_collection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("2025/Etyek")).unwrap();
        File::create(root.join("2025/Etyek/a.jpg")).unwrap();
        fs::create_dir_all(root.join("2026/Park")).unwrap();
        File::create(root.join("2026/Park/b.heic")).unwrap();
        File::create(root.join("2026/Park/clip.mov")).unwrap();
        fs::create_dir_all(root.join("Samples")).unwrap();
        File::create(root.join("Samples/c.png")).unwrap();

        let tree = scan_tree(root).unwrap();
        assert_eq!(tree.kind, Kind::Collection);
        assert_eq!(tree.children.len(), 3);
        let y2025 = tree.children.iter().find(|c| c.name == "2025").unwrap();
        assert_eq!(y2025.kind, Kind::Collection);
        assert_eq!(y2025.children[0].name, "Etyek");
        assert_eq!(y2025.children[0].kind, Kind::Album);
        let samples = tree.children.iter().find(|c| c.name == "Samples").unwrap();
        assert_eq!(samples.kind, Kind::Album);
        assert!(samples.children.is_empty());
    }

    #[test]
    fn video_only_folder_is_album() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("Clips")).unwrap();
        File::create(root.join("Clips/clip.mov")).unwrap();

        let tree = scan_tree(root).unwrap();
        let clips = tree.children.iter().find(|c| c.name == "Clips").unwrap();
        assert_eq!(clips.kind, Kind::Album);
        assert!(clips.children.is_empty());
    }

    #[test]
    fn find_root_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".album")).unwrap();
        fs::create_dir_all(dir.path().join("2025/Etyek")).unwrap();
        let found = find_library_root(&dir.path().join("2025/Etyek")).unwrap();
        assert_eq!(found, dir.path());
    }
}
