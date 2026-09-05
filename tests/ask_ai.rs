//! End-to-end Ask AI coverage: a real library on disk, a real catalog write,
//! and an injectable OpenRouter HTTP transport — no network or credentials needed.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hallward::ai::{self, AskHandle, AskValue, Timeouts};
use hallward::catalog;
use hallward::credentials::{self, CredentialSource, ResolvedKey};
use hallward::image_edit::{self, PostFn, ASK_MODEL, EDIT_MODEL};
use hallward::index;
use hallward::openrouter::{self, CHAT_URL, IMAGES_URL};
use image::{Rgb, RgbImage};
use serde_json::{json, Value};

/// A library root with a catalog, plus helpers to add indexed photos.
struct Library {
    dir: tempfile::TempDir,
}

impl Library {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        catalog::open(dir.path(), true).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn photo(&self, rel: &str) -> PathBuf {
        let abs = self.root().join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        RgbImage::from_pixel(48, 32, Rgb([12, 34, 56]))
            .save(&abs)
            .unwrap();
        index::index_new_file(self.root(), &abs, Some("2024:01:02 03:04:05")).unwrap();
        abs
    }

    fn album(&self, album: &str) -> Vec<String> {
        let conn = catalog::open(self.root(), false).unwrap();
        catalog::photos_in_album(&conn, album)
            .unwrap()
            .into_iter()
            .map(|photo| photo.filename)
            .collect()
    }

    fn captured_at(&self, album: &str, filename: &str) -> Option<String> {
        let conn = catalog::open(self.root(), false).unwrap();
        catalog::photos_in_album(&conn, album)
            .unwrap()
            .into_iter()
            .find(|photo| photo.filename == filename)
            .unwrap_or_else(|| panic!("{filename} is not in the catalog"))
            .captured_at
    }
}

fn budget() -> Timeouts {
    Timeouts {
        ask: Duration::from_secs(10),
        edit: Duration::from_secs(10),
    }
}

fn test_key() -> ResolvedKey {
    ResolvedKey::new("test-key", CredentialSource::File)
}

fn settle(handle: &mut AskHandle) -> Result<AskValue, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(outcome) = handle.try_recv() {
            return outcome.result;
        }
        assert!(Instant::now() < deadline, "the Ask AI job never finished");
        thread::sleep(Duration::from_millis(10));
    }
}

fn ask_with(
    prompt: &str,
    files: &[PathBuf],
    root: &Path,
    key: ResolvedKey,
    timeouts: Timeouts,
    post: PostFn,
) -> Result<AskValue, String> {
    let mut handle = ai::spawn_with(
        1,
        prompt.to_string(),
        files.to_vec(),
        root.to_path_buf(),
        key,
        timeouts,
        post,
    );
    settle(&mut handle)
}

fn ask(prompt: &str, files: &[PathBuf], root: &Path, post: PostFn) -> Result<AskValue, String> {
    ask_with(prompt, files, root, test_key(), budget(), post)
}

fn with_credentials_path<F: FnOnce()>(f: F) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let prev = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
    let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
    std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
    std::env::remove_var("OPENROUTER_API_KEY");
    f();
    match prev {
        Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
        None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
    }
    match prev_openrouter {
        Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
        None => std::env::remove_var("OPENROUTER_API_KEY"),
    }
}

static OPENROUTER_STUB: Mutex<Option<OpenRouterStub>> = Mutex::new(None);

/// Serializes tests that share the global `OPENROUTER_STUB` so parallel runs
/// don't overwrite each other's installed response.
static OPENROUTER_TEST_LOCK: Mutex<()> = Mutex::new(());

struct OpenRouterStub {
    responses: Vec<(u16, String)>,
    seen_urls: Vec<String>,
    seen_keys: Vec<String>,
    seen_bodies: Vec<Value>,
}

impl OpenRouterStub {
    fn single(status: u16, body: Value) -> Self {
        Self {
            responses: vec![(status, body.to_string())],
            seen_urls: Vec::new(),
            seen_keys: Vec::new(),
            seen_bodies: Vec::new(),
        }
    }

