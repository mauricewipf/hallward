//! Headless end-to-end coverage for the Hallward TUI: real `App`, real
//! catalog + filesystem, scripted keys/mouse through production handlers,
//! stubbed viewer/clock/OpenRouter. No network, no tokens, no GUI spawns.
//!
//! Run: `cargo test --test e2e_tui`
//! Add a scenario: fixture photos -> `Harness` -> script keys -> assert
//! semantic state (`app.*`) + filesystem/catalog + `FakeViewerOpener` calls.
//! Prefer semantic asserts; use `h.screen()` only for spot buffer queries.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{
    ask_then_edit_post, install_queue, install_text_response, png_response, stub_lock, stub_post,
    text_response, Fixture, Harness,
};
use crossterm::event::KeyCode;

fn album_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.photo("2025/Rome/a.jpg");
    fixture.photo("2025/Rome/b.jpg");
    fixture.photo("2025/Rome/c.jpg");
    fixture.photo("2025/Sopron/d.jpg");
    fixture.photo("2025/Sopron/e.jpg");
    fixture
}

/// Miller-drill into the Rome album grid: 2025 -> Rome -> grid.
fn open_rome(h: &mut Harness) {
    assert_eq!(h.app.focus_name(), "miller");
    h.key(KeyCode::Right);
    h.key(KeyCode::Right);
    assert_eq!(h.app.focus_name(), "grid");
    assert_eq!(h.app.current_album_name().as_deref(), Some("Rome"));
    assert_eq!(h.app.photo_count(), 3);
}

fn filenames(h: &Harness, album: &str) -> Vec<String> {
    h.fixture.album_filenames(album)
}

#[test]
fn browse_mark_and_open_viewer_with_start_index() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());

    // Spot buffer query: idle search pane title before anything is marked.
    assert!(h.screen().contains("Search"));
    open_rome(&mut h);

    // Footer shows position while the grid has focus.
    assert!(h.screen().contains("1/3"));

    // Arrow across the row, Space marks the focused still.
    h.key(KeyCode::Right);
    assert_eq!(h.app.grid_index(), 1);
    h.key(KeyCode::Char(' '));
    assert!(h.app.is_marked("2025/Rome/b.jpg"));
    assert!(h.app.is_ask_active());
    // Ask AI pane takes over once a still is marked.
    assert!(h.screen().contains("Ask AI"));

    // Enter opens marked photos in album order, starting at the focused one.
    h.key(KeyCode::Enter);
    let calls = h.viewer.calls();
    assert_eq!(calls.len(), 1);
    let (files, start) = &calls[0];
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec!["b.jpg"]);
    assert_eq!(*start, 0);
}

#[test]
fn grid_click_unmarks_and_double_click_opens_viewer() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());
    open_rome(&mut h);

    // Mark via keyboard, then single-click the same cell to unmark it.
    h.key(KeyCode::Char(' '));
    assert!(h.app.is_marked("2025/Rome/a.jpg"));
    assert!(h.click_grid(0));
    assert!(!h.app.is_marked("2025/Rome/a.jpg"));

    // Click again immediately: the fixed test clock is frozen, so this is a
    // deterministic double-click -> viewer opens, mark restored for open.
    assert!(h.click_grid(0));
    assert_eq!(h.viewer.len(), 1);
    assert!(h.app.is_marked("2025/Rome/a.jpg"));
}

#[test]
fn esc_clears_marks() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    assert!(!h.app.marked_sorted().is_empty());
    h.key(KeyCode::Esc);
    assert!(h.app.marked_sorted().is_empty());
    assert!(!h.app.is_ask_active());
}

#[test]
fn copy_paste_between_albums_keeps_source() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.key(KeyCode::Char('c'));
    assert_eq!(h.app.clipboard_rels(), vec!["2025/Rome/a.jpg"]);
    assert!(h.app.status_text().contains("cop"));

    // Left from the first grid cell returns to the tree; Down selects
    // Sopron; Right enters its grid.
    h.key(KeyCode::Left);
    assert_eq!(h.app.focus_name(), "miller");
    h.key(KeyCode::Down);
    h.key(KeyCode::Right);
    assert_eq!(h.app.current_album_name().as_deref(), Some("Sopron"));

    h.key(KeyCode::Char('p'));
    assert!(h.fixture.exists("2025/Sopron/a.jpg"));
    assert!(h.fixture.exists("2025/Rome/a.jpg"));
    assert_eq!(
        filenames(&h, "2025/Sopron"),
        vec!["a.jpg", "d.jpg", "e.jpg"]
    );
}

