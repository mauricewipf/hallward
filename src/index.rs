use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::catalog::{self, Photo};
use crate::clipboard::{ClipboardOp, PastedItem};
use crate::library::{album_paths, ALBUM_DIR};
use crate::media::{self, is_hidden, is_media_ext, is_video};
use crate::meta;
use crate::thumbs;

#[derive(Debug, Default)]
pub struct IndexStats {
    pub added_or_updated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub removed: usize,
    pub total: i64,
}

impl IndexStats {
    pub fn summary(&self) -> String {
        format!(
            "indexed {} files (updated {}, skipped {}, failed {}, removed {})",
            self.total, self.added_or_updated, self.skipped, self.failed, self.removed
        )
    }
}

/// CLI-only progress display and cancellation state. The signal handler only
/// sets a flag, leaving catalog writes and terminal cleanup to the indexer.
pub struct CliProgress {
    cancelled: Arc<AtomicBool>,
    scanning: ProgressBar,
    thumbnails: Mutex<Option<ProgressBar>>,
}

impl CliProgress {
    pub fn new() -> Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal_cancelled = Arc::clone(&cancelled);
        ctrlc::set_handler(move || signal_cancelled.store(true, Ordering::SeqCst))?;

        let scanning = ProgressBar::new_spinner();
        scanning.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}  [{elapsed_precise}]")?
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        scanning.set_message("Scanning library… 0 media files examined");
        scanning.enable_steady_tick(Duration::from_millis(100));
        Ok(Self {
            cancelled,
            scanning,
            thumbnails: Mutex::new(None),
        })
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            bail!("indexing cancelled")
        }
        Ok(())
    }

    fn scanned(&self) {
        self.scanning.inc(1);
        self.scanning.set_message(format!(
            "Scanning library… {} media files examined",
            self.scanning.position()
        ));
    }

    fn start_thumbnails(&self, total: u64) -> ProgressBar {
        self.scanning.finish_and_clear();
        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template(
                "{msg}  {bar:30.cyan/blue} {pos}/{len} {percent}%  [{elapsed_precise}<{eta_precise}]",
            )
            .expect("valid progress template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        bar.set_message("Generating thumbnails");
        *self.thumbnails.lock().expect("progress lock poisoned") = Some(bar.clone());
        bar
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn finish(&self) {
        self.scanning.finish_and_clear();
        if let Some(bar) = self
            .thumbnails
            .lock()
            .expect("progress lock poisoned")
            .take()
        {
            bar.finish_and_clear();
        }
    }
}

pub fn init_library_with_progress(root: &Path, progress: &CliProgress) -> Result<IndexStats> {
    let (album, _) = album_paths(root);
    fs::create_dir_all(&album)?;
    fs::create_dir_all(album.join("thumbs"))?;
    index_library_with_progress(root, progress)
}

pub fn index_library(root: &Path) -> Result<IndexStats> {
    index_library_inner(root, None)
}

pub fn index_library_with_progress(root: &Path, progress: &CliProgress) -> Result<IndexStats> {
    index_library_inner(root, Some(progress))
}

fn index_library_inner(root: &Path, progress: Option<&CliProgress>) -> Result<IndexStats> {
    let mut conn = catalog::open(root, true)?;
    let mut stats = IndexStats::default();
    let mut seen = Vec::new();
    let mut dirty: Vec<Photo> = Vec::new();
    let have_ffmpeg = media::bin_on_path("ffmpeg");
    let have_ffprobe = media::bin_on_path("ffprobe");
    let mut warned_ffprobe = false;
    // One query up front: the scan loop below must not pay a point-SELECT
    // per file (N+1 reads dominate warm runs on large/slow mounts).
    let existing = catalog::all_index_state(&conn)?;

    for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        if e.file_type().is_dir() {
            name != ALBUM_DIR && !is_hidden(&name)
        } else {
            !is_hidden(&name)
        }
    }) {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }
        let entry = entry?;
        if !entry.file_type().is_file() || !is_media_ext(entry.path()) {
            continue;
        }
        if let Some(progress) = progress {
            progress.scanned();
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let meta_fs = entry.metadata()?;
        let mtime = mtime_epoch(&meta_fs)?;
        let size = meta_fs.len() as i64;
        let raw_relpath = raw_relpath_for_still(root, path);
        if let Some(&(old_m, old_s, ref old_raw)) = existing.get(&rel) {
            if old_m == mtime
                && old_s == size
                && old_raw == &raw_relpath
                && meta_fs
                    .modified()
                    .map(|src_m| thumbs::is_current_with_src_mtime(root, &rel, src_m))
                    .unwrap_or(false)
            {
                seen.push(rel);
                stats.skipped += 1;
                continue;
            }
        }
        if is_video(path) {
            if !have_ffprobe && !warned_ffprobe {
                eprintln!("hallward: ffprobe not found; Live Photo detection falls back to filename pairing (install ffmpeg)");
                warned_ffprobe = true;
            }
            if media::is_live_photo_companion(path, have_ffprobe) {
                continue;
            }
        }
        if media::is_dng_raw_companion(path) {
            continue;
        }
        seen.push(rel.clone());
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let album = catalog::album_relpath(&rel);
        let photo_meta = if is_video(path) {
            meta::video_meta_from_mtime(mtime)
        } else {
            meta::read_meta(path)
        };
        dirty.push(catalog::photo_from_file(
            rel, filename, album, mtime, size, photo_meta, raw_relpath,
        ));
    }

    if !have_ffmpeg && dirty.iter().any(|p| is_video(&root.join(&p.relpath))) {
        eprintln!(
            "hallward: ffmpeg not found; video thumbnails skipped (install ffmpeg to enable)"
        );
    }
    let thumbnail_progress = progress.map(|p| p.start_thumbnails(dirty.len() as u64));
    let results: Vec<(Photo, Option<String>)> = dirty
        .into_par_iter()
        .map(|photo| -> Result<_> {
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }
            let abs = root.join(&photo.relpath);
            let result = if is_video(&abs) && !have_ffmpeg {
                (photo, None)
            } else {
                // Freshness was verified in the scan loop above; skip the
                // re-stat so dirty files cost one pass over the bytes.
                match thumbs::generate_thumb_force(root, &abs, &photo.relpath) {
                    Ok(_) => (photo, None),
                    Err(e) => (photo, Some(format!("{e:#}"))),
                }
            };
            if let Some(bar) = &thumbnail_progress {
                bar.inc(1);
            }
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }
            Ok(result)
        })
        .collect::<Result<_>>()?;

    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }
    let photos: Vec<Photo> = results.iter().map(|(photo, _)| photo.clone()).collect();
    stats.removed = catalog::apply_index_changes(&mut conn, &seen, &photos)?;
    for (photo, err) in results {
        if let Some(e) = err {
            eprintln!("hallward: thumbnail failed for {}: {e}", photo.relpath);
            stats.failed += 1;
        }
        stats.added_or_updated += 1;
    }
    stats.total = catalog::count(&conn)?;
    if let Some(progress) = progress {
        progress.finish();
    }
    Ok(stats)
}

