//! In-session copy/cut clipboard and paste into another album or folder.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::catalog;
use crate::delete;
use crate::library::ALBUM_DIR;
use crate::media::is_hidden;
use crate::thumbs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    Copy,
    Cut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    pub op: ClipboardOp,
    pub rels: Vec<String>,
}

impl Clipboard {
    /// Append `extra` in given order, skipping rels already on the clipboard.
    pub fn add_rels(&mut self, extra: &[String]) {
        for rel in extra {
            if !self.rels.iter().any(|existing| existing == rel) {
                self.rels.push(rel.clone());
            }
        }
    }

    fn toggle_rels(&mut self, extra: &[String]) {
        for rel in extra {
            if let Some(index) = self.rels.iter().position(|existing| existing == rel) {
                self.rels.remove(index);
            } else {
                self.rels.push(rel.clone());
            }
        }
    }

    /// `c` / `x`: start a clipboard, add to one, or toggle off a photo already in that state.
    /// Same-key on a copied/cut photo removes it. Switching `c`↔`x` changes the mode and adds.
    pub fn from_key(existing: Option<Self>, extra: Vec<String>, op: ClipboardOp) -> Option<Self> {
        if extra.is_empty() {
            return existing;
        }
        match existing {
            None => Some(Self { op, rels: extra }),
            Some(mut clip) if clip.op == op => {
                clip.toggle_rels(&extra);
                if clip.rels.is_empty() {
                    None
                } else {
                    Some(clip)
                }
            }
            Some(mut clip) => {
                clip.op = op;
                clip.add_rels(&extra);
                Some(clip)
            }
        }
    }

