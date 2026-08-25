use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use sha2::{Digest, Sha256};

use crate::library::ALBUM_DIR;
use crate::media::{is_dng, is_heic};
use crate::meta;

pub const THUMB_SIZE: u32 = 256;

pub fn thumbs_dir(root: &Path) -> PathBuf {
    root.join(ALBUM_DIR).join("thumbs")
}

pub fn thumb_filename(relpath: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(relpath.as_bytes());
    format!("{:x}.jpg", hasher.finalize())
}

pub fn thumb_path(root: &Path, relpath: &str) -> PathBuf {
    thumbs_dir(root).join(thumb_filename(relpath))
}

/// True when a 256×256 thumb exists and is at least as new as the original.
pub fn is_current(root: &Path, abs: &Path, relpath: &str) -> bool {
    let out = thumb_path(root, relpath);
    let Ok(src_m) = abs.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(dst_m) = out.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    dst_m >= src_m && is_square_thumb(&out)
}

pub fn generate_thumb(root: &Path, abs: &Path, relpath: &str) -> Result<PathBuf> {
    let out = thumb_path(root, relpath);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    if is_current(root, abs, relpath) {
        return Ok(out);
    }

    let img = load_image(abs).with_context(|| format!("decode {}", abs.display()))?;
    let thumb = crop_square(img);
    thumb
        .to_rgb8()
        .save_with_format(&out, ImageFormat::Jpeg)
        .with_context(|| format!("write {}", out.display()))?;
    Ok(out)
}

fn crop_square(img: DynamicImage) -> DynamicImage {
    img.resize_to_fill(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle)
}

fn is_square_thumb(path: &Path) -> bool {
    image::image_dimensions(path)
        .ok()
        .is_some_and(|(w, h)| w == THUMB_SIZE && h == THUMB_SIZE)
}

fn load_image(path: &Path) -> Result<DynamicImage> {
    if is_heic(path) {
        return load_heic(path);
    }
    if is_dng(path) {
        return load_dng(path);
    }
    Ok(image::open(path)?)
}

fn load_heic(path: &Path) -> Result<DynamicImage> {
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("preview.jpg");
    let status = Command::new("heif-convert")
        .arg(path)
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run heif-convert (install libheif)")?;
    if !status.success() {
        anyhow::bail!("heif-convert failed on {}", path.display());
    }
    Ok(image::open(&out)?)
}

fn load_dng(path: &Path) -> Result<DynamicImage> {
    let bytes = fs::read(path)?;
    if let Some(jpeg) = meta::embedded_jpeg(&bytes) {
        if let Ok(img) = image::load_from_memory(&jpeg) {
            return Ok(img);
        }
    }
    Ok(placeholder())
}

fn placeholder() -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        THUMB_SIZE,
        THUMB_SIZE,
        image::Rgb([32, 32, 32]),
    ))
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    #[test]
    fn crop_square_fills_thumb_size() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(40, 20, Rgb([255, 0, 0])));
        let thumb = crop_square(img);
        assert_eq!((thumb.width(), thumb.height()), (THUMB_SIZE, THUMB_SIZE));
    }

    #[test]
    fn crop_square_portrait_also_fills() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(20, 40, Rgb([0, 255, 0])));
        let thumb = crop_square(img);
        assert_eq!((thumb.width(), thumb.height()), (THUMB_SIZE, THUMB_SIZE));
    }
}
