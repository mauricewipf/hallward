use std::fs;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
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

pub fn init_library(root: &Path) -> Result<IndexStats> {
    let (album, _) = album_paths(root);
    fs::create_dir_all(&album)?;
    fs::create_dir_all(album.join("thumbs"))?;
    index_library(root)
}

pub fn index_library(root: &Path) -> Result<IndexStats> {
    let conn = catalog::open(root, true)?;
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
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_media_ext(path) {
            continue;
        }
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
                eprintln!(
                    "hallward: ffprobe not found; Live Photo detection falls back to filename pairing (install ffmpeg)"
                );
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

    stats.removed = catalog::delete_missing(&conn, &seen)?;

    if !have_ffmpeg && dirty.iter().any(|p| is_video(&root.join(&p.relpath))) {
        eprintln!(
            "hallward: ffmpeg not found; video thumbnails skipped (install ffmpeg to enable)"
        );
    }

    let results: Vec<(Photo, Option<String>)> = dirty
        .into_par_iter()
        .map(|photo| {
            let abs = root.join(&photo.relpath);
            if is_video(&abs) && !have_ffmpeg {
                return (photo, None);
            }
            match thumbs::generate_thumb(root, &abs, &photo.relpath) {
                Ok(_) => (photo, None),
                Err(e) => (photo, Some(format!("{e:#}"))),
            }
        })
        .collect();

    for (photo, err) in results {
        if let Some(e) = err {
            eprintln!("hallward: thumbnail failed for {}: {e}", photo.relpath);
            stats.failed += 1;
        }
        catalog::upsert(&conn, &photo)?;
        stats.added_or_updated += 1;
    }

    stats.total = catalog::count(&conn)?;
    Ok(stats)
}

fn mtime_epoch(meta: &fs::Metadata) -> Result<i64> {
    let d = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(d.as_secs() as i64)
}