#[test]
fn cut_paste_moves_file_and_clears_clipboard() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.key(KeyCode::Char('x'));
    assert_eq!(h.app.clipboard_rels(), vec!["2025/Rome/a.jpg"]);

    h.key(KeyCode::Left);
    h.key(KeyCode::Down);
    h.key(KeyCode::Right);
    h.key(KeyCode::Char('p'));

    assert!(!h.fixture.exists("2025/Rome/a.jpg"));
    assert!(h.fixture.exists("2025/Sopron/a.jpg"));
    assert!(h.app.clipboard_rels().is_empty());
    assert_eq!(filenames(&h, "2025/Rome"), vec!["b.jpg", "c.jpg"]);
}

#[test]
fn delete_with_confirm_unlinks_and_reindexes() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.key(KeyCode::Char('d'));
    assert_eq!(h.app.pending_delete_rels(), vec!["2025/Rome/a.jpg"]);
    assert!(h.screen().contains("Delete"));

    h.key(KeyCode::Char('y'));
    assert!(h.app.pending_delete_rels().is_empty());
    assert!(!h.fixture.exists("2025/Rome/a.jpg"));
    assert_eq!(filenames(&h, "2025/Rome"), vec!["b.jpg", "c.jpg"]);
}

#[test]
fn search_filters_tree_then_esc_clears() {
    let _lock = stub_lock();
    let mut h = Harness::new(album_fixture());

    h.shift_tab();
    assert_eq!(h.app.focus_name(), "search");
    h.type_text("sop");
    assert_eq!(h.app.query_text(), "sop");
    assert!(h.screen().contains("Sopron"));

    h.key(KeyCode::Esc);
    assert_eq!(h.app.query_text(), "");
    assert_eq!(h.app.focus_name(), "miller");
}

#[test]
fn ask_ai_answers_question_without_tokens() {
    let _lock = stub_lock();
    let fixture = album_fixture();
    install_queue(vec![(200, text_response("A red Ferrari."))]);
    let mut h = Harness::with_transport(fixture, stub_post);
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    assert!(h.app.is_ask_active());
    h.type_text("which car?");
    assert_eq!(h.app.ask_prompt_text(), "which car?");

    h.key(KeyCode::Enter);
    let reply = h.settle_ask(Duration::from_secs(15));
    assert_eq!(reply.as_deref(), Some("A red Ferrari."));
    assert!(h.screen().contains("A red Ferrari."));
}

#[test]
fn ask_ai_without_key_shows_setup_command() {
    let _lock = stub_lock();
    let mut h = Harness::without_saved_key(album_fixture());
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.type_text("who is in the photo?");
    h.key(KeyCode::Enter);
    h.redraw();

    let screen = h.screen();
    assert!(screen.contains("RUN:"), "{screen}");
    assert!(
        screen.contains("hallward credentials set OPENROUTER_API_KEY"),
        "{screen}"
    );
    assert!(
        screen.contains("https://openrouter.ai/settings/keys"),
        "{screen}"
    );
    assert!(
        !screen.contains("Or set HALLWARD_OPENROUTER_API_KEY"),
        "{screen}"
    );

    h.key(KeyCode::Esc);
    let screen = h.screen();
    assert!(
        !screen.contains("hallward credentials set OPENROUTER_API_KEY"),
        "{screen}"
    );
}

#[test]
fn ask_ai_edit_saves_sibling_next_to_source() {
    let _lock = stub_lock();
    let fixture = album_fixture();
    install_queue(vec![
        (200, text_response(r#"{"edit":"Blur the background."}"#)),
        (200, png_response()),
    ]);
    // Classifies via the ask model, then edits via the images endpoint.
    let mut h = Harness::with_transport(fixture, stub_post);
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.type_text("blur the background");
    h.key(KeyCode::Enter);
    let reply = h.settle_ask(Duration::from_secs(15)).unwrap_or_default();
    assert!(reply.contains("edited"), "unexpected reply: {reply}");

    assert!(h.fixture.exists("2025/Rome/a-edited.png"));
    let album = filenames(&h, "2025/Rome");
    assert!(album.contains(&"a-edited.png".to_string()));
    assert!(album.contains(&"a.jpg".to_string()));
}

#[test]
fn ask_ai_edit_helper_routes_models_without_tokens() {
    let _lock = stub_lock();
    let fixture = album_fixture();
    install_text_response(text_response("unused"));
    let mut h = Harness::with_transport(fixture, ask_then_edit_post);
    open_rome(&mut h);

    h.key(KeyCode::Char(' '));
    h.type_text("blur the background");
    h.key(KeyCode::Enter);
    let reply = h.settle_ask(Duration::from_secs(15)).unwrap_or_default();
    assert!(reply.contains("edited"), "unexpected reply: {reply}");
    assert!(h.fixture.exists("2025/Rome/a-edited.png"));
}