/// Index a single new still without pruning the rest of the catalog.
///
/// The catalog row is stored even when thumbnail generation fails so the
/// photo stays visible in the gallery; the thumbnail error is still returned.
pub fn index_new_file(
    root: &Path,
    abs: &Path,
    captured_at_fallback: Option<&str>,
) -> Result<Photo> {
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("{} is outside the library", abs.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let photo = build_photo(root, abs, &rel, captured_at_fallback)?;
    let conn = catalog::open(root, true)?;
    catalog::upsert_photo(&conn, &photo)?;
    let have_ffmpeg = media::bin_on_path("ffmpeg");
    if is_video(abs) && !have_ffmpeg {
        return Ok(photo);
    }
    if let Err(e) = thumbs::generate_thumb(root, abs, &rel) {
        anyhow::bail!("thumbnail failed for {rel}: {e:#}");
    }
    Ok(photo)
}

/// Incrementally index only what a paste created, plus prune moved sources.
///
/// Mirrors `index_library_inner` semantics for the touched paths: same
/// hidden/`.album`/media-ext/live-companion filters, same row fields from the
/// destination file, video cataloged even without `ffmpeg`, and the row is
/// kept even when its thumbnail fails (`failed` counts it).
pub fn index_pasted(
    root: &Path,
    items: &[PastedItem],
    clip_rels: &[String],
    op: ClipboardOp,
) -> Result<IndexStats> {
    let conn = catalog::open(root, true)?;
    let mut stats = IndexStats::default();
    let have_ffmpeg = media::bin_on_path("ffmpeg");
    let have_ffprobe = media::bin_on_path("ffprobe");
    let is_cut = op == ClipboardOp::Cut;

    let mut dest_rels: Vec<String> = Vec::new();
    for item in items {
        let dest_abs = root.join(&item.dest);
        if item.is_dir {
            if !dest_abs.is_dir() {
                continue;
            }
            let mut nested: Vec<std::path::PathBuf> = Vec::new();
            for entry in WalkDir::new(&dest_abs).into_iter().filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                if e.file_type().is_dir() {
                    name != ALBUM_DIR && !is_hidden(&name)
                } else {
                    !is_hidden(&name)
                }
            }) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        eprintln!("hallward: thumbnail failed: {e:#}");
                        stats.failed += 1;
                        continue;
                    }
                };
                if !entry.file_type().is_file() || !is_media_ext(entry.path()) {
                    continue;
                }
                if is_video(entry.path())
                    && media::is_live_photo_companion(entry.path(), have_ffprobe)
                {
                    continue;
                }
                if media::is_dng_raw_companion(entry.path()) {
                    continue;
                }
                nested.push(entry.path().to_path_buf());
            }
            nested.sort();
            for abs in nested {
                let rel = match abs.strip_prefix(root) {
                    Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                    Err(_) => {
                        stats.failed += 1;
                        continue;
                    }
                };
                match build_photo(root, &abs, &rel, None) {
                    Ok(photo) => {
                        if let Err(e) = catalog::upsert_photo(&conn, &photo) {
                            eprintln!("hallward: index failed for {rel}: {e:#}");
                            stats.failed += 1;
                            continue;
                        }
                        stats.added_or_updated += 1;
                        if !is_video(&abs) || have_ffmpeg {
                            if let Err(e) = thumbs::generate_thumb(root, &abs, &rel) {
                                eprintln!("hallward: thumbnail failed for {rel}: {e:#}");
                                stats.failed += 1;
                            }
                        }
                        dest_rels.push(rel);
                    }
                    Err(e) => {
                        eprintln!("hallward: index failed: {e:#}");
                        stats.failed += 1;
                    }
                }
            }
        } else {
            if !dest_abs.is_file() {
                stats.failed += 1;
                continue;
            }
            if is_video(&dest_abs) && media::is_live_photo_companion(&dest_abs, have_ffprobe) {
                continue;
            }
            if media::is_dng_raw_companion(&dest_abs) {
                continue;
            }
            match build_photo(root, &dest_abs, &item.dest, None) {
                Ok(photo) => {
                    if let Err(e) = catalog::upsert_photo(&conn, &photo) {
                        eprintln!("hallward: index failed for {}: {e:#}", item.dest);
                        stats.failed += 1;
                        continue;
                    }
                    stats.added_or_updated += 1;
                    if !is_video(&dest_abs) || have_ffmpeg {
                        if let Err(e) = thumbs::generate_thumb(root, &dest_abs, &item.dest) {
                            eprintln!("hallward: thumbnail failed for {}: {e:#}", item.dest);
                            stats.failed += 1;
                        }
                    }
                    dest_rels.push(item.dest.clone());
                }
                Err(e) => {
                    eprintln!("hallward: index failed for {}: {e:#}", item.dest);
                    stats.failed += 1;
                }
            }
        }
    }

    if is_cut {
        use std::collections::HashSet;
        let moved_srcs: HashSet<&str> = items.iter().map(|i| i.src.as_str()).collect();
        for item in items {
            let removed = if item.is_dir {
                catalog::delete_under_prefix(&conn, &item.src)?
            } else {
                catalog::delete_photo(&conn, &item.src)?
            };
            stats.removed += removed;
        }
        // Sources that vanished without a mapping (stale rows the full
        // indexer would prune via its keep-list) are ghosts: drop them.
        // Sources that still exist on disk (e.g. files skipped because
        // dest_files was None) must be kept.
        for src in clip_rels {
            if moved_srcs.contains(src.as_str()) {
                continue;
            }
            let abs = root.join(src);
            if !abs.exists() {
                if abs.is_dir() || src.ends_with('/') {
                    stats.removed += catalog::delete_under_prefix(&conn, src)?;
                } else {
                    stats.removed += catalog::delete_photo(&conn, src)?;
                }
            }
        }
    }

    if !have_ffmpeg && dest_rels.iter().any(|rel| is_video(&root.join(rel))) {
        eprintln!(
            "hallward: ffmpeg not found; video thumbnails skipped (install ffmpeg to enable)"
        );
    }
    stats.total = catalog::count(&conn)?;
    Ok(stats)
}

