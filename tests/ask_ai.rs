//! End-to-end Ask AI coverage: a real child process, a real library on disk, a
//! real catalog write. The agent is the `stub_agent` binary, so these tests need
//! no network, no credentials, and no installed CLI.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use hallward::ai::{self, AgentCli, AskHandle, AskValue, Timeouts};
use hallward::catalog;
use hallward::image_edit;
use hallward::index;
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
        image::RgbImage::from_pixel(48, 32, image::Rgb([12, 34, 56]))
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
///
/// The scenario travels via a per-test wrapper script rather than the ambient
/// environment, so tests stay isolated under cargo's parallel harness.
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

    /// One record per invocation of the stub, in call order.
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

/// Short enough that a wedged test fails fast, long enough for a real spawn.
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
    let dir_at = argv.iter().position(|arg| arg == "--dir").unwrap();
    assert_eq!(
        fs::canonicalize(&argv[dir_at + 1]).unwrap(),
        fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
        "OpenCode --dir must stay the Hallward process cwd so it keeps the user's model"
    );
}

#[test]
fn an_edit_directive_writes_indexes_and_reports_a_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Remove the people in the background.\"}" },
        "edit": { "write_png": true, "stdout": "photo-edited.png" }
    }));

    let value = ask(
        stub.cli("opencode"),
        "remove the people in the background",
        std::slice::from_ref(&photo),
        library.root(),
    );

    let AskValue::Saved(saved) = value.unwrap() else {
        panic!("expected a saved edit");
    };
    assert_eq!(saved.filename, "photo-edited.png");
    assert_eq!(saved.relpath, "Rome/photo-edited.png");
    assert!(library.root().join("Rome/photo-edited.png").is_file());
    assert_eq!(library.album("Rome"), vec!["photo-edited.png", "photo.jpg"]);
    assert_eq!(
        library.captured_at("Rome", "photo-edited.png"),
        library.captured_at("Rome", "photo.jpg"),
        "the sibling inherits the capture date so it sorts beside the original"
    );
    assert_eq!(
        image::image_dimensions(&photo).unwrap(),
        (48, 32),
        "the original must not be overwritten"
    );
}

#[test]
fn the_edit_run_keeps_the_users_configured_model() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "write_png": true, "stdout": "photo-edited.png" }
    }));

    ask(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
    )
    .unwrap();

    for call in stub.calls() {
        let argv = argv(&call);
        assert!(
            !argv.iter().any(|arg| arg == "--model"),
            "Hallward must not override the agent's model: {argv:?}"
        );
    }
}

#[test]
fn the_edit_run_is_the_only_one_allowed_to_write() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "write_png": true, "stdout": "photo-edited.png" }
    }));

    ask(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
    )
    .unwrap();

    assert_eq!(
        stub.call("ask")["opencode_permission"],
        json!("{\"*\":\"deny\"}")
    );
    assert_eq!(
        stub.call("edit")["opencode_permission"],
        json!("{\"*\":\"allow\"}")
    );
}

#[test]
fn the_edit_run_works_in_the_album_the_photo_lives_in() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "write_png": true, "stdout": "photo-edited.png" }
    }));

    ask(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
    )
    .unwrap();

    let edit = stub.call("edit");
    let cwd = PathBuf::from(edit["cwd"].as_str().unwrap());
    assert_eq!(
        fs::canonicalize(cwd).unwrap(),
        fs::canonicalize(library.root().join("Rome")).unwrap()
    );
    let argv = argv(&edit);
    let dir_at = argv.iter().position(|arg| arg == "--dir").unwrap();
    let project = fs::canonicalize(&argv[dir_at + 1]).unwrap();
    assert_ne!(
        project,
        fs::canonicalize(library.root().join("Rome")).unwrap(),
        "OpenCode --dir is the project for model selection, not the album"
    );
    assert!(
        argv.iter()
            .any(|arg| arg.ends_with("photo.jpg") && Path::new(arg).is_absolute()),
        "edit must attach the original still by absolute path: {argv:?}"
    );
}

#[test]
fn an_agent_that_saves_nothing_says_so() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "stdout": "I cannot generate images." }
    }));

    let error = ask(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
    )
    .unwrap_err();

    assert_eq!(error, image_edit::no_saved_image_message("OpenCode"));
    assert!(!library.root().join("Rome/photo-edited.png").exists());
    assert_eq!(library.album("Rome"), vec!["photo.jpg"]);
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
            "stderr": "Upstream request failed: [NOT_FOUND] Model not found",
            "exit": 1
        }
    }));

    let error = ask(stub.cli("opencode"), "which car?", &[photo], library.root()).unwrap_err();

    assert!(error.contains("[NOT_FOUND] Model not found"), "{error}");
}

#[test]
fn editing_needs_exactly_one_marked_photo() {
    let library = Library::new();
    let first = library.photo("Rome/one.jpg");
    let second = library.photo("Rome/two.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "write_png": true }
    }));

    let error = ask(
        stub.cli("opencode"),
        "blur the background",
        &[first, second],
        library.root(),
    )
    .unwrap_err();

    assert_eq!(error, image_edit::edit_needs_one_photo_message());
    assert!(stub.calls().iter().all(|call| call["call"] == "ask"));
}

#[test]
fn a_timed_out_edit_leaves_no_partial_sibling() {
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let stub = Stub::new(json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "sleep_ms": 30_000, "write_png": true }
    }));

    let started = Instant::now();
    let error = ask_with(
        stub.cli("opencode"),
        "blur the background",
        &[photo],
        library.root(),
        Timeouts {
            ask: Duration::from_secs(10),
            edit: Duration::from_millis(300),
        },
    )
    .unwrap_err();

    assert_eq!(error, "The AI request timed out.");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!library.root().join("Rome/photo-edited.png").exists());
    assert_eq!(library.album("Rome"), vec!["photo.jpg"]);
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
    let library = Library::new();
    let photo = library.photo("Rome/photo.jpg");
    let scenario = json!({
        "ask": { "stdout": "{\"edit\":\"Blur the background.\"}" },
        "edit": { "write_png": true, "stdout": "saved" }
    });

    for expected in ["photo-edited.png", "photo-edited-2.png"] {
        let stub = Stub::new(scenario.clone());
        let value = ask(
            stub.cli("opencode"),
            "blur the background",
            std::slice::from_ref(&photo),
            library.root(),
        );
        let AskValue::Saved(saved) = value.unwrap() else {
            panic!("expected a saved edit");
        };
        assert_eq!(saved.filename, expected);
    }

    assert_eq!(
        library.album("Rome"),
        vec!["photo-edited-2.png", "photo-edited.png", "photo.jpg"]
    );
}