    fn queue(status_bodies: Vec<(u16, Value)>) -> Self {
        Self {
            responses: status_bodies
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
            seen_urls: Vec::new(),
            seen_keys: Vec::new(),
            seen_bodies: Vec::new(),
        }
    }
}

fn install_openrouter_stub(stub: OpenRouterStub) {
    *OPENROUTER_STUB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stub);
}

fn stub_openrouter_post(
    url: &str,
    api_key: &str,
    body: &Value,
    _timeout: Duration,
) -> Result<(u16, String), String> {
    let mut guard = OPENROUTER_STUB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let stub = guard.as_mut().expect("openrouter stub not installed");
    stub.seen_urls.push(url.to_string());
    stub.seen_keys.push(api_key.to_string());
    stub.seen_bodies.push(body.clone());
    let (status, text) = stub.responses.remove(0);
    Ok((status, text))
}

fn openrouter_test_lock() -> std::sync::MutexGuard<'static, ()> {
    OPENROUTER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn text_response(text: &str) -> Value {
    json!({
        "choices": [{ "message": { "content": text } }]
    })
}

fn png_response() -> Value {
    let png = RgbImage::from_pixel(24, 16, Rgb([90, 140, 200]));
    let mut bytes = Vec::new();
    png.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    let data = BASE64.encode(bytes);
    json!({
        "data": [{ "b64_json": data }]
    })
}

fn is_ask_request(url: &str, body: &Value) -> bool {
    url == CHAT_URL || body.get("model") == Some(&json!(ASK_MODEL))
}

fn is_edit_request(url: &str, body: &Value) -> bool {
    url == IMAGES_URL || body.get("model") == Some(&json!(EDIT_MODEL))
}

fn ask_then_edit_post(
    ask_text: &str,
    url: &str,
    api_key: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, String), String> {
    if is_ask_request(url, body) {
        Ok((200, text_response(ask_text).to_string()))
    } else if is_edit_request(url, body) {
        Ok((200, png_response().to_string()))
    } else {
        stub_openrouter_post(url, api_key, body, timeout)
    }
}

#[test]
fn a_plain_answer_reaches_the_caller() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_openrouter_stub(OpenRouterStub::single(200, text_response("A red Ferrari.")));

    let value = ask("which car?", &[photo], library.root(), stub_openrouter_post).unwrap();

    assert_eq!(value, AskValue::Answer("A red Ferrari.".into()));
}

#[test]
fn multi_paragraph_answers_keep_only_the_last_paragraph() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_openrouter_stub(OpenRouterStub::single(
        200,
        text_response("Let me inspect the image.\n\nA red Ferrari."),
    ));

    let value = ask("which car?", &[photo], library.root(), stub_openrouter_post).unwrap();

    assert_eq!(value, AskValue::Answer("A red Ferrari.".into()));
}

#[test]
fn ask_only_sees_jpeg_previews() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_openrouter_stub(OpenRouterStub::single(200, text_response("ok")));

    ask(
        "what is this?",
        &[photo],
        library.root(),
        stub_openrouter_post,
    )
    .unwrap();

    let stub = OPENROUTER_STUB.lock().unwrap_or_else(|p| p.into_inner());
    let body = stub.as_ref().unwrap().seen_bodies.first().unwrap();
    assert_eq!(body["model"], ASK_MODEL);
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "image_url");
    assert!(content[0]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
    assert!(content[0]["image_url"]["url"].as_str().unwrap().len() > 30);
    assert_eq!(stub.as_ref().unwrap().seen_urls[0], CHAT_URL);
}

#[test]
fn an_edit_directive_writes_indexes_and_reports_a_sibling() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_openrouter_stub(OpenRouterStub::queue(vec![
        (
            200,
            text_response(r#"{"edit":"Remove the people in the background."}"#),
        ),
        (200, png_response()),
    ]));

    let value = ask(
        "remove the people in the background",
        std::slice::from_ref(&photo),
        library.root(),
        stub_openrouter_post,
    )
    .unwrap();

    let AskValue::Saved(saved) = value else {
        panic!("expected a saved edit");
    };
    assert_eq!(saved.filename, "photo-edited.png");
    assert_eq!(saved.relpath, "Rome/photo-edited.png");
    assert!(library.root().join("Rome/photo-edited.png").is_file());
    assert_eq!(library.album("Rome"), vec!["photo-edited.png", "photo.jpg"]);
    assert_eq!(
        library.captured_at("Rome", "photo-edited.png"),
        library.captured_at("Rome", "photo.jpg")
    );

    let stub = OPENROUTER_STUB.lock().unwrap_or_else(|p| p.into_inner());
    let seen = stub.as_ref().unwrap();
    assert!(seen.seen_urls.iter().any(|url| url == CHAT_URL));
    assert!(seen.seen_urls.iter().any(|url| url == IMAGES_URL));
    assert_eq!(seen.seen_keys, vec!["test-key", "test-key"]);
}

