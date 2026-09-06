use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "tif", "tiff", "dng", "bmp",
];

pub const VIDEO_EXTS: &[&str] = &["mov", "mp4"];

const LIVE_STILL_EXT_VARIANTS: &[&str] =
    &["heic", "HEIC", "heif", "HEIF", "jpg", "JPG", "jpeg", "JPEG"];

/// Developed still extensions that pair with a same-stem DNG twin.
const JPEG_DNG_STILL_EXT_VARIANTS: &[&str] = &["jpg", "JPG", "jpeg", "JPEG"];

const DNG_EXT_VARIANTS: &[&str] = &["dng", "DNG"];

pub fn is_image(path: &Path) -> bool {
    ext_in(path, IMAGE_EXTS)
}

pub fn is_video(path: &Path) -> bool {
    ext_in(path, VIDEO_EXTS)
}

pub fn is_media_ext(path: &Path) -> bool {
    is_image(path) || is_video(path)
}

pub fn is_heic(path: &Path) -> bool {
    ext_in(path, &["heic", "heif"])
}

pub fn is_dng(path: &Path) -> bool {
    ext_in(path, &["dng"])
}

fn ext_in(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

pub fn bin_on_path(bin: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Argv (including argv0) to read Apple's Live Photo content identifier from a video.
pub fn ffprobe_live_photo_argv(path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "ffprobe".into(),
        "-v".into(),
        "error".into(),
        "-show_entries".into(),
        "format_tags=com.apple.quicktime.content.identifier".into(),
        "-of".into(),
        "default=nk=1:nw=1".into(),
        path.as_os_str().to_os_string(),
    ]
}

/// True when `path` is a Live Photo motion component and should not be indexed.
///
/// Prefers Apple's `com.apple.quicktime.content.identifier` via ffprobe. On
/// missing ffprobe or probe error, falls back to a same-stem still sibling.
pub fn is_live_photo_companion(path: &Path, have_ffprobe: bool) -> bool {
    if !is_video(path) {
        return false;
    }
    if have_ffprobe {
        match probe_content_identifier(path) {
            Ok(Some(_)) => return true,
            Ok(None) => return false,
            Err(_) => {}
        }
    }
    live_photo_companion_by_stem(path)
}

fn probe_content_identifier(path: &Path) -> anyhow::Result<Option<String>> {
    let args = ffprobe_live_photo_argv(path);
    let output = Command::new(&args[0])
        .args(&args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!("ffprobe failed on {}", path.display());
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        Ok(None)
    } else {
        Ok(Some(id))
    }
}

/// Same-stem still sibling (`<stem>.{heic,heif,jpg,jpeg}`), case variants on the extension.
/// True when `stem` names an AI edit sidecar (`{stem}-edited`, `{stem}-edited-N`).
pub fn is_edited_sidecar_stem(stem: &str) -> bool {
    stem.ends_with("-edited") || stem.contains("-edited-")
}

/// True when `path` is a JPEG with a same-stem DNG sibling in the same folder.
pub fn is_jpeg_dng_developed_still(path: &Path) -> bool {
    if !is_jpeg_dng_still(path) {
        return false;
    }
    dng_twin_for_still(path).is_some()
}

/// True when `path` is a DNG that should not be indexed (JPEG twin exists).
pub fn is_dng_raw_companion(path: &Path) -> bool {
    if !is_dng(path) {
        return false;
    }
    jpeg_dng_still_for_raw(path).is_some()
}

fn is_jpeg_dng_still(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| JPEG_DNG_STILL_EXT_VARIANTS.iter().any(|v| ext == *v))
        .unwrap_or(false)
}

/// Same-stem DNG sibling for a developed JPEG still, if present.
pub fn dng_twin_for_still(still: &Path) -> Option<PathBuf> {
    if !is_jpeg_dng_still(still) {
        return None;
    }
    let parent = still.parent()?;
    let stem = still.file_stem()?.to_str()?;
    dng_at_stem(parent, stem)
}

/// Same-stem JPEG sibling for a DNG raw file, if present.
pub fn jpeg_dng_still_for_raw(raw: &Path) -> Option<PathBuf> {
    let parent = raw.parent()?;
    let stem = raw.file_stem()?.to_str()?;
    if stem.is_empty() {
        return None;
    }
    jpeg_dng_still_at_stem(parent, stem)
}

