//! Reusable headless e2e sandbox for the Hallward TUI.
//!
//! A test builds a [`Fixture`] (tempdir library with generated images),
//! wraps it in a [`Harness`] (real `App` + `TestBackend` driver +
//! [`FakeViewerOpener`](hallward::tui::FakeViewerOpener) + fixed clock +
//! stubbed OpenRouter transport), scripts keys/mouse, and asserts semantic
//! state (focus, marks, clipboard, catalog rows, filesystem) plus a few
//! targeted buffer queries. No network, no real viewer, no tokens spent.
//!
//! Tools (`ffmpeg`/`heif-convert`) are intentionally *not* stubbed: still-image
//! flows are hermetic, while video/HEIC flows early-return via
//! [`tools_available`] when the binaries are missing (e.g. CI).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hallward::catalog;
use hallward::credentials;
use hallward::index;
use hallward::media;
use hallward::tui::{App, FakeViewerOpener, FixedClock, TestDriver};
use image::{Rgb, RgbImage};
use serde_json::{json, Value};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Fixture: tempdir library with generated images
// ---------------------------------------------------------------------------

/// A library root with a catalog, plus helpers to add indexed photos.
///
/// Images are generated with the `image` crate (no committed binaries, no
/// `ffmpeg` needed for JPEG/PNG). Each photo is indexed like production
/// (`index_new_file`) so catalog assertions match real behavior.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        // NOTE: `tempfile::tempdir()` names dirs `.tmpXXXX` (hidden), and
        // `index_library` prunes hidden dirs during its walk. The prefix
        // keeps the root visible so full reindexes behave like production
        // (`~/Pictures/...`).
        let dir = tempfile::Builder::new()
            .prefix("hallward-e2e")
            .tempdir()
            .expect("tempdir for fixture library");
        catalog::open(dir.path(), true).expect("create fixture catalog");
        Self { dir }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Write a generated still at `rel` (parent dirs created) and index it.
    /// The pixel color varies with the name so thumbs are not all identical.
    pub fn photo(&self, rel: &str) -> PathBuf {
        let abs = self.root().join(rel);
        fs::create_dir_all(abs.parent().expect("photo parent")).expect("mkdirs");
        let hash: u8 = rel.bytes().fold(0u8, |acc, b| acc.wrapping_add(b));
        RgbImage::from_pixel(48, 32, Rgb([hash, 100, 200]))
            .save(&abs)
            .expect("write fixture photo");
        index::index_new_file(self.root(), &abs, Some("2024:01:02 03:04:05"))
            .expect("index fixture photo");
        abs
    }

    /// Filenames in `album`, in catalog order.
    pub fn album_filenames(&self, album: &str) -> Vec<String> {
        let conn = catalog::open(self.root(), false).expect("open catalog");
        catalog::photos_in_album(&conn, album)
            .expect("read album")
            .into_iter()
            .map(|photo| photo.filename)
            .collect()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.root().join(rel).exists()
    }
}

// ---------------------------------------------------------------------------
// Credentials isolation (never touch the developer's real key)
// ---------------------------------------------------------------------------

/// Points `HALLWARD_CREDENTIALS_PATH` at a tempdir and strips
/// `OPENROUTER_API_KEY` (same pattern as
/// `tests/ask_ai.rs`). Restores everything on drop.
pub struct CredGuard {
    prev_path: Option<std::ffi::OsString>,
    prev_openrouter: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl CredGuard {
    pub fn isolated() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for credentials");
        let path = dir.path().join("credentials");
        let prev_path = std::env::var_os("HALLWARD_CREDENTIALS_PATH");
        let prev_openrouter = std::env::var_os("OPENROUTER_API_KEY");
        std::env::set_var("HALLWARD_CREDENTIALS_PATH", &path);
        std::env::remove_var("OPENROUTER_API_KEY");
        Self {
            prev_path,
            prev_openrouter,
            _dir: dir,
        }
    }