#[test]
fn an_invalid_saved_key_is_cleared() {
    let _lock = openrouter_test_lock();
    install_openrouter_stub(OpenRouterStub::single(
        401,
        json!({"error": {"code": 401, "message": "bad"}}),
    ));
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    with_credentials_path(|| {
        credentials::save_api_key("saved-key").unwrap();
        let error = ask("which car?", &[photo], library.root(), stub_openrouter_post).unwrap_err();
        assert_eq!(error, credentials::INVALID_SAVED_KEY);
        assert!(!credentials::resolve().is_some());
    });
}

#[test]
fn a_provider_error_names_the_ask_model() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_openrouter_stub(OpenRouterStub::single(
        404,
        json!({"error": {"code": 404, "message": "model missing"}}),
    ));

    let error = ask("which car?", &[photo], library.root(), stub_openrouter_post).unwrap_err();

    assert!(error.contains(ASK_MODEL), "{error}");
    assert!(error.contains("not available"), "{error}");
}

#[test]
fn editing_needs_exactly_one_marked_photo() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let first = library.photo("Rome/one.jpg");
    let second = library.photo("Rome/two.jpg");
    install_openrouter_stub(OpenRouterStub::single(
        200,
        text_response(r#"{"edit":"Blur the background."}"#),
    ));

    let error = ask(
        "blur the background",
        &[first, second],
        library.root(),
        stub_openrouter_post,
    )
    .unwrap_err();

    assert_eq!(error, image_edit::edit_needs_one_photo_message());
}

#[test]
fn a_timed_out_edit_leaves_no_partial_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    fn slow_edit_post(
        url: &str,
        api_key: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        if is_ask_request(url, body) {
            Ok((
                200,
                text_response(r#"{"edit":"Blur the background."}"#).to_string(),
            ))
        } else {
            thread::sleep(Duration::from_secs(2));
            stub_openrouter_post(url, api_key, body, timeout)
        }
    }

    let started = Instant::now();
    let error = ask_with(
        "blur the background",
        &[photo],
        library.root(),
        test_key(),
        Timeouts {
            ask: Duration::from_secs(10),
            edit: Duration::from_millis(300),
        },
        slow_edit_post,
    )
    .unwrap_err();

    assert_eq!(error, "The AI request timed out.");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!library.root().join("Rome/photo-edited.png").exists());
}

