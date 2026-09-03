//! Save edited stills beside their originals and index them into the catalog.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use image::ImageFormat;

use crate::catalog;
use crate::credentials::ResolvedKey;
use crate::index;
use crate::meta;
use crate::openrouter;
use crate::thumbs;

pub use crate::openrouter::{post_openrouter, PostFn, ASK_MODEL, EDIT_MODEL};

pub const EDIT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEdit {
    pub relpath: String,
    pub filename: String,
}

pub fn edit_needs_one_photo_message() -> String {
    "Image editing needs exactly one marked photo.".into()
}

pub fn saved_message(filename: &str) -> String {
    format!("Saved {filename}")
}

pub fn unique_sibling_path(source: &Path, ext: &str) -> Result<PathBuf, String> {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "Could not choose a filename for the edited photo.".to_string())?;
    let first = parent.join(format!("{stem}-edited.{ext}"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem}-edited-{n}.{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Could not choose a free filename for the edited photo.".into())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| "Could not write the edited photo.".to_string())?;
    let tmp = parent.join(format!(".{file_name}.tmp"));
    fs::write(&tmp, bytes).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("Could not write {}: {error}", path.display())
    })?;
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("Could not save {}: {error}", path.display())
    })?;
    Ok(())
}

pub fn run_edit(
    source: &Path,
    library_root: &Path,
    instruction: &str,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    post: openrouter::PostFn,
    timeout: Duration,
) -> Result<SavedEdit, String> {
    openrouter::check_cancel(cancel)?;
    let input = thumbs::prepare_edit_input(source)
        .map_err(|error| format!("Could not prepare the photo for editing: {error:#}"))?;
    let dest = unique_sibling_path(source, "png")?;
    let edited = openrouter::run_edit_request(
        &input.mime,
        &input.bytes,
        instruction,
        key,
        cancel,
        post,
        timeout,
    )?;
    openrouter::check_cancel(cancel)?;
    let png = transcode_to_png(&edited)?;
    atomic_write(&dest, &png)?;
    index_saved_edit(source, &dest, library_root)
}

pub fn run_ask(
    jpeg_paths: &[PathBuf],
    prompt: &str,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    post: openrouter::PostFn,
    timeout: Duration,
) -> Result<String, String> {
    openrouter::run_ask(jpeg_paths, prompt, key, cancel, post, timeout)
}

fn transcode_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes)
        .map_err(|_| "OpenRouter returned an unreadable image.".to_string())?;
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|_| "OpenRouter returned an unreadable image.".to_string())?;
    Ok(out)
}

pub fn index_saved_edit(
    source: &Path,
    dest: &Path,
    library_root: &Path,
) -> Result<SavedEdit, String> {
    if !dest.is_file() {
        return Err("The edited photo was not written.".into());
    }
    let captured = meta::read_meta(source)
        .captured_at
        .or_else(|| catalog_captured_at(library_root, source));
    let photo =
        index::index_new_file(library_root, dest, captured.as_deref()).map_err(|error| {
            format!(
                "Saved {} but indexing failed: {error:#}",
                dest.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dest.display().to_string())
            )
        })?;
    Ok(SavedEdit {
        relpath: photo.relpath,
        filename: photo.filename,
    })
}

/// A still without EXIF still has a catalog date from indexing. Reuse it so the
/// sibling sorts beside its original instead of ahead of the whole album.
fn catalog_captured_at(library_root: &Path, source: &Path) -> Option<String> {
    let rel = source
        .strip_prefix(library_root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let conn = catalog::open(library_root, false).ok()?;
    catalog::captured_at(&conn, &rel).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::credentials::CredentialSource;
    use crate::library::album_paths;
    use image::{Rgb, RgbImage};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn unique_sibling_skips_existing_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"orig").unwrap();
        assert_eq!(
            unique_sibling_path(&source, "png")
                .unwrap()
                .file_name()
                .unwrap(),
            "photo-edited.png"
        );
        fs::write(dir.path().join("photo-edited.png"), b"one").unwrap();
        assert_eq!(
            unique_sibling_path(&source, "png")
                .unwrap()
                .file_name()
                .unwrap(),
            "photo-edited-2.png"
        );
    }

    #[test]
    fn atomic_write_replaces_via_temp_and_leaves_no_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("photo-edited.png");
        atomic_write(&dest, b"hello").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn cancel_before_edit_does_not_write() {
        let cancel = AtomicBool::new(true);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.jpg");
        fs::write(&source, b"orig").unwrap();
        let key = ResolvedKey::new("test", CredentialSource::File);
        let err = run_edit(
            &source,
            dir.path(),
            "remove people",
            &key,
            &cancel,
            |_url, _api_key, _body, _timeout| Ok((200, "{}".into())),
            EDIT_TIMEOUT,
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!dir.path().join("photo-edited.png").exists());
    }

    #[test]
    fn saved_sibling_is_indexed_into_the_album() {
        let dir = tempfile::tempdir().unwrap();
        let album = dir.path().join("Rome");
        fs::create_dir_all(&album).unwrap();
        catalog::open(dir.path(), true).unwrap();
        let source = album.join("photo.jpg");
        RgbImage::from_pixel(32, 24, Rgb([10, 20, 30]))
            .save(&source)
            .unwrap();
        index::index_new_file(dir.path(), &source, Some("2024:01:02 03:04:05")).unwrap();

        let dest = unique_sibling_path(&source, "png").unwrap();
        RgbImage::from_pixel(32, 24, Rgb([40, 50, 60]))
            .save(&dest)
            .unwrap();
        let saved = index_saved_edit(&source, &dest, dir.path()).unwrap();

        let conn = catalog::open(dir.path(), false).unwrap();
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 2);
        assert!(photos.iter().any(|item| item.relpath == saved.relpath));
        assert_eq!(saved.filename, "photo-edited.png");
        let (_, db) = album_paths(dir.path());
        assert!(db.exists());
    }
}
