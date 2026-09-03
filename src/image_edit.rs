//! Direct Gemini image editing: prompt + photo in, edited sibling PNG out.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::ImageFormat;
use serde_json::{json, Value};

use crate::catalog;
use crate::credentials::{self, CredentialSource, ResolvedKey};
use crate::index;
use crate::meta;
use crate::thumbs::{self, EditInput};

pub const GEMINI_EDIT_MODEL: &str = "gemini-3.1-flash-image";
pub const EDIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(50);
const ERROR_CAP: usize = 800;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEdit {
    pub relpath: String,
    pub filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedImage {
    pub bytes: Vec<u8>,
}

pub type PostGeminiFn = fn(&str, &str, &Value, Duration) -> Result<(u16, String), String>;

pub fn edit_needs_one_photo_message() -> String {
    "Image editing needs exactly one marked photo.".into()
}

pub fn saved_message(filename: &str) -> String {
    format!("Saved {filename}")
}

pub fn model_unavailable_message() -> String {
    format!("Gemini model {GEMINI_EDIT_MODEL} is not available.")
}

pub fn gemini_request_body(instruction: &str, mime: &str, b64: &str) -> Value {
    let prompt = format!(
        "Edit this photograph according to the following instruction. \
Preserve everything not mentioned. Do not add text, watermarks, or captions. \
Return the edited image.\n\nInstruction:\n{instruction}"
    );
    json!({
        "contents": [{
            "role": "user",
            "parts": [
                {
                    "inlineData": {
                        "mimeType": mime,
                        "data": b64,
                    }
                },
                { "text": prompt }
            ]
        }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"],
        },
    })
}

pub fn interpret_gemini_http(
    status: u16,
    body: &str,
    source: CredentialSource,
) -> Result<EditedImage, String> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(err) = map_gemini_error_json(status, &value, source) {
            return Err(err);
        }
        if (200..300).contains(&status) {
            return parse_edited_image(&value);
        }
    }
    if !(200..300).contains(&status) {
        return Err(map_status_error(status, body, source));
    }
    Err("Gemini returned an unreadable response.".into())
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

pub fn run_gemini_edit(
    source: &Path,
    library_root: &Path,
    instruction: &str,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    post: PostGeminiFn,
    timeout: Duration,
) -> Result<SavedEdit, String> {
    check_cancel(cancel)?;
    let input = thumbs::prepare_edit_input(source)
        .map_err(|error| format!("Could not prepare the photo for editing: {error:#}"))?;
    let dest = unique_sibling_path(source, "png")?;
    let edited = request_edit(
        &key.key,
        key.source,
        &input,
        instruction,
        cancel,
        post,
        timeout,
    )?;
    check_cancel(cancel)?;
    let png = transcode_to_png(&edited.bytes)?;
    atomic_write(&dest, &png)?;
    index_saved_edit(source, &dest, library_root)
}

fn request_edit(
    api_key: &str,
    source: CredentialSource,
    input: &EditInput,
    instruction: &str,
    cancel: &AtomicBool,
    post: PostGeminiFn,
    timeout: Duration,
) -> Result<EditedImage, String> {
    let b64 = BASE64.encode(&input.bytes);
    let body = gemini_request_body(instruction, &input.mime, &b64);
    let url = format!(
        "https://generativelanguage.googleapis.com/v1/models/{GEMINI_EDIT_MODEL}:generateContent"
    );
    let (status, text) = post_cancellable(&url, api_key, &body, cancel, post, timeout)?;
    interpret_gemini_http(status, &text, source)
}

fn post_cancellable(
    url: &str,
    api_key: &str,
    body: &Value,
    cancel: &AtomicBool,
    post: PostGeminiFn,
    timeout: Duration,
) -> Result<(u16, String), String> {
    check_cancel(cancel)?;
    let url = url.to_string();
    let api_key = api_key.to_string();
    let body = body.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(post(&url, &api_key, &body, timeout));
    });
    let started = Instant::now();
    loop {
        check_cancel(cancel)?;
        if started.elapsed() >= timeout {
            return Err("The AI request timed out.".into());
        }
        match rx.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Empty) => thread::sleep(POLL),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("The AI request ended unexpectedly.".into());
            }
        }
    }
}

