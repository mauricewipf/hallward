use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use anyhow::Result;
use exif::{In, Reader, Tag, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhotoMeta {
    pub captured_at: Option<String>,
    pub camera: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub fn read_meta(path: &Path) -> PhotoMeta {
    read_meta_inner(path).unwrap_or_default()
}

/// Sort key for videos that lack EXIF: filesystem mtime as `YYYY:MM:DD HH:MM:SS` (UTC).
pub fn video_meta_from_mtime(mtime: i64) -> PhotoMeta {
    PhotoMeta {
        captured_at: Some(unix_to_exif_datetime(mtime)),
        camera: None,
        width: None,
        height: None,
    }
}

/// Civil UTC date from a Unix timestamp. Howard Hinnant's days-from-civil inverse.
fn unix_to_exif_datetime(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}:{m:02}:{d:02} {hour:02}:{min:02}:{sec:02}")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn read_meta_inner(path: &Path) -> Result<PhotoMeta> {
    let mut file = File::open(path)?;
    if let Ok(exif) = Reader::new().read_from_container(&mut BufReader::new(&mut file)) {
        return Ok(from_exif(&exif));
    }
    // HEIC and some DNG: scan for an Exif TIFF blob.
    let bytes = std::fs::read(path)?;
    if let Some(meta) = parse_embedded_exif(&bytes) {
        return Ok(meta);
    }
    Ok(PhotoMeta::default())
}

fn parse_embedded_exif(bytes: &[u8]) -> Option<PhotoMeta> {
    let needle = b"Exif\0\0";
    let pos = bytes.windows(needle.len()).position(|w| w == needle)?;
    let tiff = &bytes[pos + needle.len()..];
    let exif = Reader::new()
        .read_raw(tiff.to_vec())
        .or_else(|_| Reader::new().read_from_container(&mut Cursor::new(tiff)))
        .ok()?;
    Some(from_exif(&exif))
}

fn from_exif(exif: &exif::Exif) -> PhotoMeta {
    let captured_at = string_tag(exif, Tag::DateTimeOriginal)
        .or_else(|| string_tag(exif, Tag::DateTimeDigitized))
        .or_else(|| string_tag(exif, Tag::DateTime));
    let make = string_tag(exif, Tag::Make);
    let model = string_tag(exif, Tag::Model);
    let camera = match (make, model) {
        (Some(m), Some(model)) if model.to_lowercase().contains(&m.to_lowercase()) => Some(model),
        (Some(m), Some(model)) => Some(format!("{m} {model}")),
        (None, Some(model)) => Some(model),
        (Some(m), None) => Some(m),
        _ => None,
    };
    let width = uint_tag(exif, Tag::PixelXDimension).or_else(|| uint_tag(exif, Tag::ImageWidth));
    let height = uint_tag(exif, Tag::PixelYDimension).or_else(|| uint_tag(exif, Tag::ImageLength));
    PhotoMeta {
        captured_at,
        camera,
        width,
        height,
    }
}

fn string_tag(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let s = field.display_value().with_unit(exif).to_string();
    let s = s.trim().trim_matches('"').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn uint_tag(exif: &exif::Exif, tag: Tag) -> Option<u32> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    match field.value {
        Value::Long(ref v) => v.first().copied(),
        Value::Short(ref v) => v.first().map(|n| u32::from(*n)),
        _ => None,
    }
}

/// Best-effort JPEG preview embedded in DNG/TIFF.
pub fn embedded_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let start = bytes.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF])?;
    let rest = &bytes[start..];
    let end_rel = rest.windows(2).position(|w| w == [0xFF, 0xD9])?;
    Some(rest[..=end_rel + 1].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_embedded_jpeg_soi_eoi() {
        let mut data = vec![0, 1, 2];
        data.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0x01, 0x02, 0xFF, 0xD9, 9]);
        let jpeg = embedded_jpeg(&data).unwrap();
        assert_eq!(jpeg.first(), Some(&0xFF));
        assert_eq!(jpeg.last(), Some(&0xD9));
    }

    #[test]
    fn video_mtime_formats_exif_datetime() {
        let meta = video_meta_from_mtime(0);
        assert_eq!(meta.captured_at.as_deref(), Some("1970:01:01 00:00:00"));
        let meta = video_meta_from_mtime(1_704_067_200); // 2024-01-01 00:00:00 UTC
        assert_eq!(meta.captured_at.as_deref(), Some("2024:01:01 00:00:00"));
    }
}