fn dng_at_stem(parent: &Path, stem: &str) -> Option<PathBuf> {
    for ext in DNG_EXT_VARIANTS {
        let candidate = parent.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn jpeg_dng_still_at_stem(parent: &Path, stem: &str) -> Option<PathBuf> {
    for ext in JPEG_DNG_STILL_EXT_VARIANTS {
        let candidate = parent.join(format!("{stem}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn live_photo_companion_by_stem(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    LIVE_STILL_EXT_VARIANTS
        .iter()
        .any(|ext| parent.join(format!("{stem}.{ext}")).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;

    #[test]
    fn detects_stills_not_videos() {
        assert!(is_image(&PathBuf::from("a.HEIC")));
        assert!(is_image(&PathBuf::from("a.jpg")));
        assert!(is_dng(&PathBuf::from("raw.DNG")));
        assert!(!is_image(&PathBuf::from("clip.MOV")));
        assert!(!is_image(&PathBuf::from("note.xmp")));
    }

    #[test]
    fn detects_videos_by_extension() {
        assert!(is_video(&PathBuf::from("IMG_1234.MOV")));
        assert!(is_video(&PathBuf::from("clip.mp4")));
        assert!(!is_video(&PathBuf::from("note.txt")));
        assert!(!is_video(&PathBuf::from("a.HEIC")));
    }

    #[test]
    fn media_ext_accepts_stills_and_videos() {
        assert!(is_media_ext(&PathBuf::from("a.HEIC")));
        assert!(is_media_ext(&PathBuf::from("a.jpg")));
        assert!(is_media_ext(&PathBuf::from("clip.MOV")));
        assert!(is_media_ext(&PathBuf::from("clip.mp4")));
        assert!(!is_media_ext(&PathBuf::from("note.xmp")));
        assert!(!is_media_ext(&PathBuf::from("note.txt")));
    }

    #[test]
    fn ffprobe_argv_requests_content_identifier() {
        let args = ffprobe_live_photo_argv(Path::new("/lib/IMG_1.MOV"));
        let s: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(s[0], "ffprobe");
        assert!(s.contains(&"format_tags=com.apple.quicktime.content.identifier".into()));
        assert_eq!(s.last().unwrap(), "/lib/IMG_1.MOV");
    }

    #[test]
    fn stem_fallback_pairs_same_stem_still() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("IMG_1234.HEIC")).unwrap();
        File::create(dir.path().join("IMG_1234.MOV")).unwrap();
        assert!(live_photo_companion_by_stem(
            &dir.path().join("IMG_1234.MOV")
        ));
        assert!(is_live_photo_companion(
            &dir.path().join("IMG_1234.MOV"),
            false
        ));
    }

    #[test]
    fn stem_fallback_ignores_different_stems() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("photo.jpg")).unwrap();
        File::create(dir.path().join("other.MOV")).unwrap();
        assert!(!live_photo_companion_by_stem(&dir.path().join("other.MOV")));
        assert!(!is_live_photo_companion(
            &dir.path().join("other.MOV"),
            false
        ));
    }

    #[test]
    fn stem_fallback_standalone_video_is_not_companion() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("clip.MOV")).unwrap();
        assert!(!live_photo_companion_by_stem(&dir.path().join("clip.MOV")));
    }

    #[test]
    fn stem_fallback_extension_case_variants() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("img_1234.heic")).unwrap();
        File::create(dir.path().join("img_1234.MOV")).unwrap();
        assert!(live_photo_companion_by_stem(
            &dir.path().join("img_1234.MOV")
        ));
    }

    #[test]
    fn jpeg_dng_twins_pair_by_same_stem() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("DSC_0001.JPG")).unwrap();
        File::create(dir.path().join("DSC_0001.DNG")).unwrap();
        let jpg = dir.path().join("DSC_0001.JPG");
        let dng = dir.path().join("DSC_0001.DNG");
        assert!(is_jpeg_dng_developed_still(&jpg));
        assert!(is_dng_raw_companion(&dng));
        assert!(dng_twin_for_still(&jpg)
            .unwrap()
            .as_os_str()
            .eq_ignore_ascii_case(dng.as_os_str()));
        assert!(jpeg_dng_still_for_raw(&dng)
            .unwrap()
            .as_os_str()
            .eq_ignore_ascii_case(jpg.as_os_str()));
    }

    #[test]
    fn jpeg_dng_edited_sidecar_does_not_pair_with_original_dng() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("DSC_0001-edited.jpg")).unwrap();
        File::create(dir.path().join("DSC_0001.DNG")).unwrap();
        let edited = dir.path().join("DSC_0001-edited.jpg");
        assert!(!is_jpeg_dng_developed_still(&edited));
        assert!(dng_twin_for_still(&edited).is_none());
        assert!(!is_dng_raw_companion(&dir.path().join("DSC_0001.DNG")));
    }

    #[test]
    fn jpeg_dng_edited_twins_pair_by_same_stem() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("DSC_0001-edited.jpg")).unwrap();
        File::create(dir.path().join("DSC_0001-edited.DNG")).unwrap();
        let edited_jpg = dir.path().join("DSC_0001-edited.jpg");
        let edited_dng = dir.path().join("DSC_0001-edited.DNG");
        assert!(is_jpeg_dng_developed_still(&edited_jpg));
        assert!(is_dng_raw_companion(&edited_dng));
        assert!(dng_twin_for_still(&edited_jpg)
            .unwrap()
            .as_os_str()
            .eq_ignore_ascii_case(edited_dng.as_os_str()));
        assert!(jpeg_dng_still_for_raw(&edited_dng)
            .unwrap()
            .as_os_str()
            .eq_ignore_ascii_case(edited_jpg.as_os_str()));
    }

    #[test]
    fn lone_edited_dng_is_not_a_companion() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("DSC_0001-edited.DNG")).unwrap();
        let lone = dir.path().join("DSC_0001-edited.DNG");
        assert!(!is_dng_raw_companion(&lone));
        assert!(dng_twin_for_still(&lone).is_none());
    }

    #[test]
    fn standalone_dng_is_not_a_companion() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("solo.DNG")).unwrap();
        assert!(!is_dng_raw_companion(&dir.path().join("solo.DNG")));
    }

    #[test]
    fn standalone_dng_does_not_twin_with_itself() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join("orphan.DNG");
        File::create(&orphan).unwrap();
        assert!(dng_twin_for_still(&orphan).is_none());
    }

    #[test]
    fn standalone_jpeg_without_dng_is_not_paired() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("solo.jpg")).unwrap();
        assert!(!is_jpeg_dng_developed_still(&dir.path().join("solo.jpg")));
    }
}
