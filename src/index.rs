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
        let meta_fs = fs::metadata(path)?;
        let mtime = mtime_epoch(&meta_fs)?;
        let size = meta_fs.len() as i64;
        if let Some((old_m, old_s)) = catalog::get_mtime_size(&conn, &rel)? {
            if old_m == mtime && old_s == size && thumbs::is_current(root, path, &rel) {
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
            rel, filename, album, mtime, size, photo_meta,
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
                match thumbs::generate_thumb(root, &abs, &photo.relpath) {
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
    let meta_fs = fs::metadata(abs)?;
    let mtime = mtime_epoch(&meta_fs)?;
    let size = meta_fs.len() as i64;
    let filename = abs
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let album = catalog::album_relpath(&rel);
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
    thumbs::generate_thumb(root, abs, &rel)?;
    let photo = catalog::photo_from_file(rel, filename, album, mtime, size, photo_meta);
    let conn = catalog::open(root, true)?;
    catalog::upsert_photo(&conn, &photo)?;
    Ok(photo)
}

fn mtime_epoch(meta: &fs::Metadata) -> Result<i64> {
    let d = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(d.as_secs() as i64)
}
