//! End-to-end Ask AI coverage: a real child process, a real library on disk, a
//! real catalog write. The agent is the `stub_agent` binary, so these tests need
//! no network, no credentials, and no installed CLI.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hallward::ai::{self, AgentCli, AskHandle, AskValue, Timeouts};
use hallward::catalog;
use hallward::credentials::{self, CredentialSource, ResolvedKey};
use hallward::image_edit::{self, PostGeminiFn, GEMINI_EDIT_MODEL};
use hallward::index;
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

/// A scripted agent: a scenario file plus a wrapper that runs the stub with it.
struct Stub {
    dir: tempfile::TempDir,
    program: PathBuf,
}

impl Stub {
    fn new(mut scenario: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        scenario["argv_log"] = json!(dir.path().join("argv.log"));
        let scenario_path = dir.path().join("scenario.json");
        fs::write(&scenario_path, scenario.to_string()).unwrap();

        let program = dir.path().join("stub-agent");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nSTUB_AGENT_SCENARIO='{}' exec '{}' \"$@\"\n",
                scenario_path.display(),
                env!("CARGO_BIN_EXE_stub_agent"),
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, program }
    }

    fn cli(&self, agent: &str) -> AgentCli {
        AgentCli::with_program(agent, &self.program)
    }

    fn calls(&self) -> Vec<Value> {
        let log = self.dir.path().join("argv.log");
        let Ok(text) = fs::read_to_string(log) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn call(&self, name: &str) -> Value {
        self.calls()
            .into_iter()
            .find(|record| record["call"] == name)
            .unwrap_or_else(|| panic!("stub agent was never called for {name}"))
    }
}

fn missing_program(dir: &Path) -> AgentCli {
    AgentCli::with_program("opencode", dir.join("definitely-not-installed"))
}

fn budget() -> Timeouts {
    Timeouts {
        ask: Duration::from_secs(10),
        edit: Duration::from_secs(10),
    }
}

fn ask(cli: AgentCli, prompt: &str, files: &[PathBuf], root: &Path) -> Result<AskValue, String> {
    ask_with(cli, prompt, files, root, budget())
}

fn ask_with(
    cli: AgentCli,
    prompt: &str,
    files: &[PathBuf],
    root: &Path,
    timeouts: Timeouts,
) -> Result<AskValue, String> {
    let mut handle = ai::spawn_with(
        1,
        cli,
        prompt.to_string(),
        files.to_vec(),
        root.to_path_buf(),
        timeouts,
    );
    settle(&mut handle)
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

fn argv(record: &Value) -> Vec<String> {
    record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect()
}

fn with_credentials_path<F: FnOnce()>(f: F) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let prev = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
    let prev_key = std::env::var_os("GEMINI_API_KEY");
    std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
    std::env::remove_var("GEMINI_API_KEY");
    f();
    match prev {
        Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
        None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
    }
    match prev_key {
        Some(value) => std::env::set_var("GEMINI_API_KEY", value),
        None => std::env::remove_var("GEMINI_API_KEY"),
    }
}

static GEMINI_STUB: Mutex<Option<GeminiStub>> = Mutex::new(None);

/// Serializes tests that share the global `GEMINI_STUB` so parallel runs
/// don't overwrite each other's installed response.
static GEMINI_TEST_LOCK: Mutex<()> = Mutex::new(());

struct GeminiStub {
    status: u16,
    body: String,
    seen_url: Option<String>,
    seen_key: Option<String>,
    seen_body: Option<Value>,
}

fn install_gemini_stub(status: u16, body: Value) {
    *GEMINI_STUB.lock().unwrap() = Some(GeminiStub {
        status,
        body: body.to_string(),
        seen_url: None,
        seen_key: None,
        seen_body: None,
    });
}

fn stub_gemini_post(
    url: &str,
    api_key: &str,
    body: &Value,
    _timeout: Duration,
) -> Result<(u16, String), String> {
    let mut guard = GEMINI_STUB.lock().unwrap();
    let stub = guard.as_mut().expect("gemini stub not installed");
    stub.seen_url = Some(url.to_string());
    stub.seen_key = Some(api_key.to_string());
    stub.seen_body = Some(body.clone());
    Ok((stub.status, stub.body.clone()))
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
        "candidates": [{
            "content": {
                "parts": [{ "inlineData": { "mimeType": "image/png", "data": data } }]
            }
        }]
    })
}

fn edit_with_stub(
    instruction: &str,
    source: PathBuf,
    root: &Path,
    post: PostGeminiFn,
) -> Result<AskValue, String> {
    let key = ResolvedKey {
        key: "test-key".into(),
        source: CredentialSource::File,
    };
    let mut handle = ai::spawn_edit_with(
        1,
        instruction.to_string(),
        source,
        root.to_path_buf(),
        key,
        budget(),
        post,
    );
    settle(&mut handle)
}