    /// `p`: fold currently marked items into the clipboard before pasting.
    pub fn absorbing_marks(mut self, marked: &[String]) -> Self {
        self.add_rels(marked);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedItem {
    pub src: String,
    pub dest: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteResult {
    pub items: Vec<PastedItem>,
    pub skipped: usize,
    pub same_album_cut: bool,
}

impl PasteResult {
    /// Destination relpaths in paste order (compat helper for focus/UI).
    pub fn pasted(&self) -> Vec<String> {
        self.items.iter().map(|item| item.dest.clone()).collect()
    }
}

pub fn copied_message(n: usize) -> String {
    format!("copied {} · p paste", count_items(n))
}

pub fn cut_message(n: usize) -> String {
    format!("cut {} · p paste", count_items(n))
}

fn count_items(n: usize) -> String {
    if n == 1 {
        "1 item".into()
    } else {
        format!("{n} items")
    }
}

/// If `filename` is free in `dir`, use it; otherwise `{stem}-2.ext`, `{stem}-3.ext`, …
pub fn unique_dest(dir: &Path, filename: &str) -> Result<PathBuf> {
    let (dest, _) = plan_dest(dir, filename, &[])?;
    Ok(dest)
}

pub fn paste(root: &Path, clipboard: &Clipboard, dest_album: &str) -> Result<PasteResult> {
    paste_into(root, clipboard, Some(dest_album), dest_album)
}

pub fn paste_into(
    root: &Path,
    clipboard: &Clipboard,
    dest_files: Option<&str>,
    dest_folders: &str,
) -> Result<PasteResult> {
    for rel in &clipboard.rels {
        check_rel(rel)?;
    }
    if let Some(dest) = dest_files {
        check_album(dest)?;
    }
    check_album(dest_folders)?;
    if clipboard.op == ClipboardOp::Cut
        && same_location_cut(root, clipboard, dest_files, dest_folders)
    {
        return Ok(PasteResult {
            items: Vec::new(),
            skipped: 0,
            same_album_cut: true,
        });
    }

    let root = root.canonicalize()?;
    if let Some(dest) = dest_files {
        fs::create_dir_all(dest_album_dir(&root, dest))
            .with_context(|| format!("create album {}", dest))?;
    }
    fs::create_dir_all(dest_album_dir(&root, dest_folders))
        .with_context(|| format!("create folder {}", dest_folders))?;

    let mut items = Vec::new();
    let mut skipped = 0;
    let cut = clipboard.op == ClipboardOp::Cut;
    for rel in &clipboard.rels {
        let src = root.join(rel);
        if src.is_dir() {
            let dest = paste_folder(&root, rel, &src, dest_folders, cut)?;
            items.push(PastedItem {
                src: rel.clone(),
                dest,
                is_dir: true,
            });
        } else if src.is_file() {
            let Some(dest_album) = dest_files else {
                skipped += 1;
                continue;
            };
            match paste_file(&root, rel, &src, dest_album, cut)? {
                Some(dest_rel) => items.push(PastedItem {
                    src: rel.clone(),
                    dest: dest_rel,
                    is_dir: false,
                }),
                None => skipped += 1,
            }
        } else {
            skipped += 1;
        }
    }
    Ok(PasteResult {
        items,
        skipped,
        same_album_cut: false,
    })
}

fn same_location_cut(
    root: &Path,
    clipboard: &Clipboard,
    dest_files: Option<&str>,
    dest_folders: &str,
) -> bool {
    clipboard.rels.iter().all(|rel| {
        let src = root.join(rel);
        if src.is_dir() {
            catalog::album_relpath(rel) == dest_folders
        } else if src.is_file() {
            dest_files.is_some_and(|dest| catalog::album_relpath(rel) == dest)
        } else {
            true
        }
    })
}

fn dest_is_inside_src(src_rel: &str, dest_parent: &str) -> bool {
    dest_parent == src_rel || dest_parent.starts_with(&format!("{src_rel}/"))
}

fn paste_file(
    root: &Path,
    rel: &str,
    src: &Path,
    dest_album: &str,
    cut: bool,
) -> Result<Option<String>> {
    let dest_dir = dest_album_dir(root, dest_album);
    let filename = src
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("could not paste {rel}"))?;
    let companions = delete::live_motion_paths_for_still(src);
    let mut companions = companions;
    companions.extend(delete::raw_dng_paths_for_still(src));
    let (dest, companion_dests) = plan_dest(&dest_dir, filename, &companions)?;
    ensure_inside(root, &dest)?;
    transfer_file(src, &dest, cut)?;
    for (companion, companion_dest) in companions.iter().zip(&companion_dests) {
        ensure_inside(root, companion_dest)?;
        if companion.is_file() {
            transfer_file(companion, companion_dest, cut)?;
        }
    }
    if cut {
        let thumb = thumbs::thumb_path(root, rel);
        if thumb.is_file() {
            let _ = fs::remove_file(&thumb);
        }
    }
    Ok(Some(relpath_in(root, &dest)?))
}

fn paste_folder(
    root: &Path,
    rel: &str,
    src: &Path,
    dest_parent: &str,
    cut: bool,
) -> Result<String> {
    if dest_is_inside_src(rel, dest_parent) {
        bail!("cannot paste {rel} into itself");
    }
    let dest_dir = dest_album_dir(root, dest_parent);
    let name = src
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("could not paste {rel}"))?;
    let dest = unique_dir_dest(&dest_dir, name)?;
    ensure_inside(root, &dest)?;
    if cut {
        delete::remove_thumbs_under(root, src)?;
    }
    transfer_dir(src, &dest, cut)?;
    relpath_in(root, &dest)
}

fn unique_dir_dest(parent: &Path, name: &str) -> Result<PathBuf> {
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            name.to_string()
        } else if n == 1 {
            continue;
        } else {
            format!("{name}-{n}")
        };
        let dest = parent.join(&candidate);
        if !dest.exists() {
            return Ok(dest);
        }
    }
    bail!("could not choose a free folder name for {name}");
}

