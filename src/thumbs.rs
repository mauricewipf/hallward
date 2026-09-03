use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use sha2::{Digest, Sha256};

use crate::library::ALBUM_DIR;
use crate::media::{is_dng, is_heic, is_video};
use crate::meta;

pub const THUMB_SIZE: u32 = 256;
pub const AI_PREVIEW_MAX_SIZE: u32 = 1600;
const EDIT_INPUT_MAX_SIZE: u32 = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditInput {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

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

pub fn prepare_edit_input(source: &Path) -> Result<EditInput> {
    let image =
        load_image(source).with_context(|| format!("decode edit input {}", source.display()))?;
    let (width, height) = (image.width(), image.height());
    let image = if width > EDIT_INPUT_MAX_SIZE || height > EDIT_INPUT_MAX_SIZE {
        image.thumbnail(EDIT_INPUT_MAX_SIZE, EDIT_INPUT_MAX_SIZE)
    } else {
        image
    };
    let mut bytes = Vec::new();
    image
        .to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
        .with_context(|| format!("encode edit input {}", source.display()))?;
    Ok(EditInput {
        bytes,
        mime: "image/jpeg".into(),
        width: Some(width),
        height: Some(height),
    })
}

pub fn write_ai_preview(source: &Path, destination: &Path) -> Result<()> {
    let image =
        load_image(source).with_context(|| format!("decode Ask AI image {}", source.display()))?;
    let image = if image.width() > AI_PREVIEW_MAX_SIZE || image.height() > AI_PREVIEW_MAX_SIZE {
        image.thumbnail(AI_PREVIEW_MAX_SIZE, AI_PREVIEW_MAX_SIZE)
    } else {
        image
    };
    image
        .to_rgb8()
        .save_with_format(destination, ImageFormat::Jpeg)
        .with_context(|| format!("write Ask AI preview {}", destination.display()))
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
    if is_video(path) {
        return load_video_frame(path);
    }
    if is_heic(path) {
        return load_heic(path);
    }
    if is_dng(path) {
        return load_dng(path);
    }
    Ok(image::open(path)?)
}

/// Argv (including argv0) to extract a single JPEG frame. `ss` is the seek in seconds.
pub fn ffmpeg_frame_argv(path: &Path, out: &Path, ss: &str) -> Vec<std::ffi::OsString> {
    vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-ss".into(),
        ss.into(),
        "-i".into(),
        path.as_os_str().to_os_string(),
        "-vframes".into(),
        "1".into(),
        "-q:v".into(),
        "2".into(),
        out.as_os_str().to_os_string(),
    ]
}

fn load_video_frame(path: &Path) -> Result<DynamicImage> {
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("frame.jpg");
    if extract_frame(path, &out, "1").is_ok() {
        if let Ok(img) = image::open(&out) {
            return Ok(img);
        }
    }
    extract_frame(path, &out, "0").with_context(|| {
        format!(
            "run ffmpeg (install ffmpeg) to thumbnail {}",
            path.display()
        )
    })?;
    image::open(&out).with_context(|| format!("read ffmpeg frame for {}", path.display()))
}

fn extract_frame(path: &Path, out: &Path, ss: &str) -> Result<()> {
    let args = ffmpeg_frame_argv(path, out, ss);
    let status = Command::new(&args[0])
        .args(&args[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run ffmpeg (install ffmpeg)")?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed on {} (ss={ss})", path.display());
    }
    if !out.exists() {
        anyhow::bail!("ffmpeg wrote no frame for {}", path.display());
    }
    Ok(())
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

    #[test]
    fn ai_preview_preserves_aspect_ratio_and_strips_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.png");
        let destination = dir.path().join("preview.jpg");
        RgbImage::from_pixel(400, 200, Rgb([0, 128, 255]))
            .save(&source)
            .unwrap();

        write_ai_preview(&source, &destination).unwrap();

        assert_eq!(image::image_dimensions(destination).unwrap(), (400, 200));
    }

    #[test]
    fn ffmpeg_argv_seeks_then_extracts_one_jpeg() {
        let args = ffmpeg_frame_argv(Path::new("/lib/clip.mov"), Path::new("/tmp/frame.jpg"), "1");
        let s: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(s[0], "ffmpeg");
        assert!(s.contains(&"-ss".into()));
        assert!(!s.contains(&"-noautorotate".to_string()));
        assert_eq!(s[s.iter().position(|x| x == "-ss").unwrap() + 1], "1");
        assert!(s.contains(&"-vframes".into()));
        assert_eq!(s.last().unwrap(), "/tmp/frame.jpg");
    }
}