#[test]
fn a_plain_answer_reaches_the_caller() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({ "ask": { "stdout": "A red Ferrari." } }));

    let value = ask(stub.cli("opencode"), "which car?", &[photo], library.root());

    assert_eq!(value.unwrap(), AskValue::Answer("A red Ferrari.".into()));
}

#[test]
fn agent_chrome_above_the_answer_is_dropped() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "> build \u{b7} kimi-k2.7-code\n\nA red Ferrari." }
    }));

    let value = ask(stub.cli("opencode"), "which car?", &[photo], library.root());

    assert_eq!(value.unwrap(), AskValue::Answer("A red Ferrari.".into()));
}

#[test]
fn the_agent_only_sees_a_stripped_jpeg_preview() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({ "ask": { "stdout": "ok" } }));

    ask(
        stub.cli("opencode"),
        "what is this?",
        &[photo],
        library.root(),
    )
    .unwrap();

    let argv = argv(&stub.call("ask"));
    assert!(
        argv.iter()
            .any(|arg| arg.ends_with("image-000.jpg") && Path::new(arg).is_absolute()),
        "OpenCode must receive an absolute preview path: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg.contains("photo.jpg")),
        "the original path must never reach the agent: {argv:?}"
    );
}

#[test]
fn an_edit_directive_stops_at_classification() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Remove the people in the background.\"}" }
    }));

    let value = ask(
        stub.cli("opencode"),
        "remove the people in the background",
        std::slice::from_ref(&photo),
        library.root(),
    )
    .unwrap();

    assert_eq!(
        value,
        AskValue::Edit {
            instruction: "Remove the people in the background.".into()
        }
    );
    assert_eq!(stub.calls().len(), 1);
    assert!(!library.root().join("Rome/photo-edited.png").exists());
}

#[test]
fn gemini_edit_writes_indexes_and_reports_a_sibling() {
    let _lock = GEMINI_TEST_LOCK.lock().unwrap();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    install_gemini_stub(200, png_response());

    let value = edit_with_stub(
        "Remove the people in the background.",
        photo.clone(),
        library.root(),
        stub_gemini_post,
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
    assert_eq!(image::image_dimensions(&photo).unwrap(), (48, 32));

    let seen = GEMINI_STUB.lock().unwrap();
    let stub = seen.as_ref().unwrap();
    assert!(stub.seen_url.as_ref().unwrap().contains(GEMINI_EDIT_MODEL));
    assert_eq!(stub.seen_key.as_deref(), Some("test-key"));
    assert_eq!(
        stub.seen_body.as_ref().unwrap()["generationConfig"]["responseModalities"],
        json!(["TEXT", "IMAGE"])
    );
}

#[test]
fn classify_keeps_the_agents_default_model() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" }
    }));

    ask(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
    )
    .unwrap();

    let ask_argv = argv(&stub.call("ask"));
    assert!(
        !ask_argv.iter().any(|arg| arg == "--model"),
        "Q&A must keep the agent's default model: {ask_argv:?}"
    );
    assert_eq!(stub.calls().len(), 1);
}

#[test]
fn an_auth_failure_becomes_a_sign_in_hint() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stderr": "AI_APICallError: No auth credentials found", "exit": 1 }
    }));

    let error = ask(stub.cli("opencode"), "which car?", &[photo], library.root()).unwrap_err();

    assert_eq!(error, ai::sign_in_hint("opencode"));
}

#[test]
fn a_provider_error_is_surfaced_verbatim() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": {
            "stderr": "Upstream request failed: [NOT_FOUND] Model not found, inaccessible, and/or not deployed",
            "exit": 1
        }
    }));

    let error = ask(stub.cli("opencode"), "which car?", &[photo], library.root()).unwrap_err();

    // The original provider error must reach the user verbatim so they can
    // see which model failed — a generic "not available" message hides the cause.
    assert!(
        error.contains("[NOT_FOUND]"),
        "provider error must be surfaced verbatim: {error}"
    );
    assert!(
        error.contains("Model not found"),
        "provider error must be surfaced verbatim: {error}"
    );
    assert!(
        !error.contains("not available"),
        "the generic mapped message must not hide the provider error: {error}"
    );
}

#[test]
fn editing_needs_exactly_one_marked_photo() {
    let library = Library::new();
    let first = library.photo("Rome/one.jpg");
    let second = library.photo("Rome/two.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" }
    }));

    let error = ask(
        stub.cli("opencode"),
        "blur the background",
        &[first, second],
        library.root(),
    )
    .unwrap_err();

    assert_eq!(error, image_edit::edit_needs_one_photo_message());
}