pub fn post_gemini(
    url: &str,
    api_key: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(timeout)
        .build();
    let result = agent
        .post(url)
        .set("x-goog-api-key", api_key)
        .set("Content-Type", "application/json")
        .send_json(body.clone());
    match result {
        Ok(response) => {
            let status = response.status();
            let text = response
                .into_string()
                .map_err(|error| sanitize_error(&error.to_string(), api_key))?;
            Ok((status, text))
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Ok((status, text))
        }
        Err(ureq::Error::Transport(error)) => Err(sanitize_error(
            &format!("Could not reach Gemini: {error}"),
            api_key,
        )),
    }
}

fn transcode_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes)
        .map_err(|_| "Gemini returned an unreadable image.".to_string())?;
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|_| "Gemini returned an unreadable image.".to_string())?;
    Ok(out)
}

fn parse_edited_image(value: &Value) -> Result<EditedImage, String> {
    if candidate_finish_reason(value)
        .map(|reason| {
            reason.eq_ignore_ascii_case("SAFETY") || reason.eq_ignore_ascii_case("IMAGE_SAFETY")
        })
        .unwrap_or(false)
    {
        return Err("Gemini blocked this edit.".into());
    }
    let parts = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini returned no image.".to_string())?;
    for part in parts {
        if let Some((_, data)) = part_inline(part) {
            let bytes = BASE64
                .decode(data.trim().as_bytes())
                .map_err(|_| "Gemini returned an unreadable image.".to_string())?;
            if bytes.is_empty() {
                return Err("Gemini returned no image.".into());
            }
            return Ok(EditedImage { bytes });
        }
    }
    Err("Gemini returned no image.".into())
}

fn part_inline(part: &Value) -> Option<(&str, &str)> {
    let inline = part.get("inlineData").or_else(|| part.get("inline_data"))?;
    let mime = inline
        .get("mimeType")
        .or_else(|| inline.get("mime_type"))
        .and_then(Value::as_str)?;
    let data = inline.get("data").and_then(Value::as_str)?;
    Some((mime, data))
}

fn candidate_finish_reason(value: &Value) -> Option<&str> {
    value
        .pointer("/candidates/0/finishReason")
        .or_else(|| value.pointer("/candidates/0/finish_reason"))
        .and_then(Value::as_str)
}

fn map_gemini_error_json(status: u16, value: &Value, source: CredentialSource) -> Option<String> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Gemini returned an error.");
    let code = error
        .get("code")
        .and_then(Value::as_u64)
        .unwrap_or(status as u64) as u16;
    let status_name = error.get("status").and_then(Value::as_str).unwrap_or("");
    Some(map_named_error(code, status_name, message, source))
}

fn map_status_error(status: u16, body: &str, source: CredentialSource) -> String {
    map_named_error(status, "", &cap_error(body), source)
}

fn map_named_error(
    status: u16,
    status_name: &str,
    message: &str,
    source: CredentialSource,
) -> String {
    let lower = format!("{status_name} {message}").to_ascii_lowercase();
    if status == 401
        || status == 403
        || lower.contains("unauthenticated")
        || lower.contains("api key")
        || lower.contains("permission denied")
    {
        return invalid_key_message(source);
    }
    if status == 404 || lower.contains("not found") {
        return model_unavailable_message();
    }
    if status == 429
        || lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("rate limit")
    {
        return "Gemini quota exceeded. Try again later.".into();
    }
    if status >= 500 {
        return "Gemini is temporarily unavailable. Try again later.".into();
    }
    if lower.contains("safety") || lower.contains("blocked") {
        return "Gemini blocked this edit.".into();
    }
    let cleaned = cap_error(message);
    if cleaned.is_empty() {
        format!("Gemini returned an error (HTTP {status}).")
    } else {
        cleaned
    }
}

