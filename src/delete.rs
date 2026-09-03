//! Permanently unlink cataloged media, Live Photo companions, and thumbs.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

use crate::catalog::Photo;
use crate::media::{is_image, VIDEO_EXTS};
use crate::thumbs;

pub fn delete_rels(
    photos: &[Photo],
    marked: &HashSet<String>,
    grid_focused: bool,
    grid_idx: usize,
) -> Vec<String> {
    if !marked.is_empty() {
        return photos
            .iter()
            .filter(|photo| marked.contains(&photo.relpath))
            .map(|photo| photo.relpath.clone())
            .collect();
    }
    if grid_focused {
        if let Some(photo) = photos.get(grid_idx) {
            return vec![photo.relpath.clone()];
        }
    }
    Vec::new()
}

pub fn confirm_prompt(rels: &[String]) -> String {
    match rels {
        [one] => {
            let name = Path::new(one)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(one);
            format!("Delete {name} permanently?")
        }
        many => format!("Delete {} items permanently?", many.len()),
    }
}

pub const CONFIRM_HINT: &str = "y yes · n/Esc cancel";

pub fn paths_to_unlink(root: &Path, rels: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in rels {
        let abs = root.join(rel);
        out.push(abs.clone());
        out.push(thumbs::thumb_path(root, rel));
        out.extend(live_motion_paths_for_still(&abs));
    }
    out
}

pub fn unlink_media(root: &Path, rels: &[String]) -> Result<()> {
    for rel in rels {
        check_rel(rel)?;
    }
    let root = root.canonicalize()?;
    for path in paths_to_unlink(&root, rels) {
        unlink_inside(&root, &path)?;
    }
    Ok(())
}

pub(crate) fn live_motion_paths_for_still(still: &Path) -> Vec<PathBuf> {
    if !is_image(still) {
        return Vec::new();
    }
    let Some(parent) = still.parent() else {
        return Vec::new();
    };
    let Some(stem) = still.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    if stem.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for ext in VIDEO_EXTS {
        let upper = ext.to_ascii_uppercase();
        for variant in [*ext, upper.as_str()] {
            let candidate = parent.join(format!("{stem}.{variant}"));
            if !candidate.is_file() {
                continue;
            }
            let key = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            if seen.insert(key.clone()) {
                out.push(key);
            }
        }
    }
    out
}

fn check_rel(rel: &str) -> Result<()> {
    let path = Path::new(rel);
    let unsafe_rel = rel.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)));
    if unsafe_rel {
        bail!("refusing to delete {rel} (outside library)");
    }
    Ok(())
}

fn unlink_inside(root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let canonical = path.canonicalize()?;
    if canonical.strip_prefix(root).is_err() {
        bail!("refusing to delete {} (outside library)", path.display());
    }
    match fs::remove_file(&canonical) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::index;
    use image::{Rgb, RgbImage};
    use std::fs;

    fn photo(rel: &str) -> Photo {
        Photo {
            relpath: rel.into(),
            album: "album".into(),
            filename: rel.into(),
            mtime: 0,
            size: 0,
            captured_at: None,
            camera: None,
            width: None,
            height: None,
        }
    }

    fn album() -> Vec<Photo> {
        vec![photo("a.jpg"), photo("b.jpg"), photo("c.jpg")]
    }

    #[test]
    fn marked_rels_keep_album_order() {
        let marked = HashSet::from(["c.jpg".into(), "a.jpg".into()]);
        assert_eq!(
            delete_rels(&album(), &marked, true, 1),
            vec!["a.jpg", "c.jpg"]
        );
    }

    #[test]
    fn unmarked_grid_focus_deletes_the_selected_photo() {
        let marked = HashSet::new();
        assert_eq!(delete_rels(&album(), &marked, true, 2), vec!["c.jpg"]);
    }

    #[test]
    fn miller_focus_without_marks_deletes_nothing() {
        let marked = HashSet::new();
        assert!(delete_rels(&album(), &marked, false, 0).is_empty());
    }

    #[test]
    fn confirm_prompt_names_a_single_file() {
        assert_eq!(
            confirm_prompt(&["Rome/IMG_1234.HEIC".into()]),
            "Delete IMG_1234.HEIC permanently?"
        );
    }

    #[test]
    fn confirm_prompt_counts_multiple_items() {
        assert_eq!(
            confirm_prompt(&["a.jpg".into(), "b.jpg".into()]),
            "Delete 2 items permanently?"
        );
    }

    #[test]
    fn companion_paths_include_same_stem_motion() {
        let dir = tempfile::tempdir().unwrap();
        let still = dir.path().join("IMG_1.HEIC");
        fs::write(&still, b"still").unwrap();
        fs::write(dir.path().join("IMG_1.MOV"), b"motion").unwrap();
        let paths = live_motion_paths_for_still(&still);
        assert_eq!(paths.len(), 1);
        assert!(paths[0]
            .file_name()
            .unwrap()
            .eq_ignore_ascii_case("IMG_1.MOV"));
    }

    #[test]
    fn companion_paths_skip_standalone_video() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        fs::write(&video, b"video").unwrap();
        assert!(live_motion_paths_for_still(&video).is_empty());
    }

    #[test]
    fn paths_to_unlink_include_thumb_and_companion() {
        let dir = tempfile::tempdir().unwrap();
        let still = dir.path().join("Rome/IMG_1.jpg");
        fs::create_dir_all(still.parent().unwrap()).unwrap();
        fs::write(&still, b"still").unwrap();
        fs::write(dir.path().join("Rome/IMG_1.MOV"), b"motion").unwrap();
        let paths = paths_to_unlink(dir.path(), &["Rome/IMG_1.jpg".into()]);
        assert!(paths.iter().any(|p| p.ends_with("Rome/IMG_1.jpg")));
        assert!(paths.iter().any(|p| p
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("IMG_1.MOV"))));
        assert!(paths
            .iter()
            .any(|p| p.starts_with(thumbs::thumbs_dir(dir.path()))));
    }

    #[test]
    fn refuses_parent_dir_relpath() {
        let dir = tempfile::tempdir().unwrap();
        let err = unlink_media(dir.path(), &["../secret.jpg".into()]).unwrap_err();
        assert!(err.to_string().contains("outside library"));
    }

    #[test]
    fn unlink_then_reindex_drops_still_and_companion() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        let album = root.join("Rome");
        fs::create_dir_all(&album).unwrap();
        catalog::open(&root, true).unwrap();
        let still = album.join("IMG_1.jpg");
        RgbImage::from_pixel(32, 24, Rgb([10, 20, 30]))
            .save(&still)
            .unwrap();
        let motion = album.join("IMG_1.MOV");
        fs::write(&motion, b"motion").unwrap();
        index::index_library(&root).unwrap();
        {
            let conn = catalog::open(&root, false).unwrap();
            assert_eq!(catalog::count(&conn).unwrap(), 1);
        }
        let thumb = thumbs::thumb_path(&root, "Rome/IMG_1.jpg");
        assert!(thumb.is_file());

        unlink_media(&root, &["Rome/IMG_1.jpg".into()]).unwrap();
        assert!(!still.exists());
        assert!(!motion.exists());
        assert!(!thumb.exists());

        index::index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 0);
    }
}