#[test]
fn a_timed_out_gemini_edit_leaves_no_partial_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    fn slow_post(
        _url: &str,
        _api_key: &str,
        _body: &Value,
        _timeout: Duration,
    ) -> Result<(u16, String), String> {
        thread::sleep(Duration::from_secs(2));
        Ok((200, png_response().to_string()))
    }

    let started = Instant::now();
    let error = {
        let key = ResolvedKey {
            key: "test-key".into(),
            source: CredentialSource::File,
        };
        let mut handle = ai::spawn_edit_with(
            1,
            "Blur the background.".to_string(),
            photo,
            library.root().to_path_buf(),
            key,
            Timeouts {
                ask: Duration::from_secs(10),
                edit: Duration::from_millis(300),
            },
            slow_post,
        );
        settle(&mut handle).unwrap_err()
    };

    assert_eq!(error, "The AI request timed out.");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!library.root().join("Rome/photo-edited.png").exists());
}

#[test]
fn cancelling_stops_an_in_flight_request() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "sleep_ms": 30_000, "stdout": "too late" }
    }));

    let mut handle = ai::spawn_with(
        1,
        stub.cli("opencode"),
        "which car?".to_string(),
        vec![photo],
        library.root().to_path_buf(),
        budget(),
    );
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    handle.cancel();
    let error = settle(&mut handle).unwrap_err();

    assert!(error.contains("cancelled"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_missing_agent_binary_reports_that_it_is_not_installed() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let dir = tempfile::tempdir().unwrap();

    let error = ask(
        missing_program(dir.path()),
        "which car?",
        &[photo],
        library.root(),
    )
    .unwrap_err();

    assert!(error.contains("not installed"), "{error}");
}

#[test]
fn repeated_edits_never_overwrite_an_earlier_sibling() {
    let _lock = GEMINI_TEST_LOCK.lock().unwrap();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");

    for expected in ["photo-edited.png", "photo-edited-2.png"] {
        install_gemini_stub(200, png_response());
        let value = edit_with_stub(
            "Blur the background.",
            photo.clone(),
            library.root(),
            stub_gemini_post,
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
fn gemini_401_for_saved_key_reopens_overlay() {
    let _lock = GEMINI_TEST_LOCK.lock().unwrap();
    install_gemini_stub(
        401,
        json!({"error": {"code": 401, "status": "UNAUTHENTICATED", "message": "bad"}}),
    );
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    with_credentials_path(|| {
        credentials::save_gemini_key("saved-key").unwrap();
        let error = edit_with_stub(
            "Blur the background.",
            photo,
            library.root(),
            stub_gemini_post,
        )
        .unwrap_err();
        assert_eq!(error, credentials::INVALID_SAVED_KEY);
        assert!(!credentials::resolve().is_some());
    });
}

#[test]
fn gemini_401_for_env_key_is_not_overlay_recoverable() {
    let _lock = GEMINI_TEST_LOCK.lock().unwrap();
    install_gemini_stub(
        401,
        json!({"error": {"code": 401, "status": "UNAUTHENTICATED", "message": "bad"}}),
    );
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let key = ResolvedKey {
        key: "env-key".into(),
        source: CredentialSource::Environment,
    };
    let mut handle = ai::spawn_edit_with(
        1,
        "Blur the background.".to_string(),
        photo,
        library.root().to_path_buf(),
        key,
        budget(),
        stub_gemini_post,
    );
    let error = settle(&mut handle).unwrap_err();
    assert!(error.contains("GEMINI_API_KEY"), "{error}");
}

#[test]
fn gemini_404_429_and_no_image_are_mapped() {
    let _lock = GEMINI_TEST_LOCK.lock().unwrap();
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");

    install_gemini_stub(
        404,
        json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "model"}}),
    );
    let err = edit_with_stub("edit", photo.clone(), library.root(), stub_gemini_post).unwrap_err();
    assert!(err.contains(GEMINI_EDIT_MODEL), "{err}");

    install_gemini_stub(
        429,
        json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota"}}),
    );
    let err = edit_with_stub("edit", photo.clone(), library.root(), stub_gemini_post).unwrap_err();
    assert!(err.contains("quota"), "{err}");

    install_gemini_stub(
        200,
        json!({"candidates": [{ "content": { "parts": [{ "text": "no image" }] } }]}),
    );
    let err = edit_with_stub("edit", photo, library.root(), stub_gemini_post).unwrap_err();
    assert_eq!(err, "Gemini returned no image.");
}

#[test]
fn cancelling_gemini_edit_leaves_no_partial_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    fn slow_post(
        _url: &str,
        _api_key: &str,
        _body: &Value,
        _timeout: Duration,
    ) -> Result<(u16, String), String> {
        thread::sleep(Duration::from_secs(2));
        Ok((200, png_response().to_string()))
    }
    let key = ResolvedKey {
        key: "test-key".into(),
        source: CredentialSource::File,
    };
    let mut handle = ai::spawn_edit_with(
        1,
        "Blur.".to_string(),
        photo,
        library.root().to_path_buf(),
        key,
        budget(),
        slow_post,
    );
    handle.cancel();
    let error = settle(&mut handle).unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert!(!library.root().join("Rome/photo-edited.png").exists());
}