fn build_photo(
    root: &Path,
    abs: &Path,
    rel: &str,
    captured_at_fallback: Option<&str>,
) -> Result<Photo> {
    let meta_fs = fs::metadata(abs)?;
    let mtime = mtime_epoch(&meta_fs)?;
    let size = meta_fs.len() as i64;
    let filename = abs
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let album = catalog::album_relpath(rel);
    let mut photo_meta = if is_video(abs) {
        meta::video_meta_from_mtime(mtime)
    } else {
        meta::read_meta(abs)
    };
    if photo_meta.captured_at.is_none() {
        photo_meta.captured_at = captured_at_fallback.map(str::to_string);
    }
    if photo_meta.width.is_none() || photo_meta.height.is_none() {
        if let Ok((width, height)) = image::image_dimensions(abs) {
            photo_meta.width = photo_meta.width.or(Some(width));
            photo_meta.height = photo_meta.height.or(Some(height));
        }
    }
    Ok(catalog::photo_from_file(
        rel.to_string(),
        filename,
        album,
        mtime,
        size,
        photo_meta,
        raw_relpath_for_still(root, abs),
    ))
}

fn raw_relpath_for_still(root: &Path, still: &Path) -> Option<String> {
    media::dng_twin_for_still(still).and_then(|raw| {
        raw.strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    })
}