#[test]
fn cancelling_stops_an_in_flight_request() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    fn slow_post(
        _url: &str,
        _api_key: &str,
        _body: &Value,
        _timeout: Duration,
    ) -> Result<(u16, String), String> {
        thread::sleep(Duration::from_secs(2));
        Ok((200, text_response("too late").to_string()))
    }

    let mut handle = ai::spawn_with(
        1,
        "which car?".to_string(),
        vec![photo],
        library.root().to_path_buf(),
        test_key(),
        budget(),
        slow_post,
    );
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    handle.cancel();
    let error = settle(&mut handle).unwrap_err();

    assert!(error.contains("cancelled"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn repeated_edits_never_overwrite_an_earlier_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");

    for expected in ["photo-edited.png", "photo-edited-2.png"] {
        let value = ask(
            "blur the background",
            std::slice::from_ref(&photo),
            library.root(),
            |url, key, body, timeout| {
                ask_then_edit_post(
                    r#"{"edit":"Blur the background."}"#,
                    url,
                    key,
                    body,
                    timeout,
                )
            },
        )
        .unwrap();
        let AskValue::Saved(saved) = value else {
            panic!("expected a saved edit");
        };
        assert_eq!(saved.filename, expected);
    }

    assert_eq!(
        library.album("Rome"),
        vec!["photo-edited-2.png", "photo-edited.png", "photo.jpg"]
    );
}

#[test]
fn env_key_401_is_not_overlay_recoverable() {
    let _lock = openrouter_test_lock();
    install_openrouter_stub(OpenRouterStub::single(
        401,
        json!({"error": {"code": 401, "message": "bad"}}),
    ));
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let key = ResolvedKey::new("env-key", CredentialSource::Environment);
    let error = ask_with(
        "which car?",
        &[photo],
        library.root(),
        key,
        budget(),
        stub_openrouter_post,
    )
    .unwrap_err();
    assert!(error.contains("OPENROUTER_API_KEY"), "{error}");
}

#[test]
fn provider_404_429_and_no_image_are_mapped() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");

    install_openrouter_stub(OpenRouterStub::queue(vec![
        (200, text_response(r#"{"edit":"Blur."}"#)),
        (404, json!({"error": {"code": 404, "message": "model"}})),
    ]));
    let err = ask(
        "blur",
        std::slice::from_ref(&photo),
        library.root(),
        stub_openrouter_post,
    )
    .unwrap_err();
    assert!(err.contains(EDIT_MODEL), "{err}");

    install_openrouter_stub(OpenRouterStub::queue(vec![
        (200, text_response(r#"{"edit":"Blur."}"#)),
        (429, json!({"error": {"code": 429, "message": "quota"}})),
    ]));
    let err = ask(
        "blur",
        std::slice::from_ref(&photo),
        library.root(),
        stub_openrouter_post,
    )
    .unwrap_err();
    assert!(err.contains("quota"), "{err}");

    install_openrouter_stub(OpenRouterStub::queue(vec![
        (200, text_response(r#"{"edit":"Blur."}"#)),
        (200, json!({ "data": [] })),
    ]));
    let err = ask(
        "blur",
        std::slice::from_ref(&photo),
        library.root(),
        stub_openrouter_post,
    )
    .unwrap_err();
    assert_eq!(err, "OpenRouter returned no image.");
}

#[test]
fn cancelling_edit_leaves_no_partial_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    fn slow_edit_post(
        url: &str,
        api_key: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        if is_ask_request(url, body) {
            Ok((200, text_response(r#"{"edit":"Blur."}"#).to_string()))
        } else {
            thread::sleep(Duration::from_secs(2));
            stub_openrouter_post(url, api_key, body, timeout)
        }
    }
    let mut handle = ai::spawn_with(
        1,
        "blur".to_string(),
        vec![photo],
        library.root().to_path_buf(),
        test_key(),
        budget(),
        slow_edit_post,
    );
    handle.cancel();
    let error = settle(&mut handle).unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert!(!library.root().join("Rome/photo-edited.png").exists());
}

#[test]
fn openrouter_request_bodies_match_public_helpers() {
    let ask =
        openrouter::ask_request_body("hello", &[("image/jpeg".into(), "abcd".into())], ASK_MODEL);
    assert_eq!(ask["model"], ASK_MODEL);
    let edit = openrouter::edit_request_body("blur", "image/jpeg", "abcd", EDIT_MODEL);
    assert_eq!(edit["model"], EDIT_MODEL);
}

#[test]
fn custom_models_from_credentials_file_are_used_in_requests() {
    let _lock = openrouter_test_lock();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    with_credentials_path(|| {
        let path = std::env::var("HALLWARD_CREDENTIALS_PATH").unwrap();
        fs::write(
            path,
            "OPENROUTER_API_KEY=saved\nASK_MODEL=custom/ask\nEDIT_MODEL=custom/edit\n",
        )
        .unwrap();
        install_openrouter_stub(OpenRouterStub::single(200, text_response("ok")));
        let key = credentials::resolve().unwrap();
        ask_with(
            "what?",
            &[photo],
            library.root(),
            key,
            budget(),
            stub_openrouter_post,
        )
        .unwrap();
        let stub = OPENROUTER_STUB.lock().unwrap();
        assert_eq!(stub.as_ref().unwrap().seen_bodies[0]["model"], "custom/ask");
    });
}