    pub fn save_key(&self, key: &str) {
        credentials::save_api_key(key).expect("save test key");
    }
}

impl Drop for CredGuard {
    fn drop(&mut self) {
        match self.prev_path.take() {
            Some(value) => std::env::set_var("HALLWARD_CREDENTIALS_PATH", value),
            None => std::env::remove_var("HALLWARD_CREDENTIALS_PATH"),
        }
        match self.prev_openrouter.take() {
            Some(value) => std::env::set_var("OPENROUTER_API_KEY", value),
            None => std::env::remove_var("OPENROUTER_API_KEY"),
        }
    }
}

// ---------------------------------------------------------------------------
// Stubbed OpenRouter transport (no network, no tokens)
// ---------------------------------------------------------------------------

static STUB: Mutex<Option<StubInner>> = Mutex::new(None);

/// Serializes tests sharing the global stub so parallel runs don't clobber
/// each other's queued responses.
static STUB_LOCK: Mutex<()> = Mutex::new(());

struct StubInner {
    responses: Vec<(u16, String)>,
    seen_urls: Vec<String>,
    seen_keys: Vec<String>,
    seen_bodies: Vec<Value>,
}

impl StubInner {
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

pub fn stub_lock() -> MutexGuard<'static, ()> {
    STUB_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn install_text_response(body: Value) {
    install_queue(vec![(200, body)]);
}

pub fn install_queue(status_bodies: Vec<(u16, Value)>) {
    *STUB.lock().unwrap_or_else(|p| p.into_inner()) = Some(StubInner::queue(status_bodies));
}

pub fn seen_bodies() -> Vec<Value> {
    STUB.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|stub| stub.seen_bodies.clone())
        .unwrap_or_default()
}

pub fn seen_urls() -> Vec<String> {
    STUB.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|stub| stub.seen_urls.clone())
        .unwrap_or_default()
}

/// Injectable `PostFn`: serves queued responses, records requests.
pub fn stub_post(
    url: &str,
    api_key: &str,
    body: &Value,
    _timeout: Duration,
) -> Result<(u16, String), String> {
    let mut guard = STUB.lock().unwrap_or_else(|p| p.into_inner());
    let stub = guard.as_mut().expect("openrouter stub not installed");
    stub.seen_urls.push(url.to_string());
    stub.seen_keys.push(api_key.to_string());
    stub.seen_bodies.push(body.clone());
    let (status, text) = stub.responses.remove(0);
    Ok((status, text))
}

pub fn text_response(text: &str) -> Value {
    json!({
        "choices": [{ "message": { "content": text } }]
    })
}

pub fn png_response() -> Value {
    let png = RgbImage::from_pixel(24, 16, Rgb([90, 140, 200]));
    let mut bytes = Vec::new();
    png.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .expect("encode stub png");
    let data = base64_stub_encode(&bytes);
    json!({
        "data": [{ "b64_json": data }]
    })
}

fn base64_stub_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    BASE64.encode(bytes)
}