fn mtime_epoch(meta: &fs::Metadata) -> Result<i64> {
    let d = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::clipboard::{paste, paste_into, Clipboard, ClipboardOp};
    use image::{Rgb, RgbImage};

    fn write_still(path: &Path) {
        RgbImage::from_pixel(32, 24, Rgb([10, 20, 30]))
            .save(path)
            .unwrap();
    }

    fn mini_library() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        std::fs::create_dir_all(root.join("Rome")).unwrap();
        std::fs::create_dir_all(root.join("Paris")).unwrap();
        catalog::open(&root, true).unwrap();
        (tmp, root)
    }

    #[test]
    fn pasted_copy_is_visible_with_index_counts() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome/a.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        let stats = index_pasted(&root, &result.items, &clip.rels, clip.op).unwrap();
        assert_eq!(stats.added_or_updated, 1);
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.total, 2);
        assert!(stats.summary().contains("updated 1"));
        let conn = catalog::open(&root, false).unwrap();
        let paris = catalog::photos_in_album(&conn, "Paris").unwrap();
        assert_eq!(paris.len(), 1);
        assert_eq!(paris[0].relpath, "Paris/a.jpg");
        assert!(crate::thumbs::thumb_path(&root, "Paris/a.jpg").is_file());
    }

    #[test]
    fn pasted_cut_moves_row_and_prunes_source() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        write_still(&root.join("Rome/b.jpg"));
        index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        let result = paste(&root, &clip, "Paris").unwrap();
        let stats = index_pasted(&root, &result.items, &clip.rels, clip.op).unwrap();
        assert_eq!(stats.added_or_updated, 1);
        assert_eq!(stats.removed, 1);
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
        assert!(catalog::photos_in_album(&conn, "Paris")
            .unwrap()
            .iter()
            .any(|p| p.relpath == "Paris/a.jpg"));
        assert!(catalog::photos_in_album(&conn, "Rome")
            .unwrap()
            .iter()
            .all(|p| p.relpath != "Rome/a.jpg"));
    }

    #[test]
    fn pasted_cut_does_not_delete_skipped_live_sources() {
        let (_tmp, root) = mini_library();
        std::fs::create_dir_all(root.join("2024")).unwrap();
        write_still(&root.join("Rome/a.jpg"));
        index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Cut,
            rels: vec!["Rome/a.jpg".into()],
        };
        // Files are skipped when pasting folders-only into a collection.
        let result = paste_into(&root, &clip, None, "2024").unwrap();
        assert!(result.items.is_empty());
        let stats = index_pasted(&root, &result.items, &clip.rels, clip.op).unwrap();
        assert_eq!(stats.removed, 0);
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
    }

    #[test]
    fn pasted_folder_indexes_nested_media_only() {
        let (_tmp, root) = mini_library();
        std::fs::create_dir_all(root.join("2024")).unwrap();
        write_still(&root.join("Rome/a.jpg"));
        std::fs::write(root.join("Rome/notes.txt"), b"hi").unwrap();
        index_library(&root).unwrap();
        let clip = Clipboard {
            op: ClipboardOp::Copy,
            rels: vec!["Rome".into()],
        };
        let result = paste_into(&root, &clip, None, "2024").unwrap();
        let stats = index_pasted(&root, &result.items, &clip.rels, clip.op).unwrap();
        assert_eq!(stats.added_or_updated, 1);
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
        let photos = catalog::photos_in_album(&conn, "2024/Rome").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].relpath, "2024/Rome/a.jpg");
    }

    #[test]
    fn corrupt_paste_still_upserts_row_but_counts_failed() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/a.jpg"));
        index_library(&root).unwrap();
        std::fs::write(root.join("Paris/a.jpg"), b"not an image").unwrap();
        let items = vec![PastedItem {
            src: "Rome/a.jpg".into(),
            dest: "Paris/a.jpg".into(),
            is_dir: false,
        }];
        let stats = index_pasted(&root, &items, &["Rome/a.jpg".into()], ClipboardOp::Copy).unwrap();
        assert_eq!(stats.added_or_updated, 1);
        assert_eq!(stats.failed, 1);
        let conn = catalog::open(&root, false).unwrap();
        assert!(catalog::photos_in_album(&conn, "Paris")
            .unwrap()
            .iter()
            .any(|p| p.relpath == "Paris/a.jpg"));
    }

    #[test]
    fn jpeg_dng_twins_index_as_one_photo() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/DSC_0001.jpg"));
        std::fs::write(root.join("Rome/DSC_0001.DNG"), b"dng").unwrap();
        index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].relpath, "Rome/DSC_0001.jpg");
        assert_eq!(
            photos[0].raw_relpath.as_deref(),
            Some("Rome/DSC_0001.DNG")
        );
    }

    #[test]
    fn standalone_dng_indexes_without_raw_twin() {
        let (_tmp, root) = mini_library();
        std::fs::write(root.join("Rome/orphan.DNG"), b"dng").unwrap();
        index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].relpath, "Rome/orphan.DNG");
        assert!(photos[0].raw_relpath.is_none());
    }

    #[test]
    fn edited_jpeg_dng_twins_index_as_one_photo() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/DSC_0001-edited.jpg"));
        std::fs::write(root.join("Rome/DSC_0001-edited.DNG"), b"dng").unwrap();
        index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].relpath, "Rome/DSC_0001-edited.jpg");
        assert_eq!(
            photos[0].raw_relpath.as_deref(),
            Some("Rome/DSC_0001-edited.DNG")
        );
    }

    #[test]
    fn edited_jpeg_does_not_pair_with_original_dng() {
        let (_tmp, root) = mini_library();
        write_still(&root.join("Rome/DSC_0001-edited.jpg"));
        std::fs::write(root.join("Rome/DSC_0001.DNG"), b"dng").unwrap();
        index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 2);
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 2);
        let edited = photos
            .iter()
            .find(|p| p.relpath == "Rome/DSC_0001-edited.jpg")
            .unwrap();
        assert!(edited.raw_relpath.is_none());
        let original_dng = photos
            .iter()
            .find(|p| p.relpath == "Rome/DSC_0001.DNG")
            .unwrap();
        assert!(original_dng.raw_relpath.is_none());
    }

    #[test]
    fn lone_edited_dng_indexes_without_raw_twin() {
        let (_tmp, root) = mini_library();
        std::fs::write(root.join("Rome/DSC_0001-edited.DNG"), b"dng").unwrap();
        index_library(&root).unwrap();
        let conn = catalog::open(&root, false).unwrap();
        assert_eq!(catalog::count(&conn).unwrap(), 1);
        let photos = catalog::photos_in_album(&conn, "Rome").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].relpath, "Rome/DSC_0001-edited.DNG");
        assert!(photos[0].raw_relpath.is_none());
    }
}
