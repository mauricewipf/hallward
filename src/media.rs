use std::path::Path;

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "tif", "tiff", "dng", "bmp",
];

pub fn is_image(path: &Path) -> bool {
    ext_in(path, IMAGE_EXTS)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_stills_not_videos() {
        assert!(is_image(&PathBuf::from("a.HEIC")));
        assert!(is_image(&PathBuf::from("a.jpg")));
        assert!(is_dng(&PathBuf::from("raw.DNG")));
        assert!(!is_image(&PathBuf::from("clip.MOV")));
        assert!(!is_image(&PathBuf::from("note.xmp")));
    }
}