fn transfer_dir(src: &Path, dest: &Path, cut: bool) -> Result<()> {
    if dest.exists() {
        bail!("destination already exists: {}", dest.display());
    }
    if cut && fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    copy_dir_recursive(src, dest)?;
    if cut {
        fs::remove_dir_all(src).with_context(|| format!("remove {}", src.display()))?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_hidden(&name) || name == ALBUM_DIR {
            continue;
        }
        let from = entry.path();
        let to = dest.join(name.as_ref());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn dest_album_dir(root: &Path, album: &str) -> PathBuf {
    if album == "." {
        root.to_path_buf()
    } else {
        root.join(album)
    }
}

fn plan_dest(
    dir: &Path,
    filename: &str,
    companions: &[PathBuf],
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("could not choose a filename for {filename}"))?;
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 0..10_000u32 {
        let new_stem = if n == 0 {
            stem.to_string()
        } else if n == 1 {
            continue;
        } else {
            format!("{stem}-{n}")
        };
        let dest = with_stem_ext(dir, &new_stem, ext);
        let companion_dests: Vec<PathBuf> = companions
            .iter()
            .map(|companion| {
                let cext = companion.extension().and_then(|e| e.to_str());
                with_stem_ext(dir, &new_stem, cext)
            })
            .collect();
        if !dest.exists() && companion_dests.iter().all(|p| !p.exists()) {
            return Ok((dest, companion_dests));
        }
    }
    bail!("could not choose a free filename for {filename}");
}

fn with_stem_ext(dir: &Path, stem: &str, ext: Option<&str>) -> PathBuf {
    match ext {
        Some(ext) => dir.join(format!("{stem}.{ext}")),
        None => dir.join(stem),
    }
}

fn transfer_file(src: &Path, dest: &Path, cut: bool) -> Result<()> {
    if dest.exists() {
        bail!("destination already exists: {}", dest.display());
    }
    if !cut {
        fs::copy(src, dest)
            .with_context(|| format!("copy {} → {}", src.display(), dest.display()))?;
        return Ok(());
    }
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    fs::copy(src, dest).with_context(|| format!("move {} → {}", src.display(), dest.display()))?;
    fs::remove_file(src).with_context(|| format!("remove {}", src.display()))?;
    Ok(())
}

fn relpath_in(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside the library", path.display()))?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn ensure_inside(root: &Path, path: &Path) -> Result<()> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut norm = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(p) => norm.push(p.as_os_str()),
            Component::RootDir => norm.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                norm.pop();
            }
            Component::Normal(s) => norm.push(s),
        }
    }
    if norm.strip_prefix(root).is_err() {
        bail!("refusing to paste {} (outside library)", path.display());
    }
    Ok(())
}

fn check_rel(rel: &str) -> Result<()> {
    let path = Path::new(rel);
    let unsafe_rel = rel.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)));
    if unsafe_rel {
        bail!("refusing to paste {rel} (outside library)");
    }
    Ok(())
}

