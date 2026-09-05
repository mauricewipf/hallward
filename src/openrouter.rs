//! OpenRouter HTTP client for Ask AI vision Q&A and image editing.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};

use crate::credentials::{self, CredentialSource, ResolvedKey};

pub const CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const IMAGES_URL: &str = "https://openrouter.ai/api/v1/images";

pub use crate::credentials::{DEFAULT_ASK_MODEL, DEFAULT_EDIT_MODEL};

/// Back-compat aliases for the built-in defaults.
pub const ASK_MODEL: &str = credentials::DEFAULT_ASK_MODEL;
pub const EDIT_MODEL: &str = credentials::DEFAULT_EDIT_MODEL;

const POLL: Duration = Duration::from_millis(50);
const ERROR_CAP: usize = 800;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub type PostFn = fn(&str, &str, &Value, Duration) -> Result<(u16, String), String>;

pub fn model_unavailable_message(model: &str) -> String {
    format!("OpenRouter model {model} is not available.")
}

pub fn ask_request_body(prompt: &str, images: &[(String, String)], model: &str) -> Value {
    let mut content = Vec::with_capacity(images.len() + 1);
    for (mime, b64) in images {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{b64}") },
        }));
    }
    content.push(json!({ "type": "text", "text": prompt }));
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": content }],
    })
}

pub fn edit_request_body(instruction: &str, mime: &str, b64: &str, model: &str) -> Value {
    let prompt = format!(
        "Edit this photograph according to the following instruction. \
Preserve everything not mentioned. Do not add text, watermarks, or captions. \
Return the edited image.\n\nInstruction:\n{instruction}"
    );
    json!({
        "model": model,
        "prompt": prompt,
        "n": 1,
        "input_references": [{
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{b64}") },
        }],
    })
}

pub fn interpret_chat_text(
    status: u16,
    body: &str,
    source: CredentialSource,
    model: &str,
) -> Result<String, String> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(err) = map_error_json(status, &value, source, model) {
            return Err(err);
        }
        if (200..300).contains(&status) {
            return parse_chat_text(&value);
        }
    }
    if !(200..300).contains(&status) {
        return Err(map_status_error(status, body, source, model));
    }
    Err("OpenRouter returned an unreadable response.".into())
}

pub fn interpret_image_bytes(
    status: u16,
    body: &str,
    source: CredentialSource,
    model: &str,
) -> Result<Vec<u8>, String> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(err) = map_error_json(status, &value, source, model) {
            return Err(err);
        }
        if (200..300).contains(&status) {
            return parse_image_bytes(&value);
        }
    }
    if !(200..300).contains(&status) {
        return Err(map_status_error(status, body, source, model));
    }
    Err("OpenRouter returned an unreadable response.".into())
}

pub fn run_ask(
    jpeg_paths: &[PathBuf],
    prompt: &str,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    post: PostFn,
    timeout: Duration,
) -> Result<String, String> {
    check_cancel(cancel)?;
    let mut images = Vec::with_capacity(jpeg_paths.len());
    for path in jpeg_paths {
        let bytes = fs::read(path).map_err(|error| {
            format!("Could not read Ask AI preview {}: {error}", path.display())
        })?;
        images.push(("image/jpeg".to_string(), BASE64.encode(bytes)));
    }
    let body = ask_request_body(prompt, &images, &key.ask_model);
    let (status, text) = post_cancellable(CHAT_URL, &key.key, &body, cancel, post, timeout)?;
    interpret_chat_text(status, &text, key.source, &key.ask_model)
}

pub fn run_edit_request(
    input_mime: &str,
    input_bytes: &[u8],
    instruction: &str,
    key: &ResolvedKey,
    cancel: &AtomicBool,
    post: PostFn,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    check_cancel(cancel)?;
    let b64 = BASE64.encode(input_bytes);
    let body = edit_request_body(instruction, input_mime, &b64, &key.edit_model);
    let (status, text) = post_cancellable(IMAGES_URL, &key.key, &body, cancel, post, timeout)?;
    interpret_image_bytes(status, &text, key.source, &key.edit_model)
}

fn post_cancellable(
    url: &str,
    api_key: &str,
    body: &Value,
    cancel: &AtomicBool,
    post: PostFn,
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

pub fn post_openrouter(
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
        .set("Authorization", &format!("Bearer {api_key}"))
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
            &format!("Could not reach OpenRouter: {error}"),
            api_key,
        )),
    }
}

fn parse_chat_text(value: &Value) -> Result<String, String> {
    if let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    Err("OpenRouter returned no answer.".into())
}