/// Transport answering "ask" with a fixed edit directive and edits with a
/// stub PNG. Pass to [`Harness::with_transport`] for edit flows without
/// queueing individual responses.
pub fn ask_then_edit_post(
    url: &str,
    api_key: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, String), String> {
    if body.get("model") == Some(&json!(hallward::image_edit::ASK_MODEL)) {
        Ok((
            200,
            text_response(r#"{"edit":"Blur the background."}"#).to_string(),
        ))
    } else if url == hallward::openrouter::IMAGES_URL
        || body.get("model") == Some(&json!(hallward::image_edit::EDIT_MODEL))
    {
        Ok((200, png_response().to_string()))
    } else {
        stub_post(url, api_key, body, timeout)
    }
}

// ---------------------------------------------------------------------------
// Tool availability: video/HEIC tests need real binaries; skip without them
// ---------------------------------------------------------------------------

/// True when `ffmpeg`, `ffprobe`, and `heif-convert` are all on PATH.
/// Still-image flows don't need these and always run.
pub fn tools_available() -> bool {
    media::bin_on_path("ffmpeg")
        && media::bin_on_path("ffprobe")
        && media::bin_on_path("heif-convert")
}

pub fn has_tool(bin: &str) -> bool {
    media::bin_on_path(bin)
}

// ---------------------------------------------------------------------------
// Harness: App + TestBackend driver + fakes, ready to script
// ---------------------------------------------------------------------------

/// The reusable sandbox: a real `App` over a tempdir fixture, drawn into a
/// `TestBackend` terminal, with viewer/clock/Ask transport faked.
///
/// Key rules: call [`Harness::redraw`] before asserting screen text or
/// clicking (hit regions are populated during draw); use [`Harness::settle_ask`]
/// to pump background Ask AI jobs to completion.
pub struct Harness {
    pub app: App,
    pub driver: TestDriver,
    pub viewer: Arc<FakeViewerOpener>,
    pub clock: Arc<FixedClock>,
    pub fixture: Fixture,
    _cred: CredGuard,
}

impl Harness {
    /// Build over an existing fixture, saving `key` as the credentials-file
    /// key and serving `transport` for Ask AI requests.
    pub fn with_transport(fixture: Fixture, transport: hallward::openrouter::PostFn) -> Self {
        let cred = CredGuard::isolated();
        cred.save_key("test-key");
        let viewer = Arc::new(FakeViewerOpener::new());
        let clock = Arc::new(FixedClock::new(Instant::now()));
        let root = fixture.root().to_path_buf();
        let app = App::new_for_test(root, viewer.clone(), clock.clone(), transport)
            .expect("App::new_for_test");
        let driver = TestDriver::new(120, 40);
        let mut harness = Self {
            app,
            driver,
            viewer,
            clock,
            fixture,
            _cred: cred,
        };
        harness.redraw();
        harness
    }

    /// Default harness: Ask AI answers `"ok"` to everything.
    pub fn new(fixture: Fixture) -> Self {
        install_text_response(text_response("ok"));
        Self::with_transport(fixture, stub_post)
    }

    /// Render the current state into the `TestBackend` buffer.
    pub fn redraw(&mut self) {
        self.app.redraw_for_test(&mut self.driver).expect("redraw");
    }

    /// Press one key through production `handle_key`. Returns `true` on quit.
    pub fn key(&mut self, code: KeyCode) -> bool {
        self.key_ev(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Press a fully-specified key event (e.g. BackTab, Shift+Tab).
    pub fn key_ev(&mut self, key: KeyEvent) -> bool {
        let quit = self
            .app
            .test_key(key, &mut self.driver)
            .expect("handle key");
        self.redraw();
        quit
    }

    pub fn shift_tab(&mut self) {
        self.key(KeyCode::BackTab);
    }

    /// Type text char-by-char through production key handling.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch));
        }
    }

    /// Click the grid cell showing photo `idx` (redraws first).
    /// Returns `false` when the photo is not on screen.
    pub fn click_grid(&mut self, idx: usize) -> bool {
        self.redraw();
        let clicked = self
            .app
            .click_grid_photo(idx, &mut self.driver)
            .expect("click grid");
        self.redraw();
        clicked
    }

    /// Full screen text for targeted buffer queries (prefer semantic
    /// accessors on `app` for most assertions).
    pub fn screen(&self) -> String {
        self.driver.screen_text()
    }

    /// Pump the background Ask AI job until a reply appears or `timeout`
    /// elapses. Redraws before returning so `screen()` shows the reply.
    /// Returns the reply text, if any.
    pub fn settle_ask(&mut self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.app.pump_ask();
            if let Some(reply) = self.app.ask_reply_text() {
                self.redraw();
                return Some(reply);
            }
            if Instant::now() >= deadline {
                self.redraw();
                return self.app.ask_reply_text();
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn root(&self) -> &Path {
        self.fixture.root()
    }
}