fn check_album(album: &str) -> Result<()> {
    if album == "." {
        return Ok(());
    }
    check_rel(album)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::index;
    use image::{Rgb, RgbImage};
    use std::fs;

    fn mini_library() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        fs::create_dir_all(root.join("Rome")).unwrap();
        fs::create_dir_all(root.join("Paris")).unwrap();
        catalog::open(&root, true).unwrap();
        (tmp, root)
    }

    fn write_still(path: &Path) {
        RgbImage::from_pixel(32, 24, Rgb([10, 20, 30]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn add_rels_appends_new_and_skips_duplicates() {
        let mut clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into(), "Rome/b.jpg".into()],
        };
        clip.add_rels(&["Rome/b.jpg".into(), "Paris/c.jpg".into()]);
        assert_eq!(clip.rels, vec!["Rome/a.jpg", "Rome/b.jpg", "Paris/c.jpg"]);
        assert_eq!(clip.op, ClipboardOp::Copy);
    }

    #[test]
    fn c_starts_a_copy_clipboard() {
        let clip = Clipboard::from_key(None, vec!["Rome/a.jpg".into()], ClipboardOp::Copy).unwrap();
        assert_eq!(clip.op, ClipboardOp::Copy);
        assert_eq!(clip.rels, vec!["Rome/a.jpg"]);
    }

    #[test]
    fn c_adds_marked_photos_to_an_existing_copy_clipboard() {
        let existing = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip =
            Clipboard::from_key(Some(existing), vec!["Rome/b.jpg".into()], ClipboardOp::Copy)
                .unwrap();
        assert_eq!(clip.op, ClipboardOp::Copy);
        assert_eq!(clip.rels, vec!["Rome/a.jpg", "Rome/b.jpg"]);
    }

    #[test]
    fn x_un_cuts_a_cut_photo() {
        let existing = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into(), "Rome/b.jpg".into()],
        };
        let clip = Clipboard::from_key(Some(existing), vec!["Rome/a.jpg".into()], ClipboardOp::Cut)
            .unwrap();
        assert_eq!(clip.op, ClipboardOp::Cut);
        assert_eq!(clip.rels, vec!["Rome/b.jpg"]);
    }

    #[test]
    fn x_toggles_each_marked_photo() {
        let existing = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip = Clipboard::from_key(
            Some(existing),
            vec!["Rome/a.jpg".into(), "Rome/c.jpg".into()],
            ClipboardOp::Cut,
        )
        .unwrap();
        assert_eq!(clip.rels, vec!["Rome/c.jpg"]);
    }

    #[test]
    fn c_un_copies_a_copied_photo() {
        let existing = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into()],
        };
        assert!(
            Clipboard::from_key(Some(existing), vec!["Rome/a.jpg".into()], ClipboardOp::Copy)
                .is_none()
        );
    }

    #[test]
    fn x_on_the_last_cut_photo_clears_the_clipboard() {
        let existing = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        assert!(
            Clipboard::from_key(Some(existing), vec!["Rome/a.jpg".into()], ClipboardOp::Cut)
                .is_none()
        );
    }

    #[test]
    fn c_on_a_cut_clipboard_switches_to_copy_and_adds() {
        let existing = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip =
            Clipboard::from_key(Some(existing), vec!["Rome/b.jpg".into()], ClipboardOp::Copy)
                .unwrap();
        assert_eq!(clip.op, ClipboardOp::Copy);
        assert_eq!(clip.rels, vec!["Rome/a.jpg", "Rome/b.jpg"]);
    }

    #[test]
    fn p_adds_marked_photos_before_paste() {
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip = clip.absorbing_marks(&["Rome/b.jpg".into(), "Rome/a.jpg".into()]);
        assert_eq!(clip.op, ClipboardOp::Copy);
        assert_eq!(clip.rels, vec!["Rome/a.jpg", "Rome/b.jpg"]);
    }

    #[test]
    fn c_without_a_new_selection_keeps_the_existing_clipboard() {
        let existing = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip = Clipboard::from_key(Some(existing.clone()), vec![], ClipboardOp::Copy).unwrap();
        assert_eq!(clip, existing);
        assert!(Clipboard::from_key(None, vec![], ClipboardOp::Copy).is_none());
    }

    #[test]
    fn p_without_marks_leaves_the_clipboard_unchanged() {
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let clip = clip.absorbing_marks(&[]);
        assert_eq!(clip.rels, vec!["Rome/a.jpg"]);
        assert_eq!(clip.op, ClipboardOp::Cut);
    }

    #[test]
    fn unique_dest_keeps_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            unique_dest(dir.path(), "photo.jpg")
                .unwrap()
                .file_name()
                .unwrap(),
            "photo.jpg"
        );
    }

    #[test]
    fn unique_dest_skips_to_dash_two_then_three() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("photo.jpg"), b"a").unwrap();
        assert_eq!(
            unique_dest(dir.path(), "photo.jpg")
                .unwrap()
                .file_name()
                .unwrap(),
            "photo-2.jpg"
        );
        fs::write(dir.path().join("photo-2.jpg"), b"b").unwrap();
        assert_eq!(
            unique_dest(dir.path(), "photo.jpg")
                .unwrap()
                .file_name()
                .unwrap(),
            "photo-3.jpg"
        );
    }

    #[test]
    fn companion_dests_share_the_new_stem() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("IMG_1.HEIC"), b"still").unwrap();
        fs::write(dir.path().join("IMG_1.MOV"), b"motion").unwrap();
        let companions = vec![dir.path().join("IMG_1.MOV")];
        let (dest, motion) = plan_dest(dir.path(), "IMG_1.HEIC", &companions).unwrap();
        assert_eq!(dest.file_name().unwrap(), "IMG_1-2.HEIC");
        assert_eq!(motion[0].file_name().unwrap(), "IMG_1-2.MOV");
    }

    #[test]
    fn same_album_cut_is_a_noop() {
        let (_tmp, root) = mini_library();
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let result = paste(&root, &clip, "Rome").unwrap();
        assert!(result.same_album_cut);
        assert!(result.pasted().is_empty());
    }

    #[test]
    fn copy_into_occupied_name_yields_dash_two() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/photo.jpg"));
        write_still(&root.join("Paris/photo.jpg"));
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/photo.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        assert_eq!(result.pasted(), vec!["Paris/photo-2.jpg"]);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].src, "Rome/photo.jpg");
        assert_eq!(result.items[0].dest, "Paris/photo-2.jpg");
        assert!(!result.items[0].is_dir);
        assert!(root.join("Rome/photo.jpg").is_file());
        assert!(root.join("Paris/photo.jpg").is_file());
        assert!(root.join("Paris/photo-2.jpg").is_file());
    }

    #[test]
    fn copy_keeps_still_and_dng_twin_in_both_albums() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/DSC_1.jpg"));
        fs::write(root.join("Rome/DSC_1.DNG"), b"dng").unwrap();
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/DSC_1.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        assert_eq!(result.pasted(), vec!["Paris/DSC_1.jpg"]);
        assert!(root.join("Rome/DSC_1.jpg").is_file());
        assert!(root.join("Rome/DSC_1.DNG").is_file());
        assert!(root.join("Paris/DSC_1.jpg").is_file());
        assert!(root.join("Paris/DSC_1.DNG").is_file());
        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
        let paris = catalog::photos_in_album(&conn, "Paris").unwrap();
        assert_eq!(paris.len(), 1);
        assert_eq!(paris[0].raw_relpath.as_deref(), Some("Paris/DSC_1.DNG"));
    }

    #[test]
    fn copy_keeps_still_and_companion_in_both_albums() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/IMG_1.jpg"));
        fs::write(root.join("Rome/IMG_1.MOV"), b"motion").unwrap();
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/IMG_1.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        assert_eq!(result.pasted(), vec!["Paris/IMG_1.jpg"]);
        assert_eq!(result.items[0].src, "Rome/IMG_1.jpg");
        assert!(!result.items[0].is_dir);
        assert!(root.join("Rome/IMG_1.jpg").is_file());
        assert!(root.join("Rome/IMG_1.MOV").is_file());
        assert!(root.join("Paris/IMG_1.jpg").is_file());
        assert!(root.join("Paris/IMG_1.MOV").is_file());
        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
    }

    #[test]
    fn paste_result_tracks_src_dest_mapping() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        write_still(&root.join("Rome/b.jpg"));
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into(), "Rome".into(), "missing.jpg".into()],
        };
        let result = paste_into(&root, &clip, Some("Paris"), "Paris").unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(
            result.items[0],
            PastedItem {
                src: "Rome/a.jpg".into(),
                dest: "Paris/a.jpg".into(),
                is_dir: false,
            }
        );
        assert_eq!(result.items[1].src, "Rome");
        assert!(result.items[1].is_dir);
        assert_eq!(result.items[1].dest, "Paris/Rome");
        assert_eq!(result.skipped, 1);
        assert_eq!(result.pasted(), vec!["Paris/a.jpg", "Paris/Rome"]);
    }

    #[test]
    fn cut_moves_still_and_companion_then_reindex() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/IMG_1.jpg"));
        fs::write(root.join("Rome/IMG_1.MOV"), b"motion").unwrap();
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/IMG_1.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        assert_eq!(result.pasted(), vec!["Paris/IMG_1.jpg"]);
        assert_eq!(result.items[0].src, "Rome/IMG_1.jpg");
        assert!(!root.join("Rome/IMG_1.jpg").exists());
        assert!(!root.join("Rome/IMG_1.MOV").exists());
        assert!(root.join("Paris/IMG_1.jpg").is_file());
        assert!(root.join("Paris/IMG_1.MOV").is_file());
        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let paris = catalog::photos_in_album(&conn, "Paris").unwrap();
        assert_eq!(paris.len(), 1);
        assert_eq!(paris[0].relpath, "Paris/IMG_1.jpg");
    }

    #[test]
    fn copy_folder_into_another_collection() {
        let (_tmp, root) = mini_library();
        fs::create_dir_all(root.join("2024")).unwrap();
        write_still(&root.join("Rome/photo.jpg"));
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome".into()],
        };
        let result = paste_into(&root, &clip, None, "2024").unwrap();
        assert_eq!(result.pasted(), vec!["2024/Rome"]);
        assert!(root.join("Rome/photo.jpg").is_file());
        assert!(root.join("2024/Rome/photo.jpg").is_file());
        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
    }

    #[test]
    fn copy_folder_into_occupied_name_yields_dash_two() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        write_still(&root.join("Paris/b.jpg"));
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome".into()],
        };
        let result = paste_into(&root, &clip, None, ".").unwrap();
        assert_eq!(result.pasted(), vec!["Rome-2"]);
        assert!(root.join("Rome/a.jpg").is_file());
        assert!(root.join("Rome-2/a.jpg").is_file());
    }

    #[test]
    fn cut_folder_moves_nested_media_then_reindex() {
        let (_tmp, root) = mini_library();
        fs::create_dir_all(root.join("2024")).unwrap();
        write_still(&root.join("Rome/IMG_1.jpg"));
        fs::write(root.join("Rome/IMG_1.MOV"), b"motion").unwrap();
        index::index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome".into()],
        };
        let result = paste_into(&root, &clip, None, "2024").unwrap();
        assert_eq!(result.pasted(), vec!["2024/Rome"]);
        assert!(!root.join("Rome").exists());
        assert!(root.join("2024/Rome/IMG_1.jpg").is_file());
        assert!(root.join("2024/Rome/IMG_1.MOV").is_file());
        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let photos = catalog::photos_in_album(&conn, "2024/Rome").unwrap();
        assert_eq!(photos[0].relpath, "2024/Rome/IMG_1.jpg");
    }

    #[test]
    fn same_parent_folder_cut_is_a_noop() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome".into()],
        };
        let result = paste_into(&root, &clip, None, ".").unwrap();
        assert!(result.same_album_cut);
        assert!(root.join("Rome/a.jpg").is_file());
    }

    #[test]
    fn refuses_pasting_a_folder_into_itself() {
        let (_tmp, root) = mini_library();
        fs::create_dir_all(root.join("2024/Rome")).unwrap();
        write_still(&root.join("2024/Rome/a.jpg"));
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["2024".into()],
        };
        let err = paste_into(&root, &clip, None, "2024").unwrap_err();
        assert!(err.to_string().contains("into itself"));
        let err = paste_into(&root, &clip, None, "2024/Rome").unwrap_err();
        assert!(err.to_string().contains("into itself"));
    }

    #[test]
    fn dest_inside_src_detects_self_and_descendants() {
        assert!(dest_is_inside_src("2024", "2024"));
        assert!(dest_is_inside_src("2024", "2024/Rome"));
        assert!(!dest_is_inside_src("2024", "."));
        assert!(!dest_is_inside_src("2024", "2025"));
        assert!(!dest_is_inside_src("Rome", "2024"));
    }
}