fn parse_image_bytes(value: &Value) -> Result<Vec<u8>, String> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "OpenRouter returned no image.".to_string())?;
    for item in items {
        if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
            let bytes = BASE64
                .decode(b64.trim().as_bytes())
                .map_err(|_| "OpenRouter returned an unreadable image.".to_string())?;
            if !bytes.is_empty() {
                return Ok(bytes);
            }
        }
    }
    if let Some(images) = value
        .pointer("/choices/0/message/images")
        .and_then(Value::as_array)
    {
        for image in images {
            if let Some(url) = image
                .get("image_url")
                .and_then(|part| part.get("url"))
                .and_then(Value::as_str)
            {
                if let Some(bytes) = decode_data_url(url) {
                    return Ok(bytes);
                }
            }
        }
    }
    Err("OpenRouter returned no image.".into())
}

fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let data = url.strip_prefix("data:")?;
    let payload = data.split_once(',').map(|(_, rest)| rest).unwrap_or(data);
    BASE64.decode(payload.trim().as_bytes()).ok()
}

fn map_error_json(
    status: u16,
    value: &Value,
    source: CredentialSource,
    model: &str,
) -> Option<String> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenRouter returned an error.");
    let code = error
        .get("code")
        .and_then(Value::as_u64)
        .unwrap_or(status as u64) as u16;
    Some(map_named_error(code, message, source, model))
}

fn map_status_error(status: u16, body: &str, source: CredentialSource, model: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(err) = map_error_json(status, &value, source, model) {
            return err;
        }
    }
    map_named_error(status, &cap_error(body), source, model)
}

fn map_named_error(status: u16, message: &str, source: CredentialSource, model: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if status == 401
        || status == 403
        || lower.contains("unauthenticated")
        || lower.contains("api key")
        || lower.contains("permission denied")
        || lower.contains("invalid api key")
    {
        return invalid_key_message(source);
    }
    if status == 404 || lower.contains("not found") || lower.contains("no endpoints found") {
        return model_unavailable_message(model);
    }
    if status == 429
        || lower.contains("resource_exhausted")
        || lower.contains("quota")
        || lower.contains("rate limit")
    {
        return "OpenRouter quota exceeded. Try again later.".into();
    }
    if status >= 500 {
        return "OpenRouter is temporarily unavailable. Try again later.".into();
    }
    if lower.contains("safety") || lower.contains("blocked") {
        return "OpenRouter blocked this request.".into();
    }
    let cleaned = cap_error(message);
    if cleaned.is_empty() {
        format!("OpenRouter returned an error (HTTP {status}).")
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

pub(crate) fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("Ask AI cancelled.".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_request_uses_chat_completions_shape() {
        let body = ask_request_body(
            "what car?",
            &[("image/jpeg".into(), "abcd".into())],
            ASK_MODEL,
        );
        assert_eq!(body["model"], ASK_MODEL);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert!(content[0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
        assert_eq!(content[1]["text"], "what car?");
    }

    #[test]
    fn edit_request_uses_images_api_shape() {
        let body = edit_request_body("blur background", "image/jpeg", "abcd", EDIT_MODEL);
        assert_eq!(body["model"], EDIT_MODEL);
        assert_eq!(body["n"], 1);
        assert_eq!(
            body["input_references"][0]["image_url"]["url"],
            "data:image/jpeg;base64,abcd"
        );
        assert!(body["prompt"].as_str().unwrap().contains("blur background"));
    }

    #[test]
    fn interpret_chat_text_reads_message_content() {
        let body = json!({
            "choices": [{ "message": { "content": "A red Ferrari." } }]
        });
        assert_eq!(
            interpret_chat_text(200, &body.to_string(), CredentialSource::File, ASK_MODEL).unwrap(),
            "A red Ferrari."
        );
    }

    #[test]
    fn interpret_image_bytes_reads_b64_json() {
        let png = BASE64.encode([1_u8, 2, 3, 4]);
        let body = json!({ "data": [{ "b64_json": png }] });
        assert_eq!(
            interpret_image_bytes(200, &body.to_string(), CredentialSource::File, EDIT_MODEL)
                .unwrap(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn interpret_auth_quota_and_model_errors() {
        let auth = json!({"error": {"code": 401, "message": "bad key"}});
        assert!(interpret_chat_text(
            401,
            &auth.to_string(),
            CredentialSource::Environment,
            ASK_MODEL
        )
        .unwrap_err()
        .contains("HALLWARD_OPENROUTER_API_KEY"));
        let saved = interpret_chat_text(401, &auth.to_string(), CredentialSource::File, ASK_MODEL)
            .unwrap_err();
        assert_eq!(saved, credentials::INVALID_SAVED_KEY);
        let quota = json!({"error": {"code": 429, "message": "quota"}});
        assert!(
            interpret_image_bytes(429, &quota.to_string(), CredentialSource::File, EDIT_MODEL)
                .unwrap_err()
                .contains("quota")
        );
        let missing = json!({"error": {"code": 404, "message": "model"}});
        assert!(interpret_image_bytes(
            404,
            &missing.to_string(),
            CredentialSource::File,
            EDIT_MODEL
        )
        .unwrap_err()
        .contains(EDIT_MODEL));
    }
}
