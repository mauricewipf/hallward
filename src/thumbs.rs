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

pub fn generate_thumb(root: &Path, abs: &Path, relpath: &str) -> Result<PathBuf> {
    let out = thumb_path(root, relpath);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    if out.exists() {
        let src_m = abs.metadata()?.modified()?;
        let dst_m = out.metadata()?.modified()?;
        if dst_m >= src_m {
            return Ok(out);
        }
    }

    let img = load_image(abs).with_context(|| format!("decode {}", abs.display()))?;
    let thumb = img.resize(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle);
    thumb
        .to_rgb8()
        .save_with_format(&out, ImageFormat::Jpeg)
        .with_context(|| format!("write {}", out.display()))?;
    Ok(out)
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