fn invalid_key_message(source: CredentialSource) -> String {
    match source {
        CredentialSource::Environment => credentials::invalid_env_key_message(),
        CredentialSource::File => credentials::invalid_saved_key_message(),
    }
}

fn sanitize_error(text: &str, api_key: &str) -> String {
    let stripped = if api_key.is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "…")
    };
    cap_error(&stripped)
}

fn cap_error(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ERROR_CAP {
        trimmed.to_string()
    } else {
        let mut s: String = trimmed.chars().take(ERROR_CAP).collect();
        s.push('…');
        s
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("Ask AI cancelled.".into())
    } else {
        Ok(())
    }
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
    use crate::library::album_paths;
    use image::{Rgb, RgbImage};

    #[test]
    fn request_body_embeds_instruction_and_image() {
        let body = gemini_request_body("Remove the people", "image/jpeg", "abcd");
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            json!(["TEXT", "IMAGE"])
        );
        let parts = &body["contents"][0]["parts"];
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(parts[0]["inlineData"]["data"], "abcd");
        assert!(parts[1]["text"]
            .as_str()
            .unwrap()
            .contains("Remove the people"));
    }

    #[test]
    fn interpret_decodes_inline_image() {
        let png = BASE64.encode([1_u8, 2, 3, 4]);
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "Done." },
                        { "inlineData": { "mimeType": "image/png", "data": png } }
                    ]
                }
            }]
        });
        let edited = interpret_gemini_http(200, &body.to_string(), CredentialSource::File).unwrap();
        assert_eq!(edited.bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn interpret_accepts_snake_case_inline_data() {
        let jpeg = BASE64.encode([9_u8, 8, 7]);
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "inline_data": { "mime_type": "image/jpeg", "data": jpeg }
                    }]
                }
            }]
        });
        let edited = interpret_gemini_http(200, &body.to_string(), CredentialSource::File).unwrap();
        assert_eq!(edited.bytes, vec![9, 8, 7]);
    }

    #[test]
    fn interpret_missing_image_is_an_error() {
        let body = json!({
            "candidates": [{ "content": { "parts": [{ "text": "I edited it." }] } }]
        });
        assert_eq!(
            interpret_gemini_http(200, &body.to_string(), CredentialSource::File).unwrap_err(),
            "Gemini returned no image."
        );
    }

    #[test]
    fn interpret_safety_block() {
        let body = json!({
            "candidates": [{
                "finishReason": "SAFETY",
                "content": { "parts": [{ "text": "no" }] }
            }]
        });
        assert_eq!(
            interpret_gemini_http(200, &body.to_string(), CredentialSource::File).unwrap_err(),
            "Gemini blocked this edit."
        );
    }

    #[test]
    fn interpret_auth_quota_and_model_errors() {
        let auth =
            json!({"error": {"code": 401, "status": "UNAUTHENTICATED", "message": "bad key"}});
        assert!(
            interpret_gemini_http(401, &auth.to_string(), CredentialSource::Environment)
                .unwrap_err()
                .contains("GEMINI_API_KEY")
        );
        let saved =
            interpret_gemini_http(401, &auth.to_string(), CredentialSource::File).unwrap_err();
        assert_eq!(saved, credentials::INVALID_SAVED_KEY);
        let quota =
            json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota"}});
        assert!(
            interpret_gemini_http(429, &quota.to_string(), CredentialSource::File)
                .unwrap_err()
                .contains("quota")
        );
        let missing = json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "model"}});
        assert!(
            interpret_gemini_http(404, &missing.to_string(), CredentialSource::File)
                .unwrap_err()
                .contains(GEMINI_EDIT_MODEL)
        );
    }

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
        let key = ResolvedKey {
            key: "test".into(),
            source: CredentialSource::File,
        };
        let err = run_gemini_edit(
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
