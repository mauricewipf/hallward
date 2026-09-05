use std::collections::{HashMap, HashSet};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use rusqlite::Connection;

use crate::ai::{self, AskHandle, Timeouts};
use crate::catalog::{self, Photo};
use crate::clipboard::{self, ClipboardOp};
use crate::credentials::{self, CredentialIssue, ResolvedKey};
use crate::delete;
use crate::image_edit;
use crate::index;
use crate::library::{self, Folder, Kind};
use crate::media::{is_image, is_video};
use crate::openrouter::PostFn;
use crate::search;
use crate::thumbs;
use crate::viewer;

/// Inner image height in rows. Width is derived from the terminal font so the photo is square.
const CELL_INNER_H: u16 = 6;
const STATUS_HINT: &str =
    "arrows move · Space mark · Esc unmark · Enter opens · click toggles mark · double-click opens · Shift+Tab search · c copy · x cut · p paste · d delete · r reindex · q quit";
const DASHED_BORDER: border::Set = border::Set {
    top_left: "┌",
    top_right: "┐",
    bottom_left: "└",
    bottom_right: "┘",
    vertical_left: "╎",
    vertical_right: "╎",
    horizontal_top: "╌",
    horizontal_bottom: "╌",
};
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Opens the external viewer. Production shells out to imv/Preview/mpv;
/// tests record the playlist instead so `cargo test` never spawns a GUI.
pub trait ViewerOpener: Send + Sync {
    fn open(&self, files: &[PathBuf], start: usize) -> Result<()>;
}

/// Production opener: the real `viewer::open` (blocking `Command::status`).
pub struct RealViewerOpener;

impl ViewerOpener for RealViewerOpener {
    fn open(&self, files: &[PathBuf], start: usize) -> Result<()> {
        viewer::open(files, start)
    }
}

/// Test opener: records `(files, start)` calls for assertions.
pub struct FakeViewerOpener {
    calls: Mutex<Vec<(Vec<PathBuf>, usize)>>,
}

impl FakeViewerOpener {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(Vec<PathBuf>, usize)> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn last(&self) -> Option<(Vec<PathBuf>, usize)> {
        self.calls().into_iter().last()
    }

    pub fn len(&self) -> usize {
        self.calls().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for FakeViewerOpener {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewerOpener for FakeViewerOpener {
    fn open(&self, files: &[PathBuf], start: usize) -> Result<()> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((files.to_vec(), start));
        Ok(())
    }
}

/// Clock seam so double-click windows and Ask AI "Waiting..." text are
/// deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock: the real wall time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock: a fixed instant the test can advance manually.
pub struct FixedClock {
    now: Mutex<Instant>,
}

impl FixedClock {
    pub fn new(now: Instant) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn advance(&self, delta: Duration) {
        let mut guard = self.now.lock().unwrap_or_else(|p| p.into_inner());
        *guard = *guard + delta;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Terminal abstraction so the same `App` drives a real Crossterm terminal
/// in production and a `TestBackend` terminal in `cargo test`.
pub trait TerminalDriver {
    /// Render the current `App` state.
    fn redraw(&mut self, app: &mut App) -> Result<()>;
    /// Clear the screen (used after the external viewer returns).
    fn clear_screen(&mut self) -> Result<()>;
    /// Suspend the TUI, run the viewer opener, then restore the TUI.
    /// The test driver just runs the closure without any terminal dance.
    fn suspend_for_viewer(&mut self, run: &mut dyn FnMut() -> Result<()>) -> Result<()>;
}

/// Production driver over a live Crossterm terminal.
pub struct CrosstermDriver<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<Stdout>>,
}

impl<'a> CrosstermDriver<'a> {
    pub fn new(terminal: &'a mut Terminal<CrosstermBackend<Stdout>>) -> Self {
        Self { terminal }
    }
}

impl TerminalDriver for CrosstermDriver<'_> {
    fn redraw(&mut self, app: &mut App) -> Result<()> {
        self.terminal.draw(|f| draw(f, app))?;
        Ok(())
    }

    fn clear_screen(&mut self) -> Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    fn suspend_for_viewer(&mut self, run: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        disable_raw_mode()?;
        leave_tui(self.terminal.backend_mut())?;
        self.terminal.show_cursor()?;
        let result = run();
        enable_raw_mode()?;
        enter_tui(self.terminal.backend_mut())?;
        self.terminal.clear()?;
        result
    }
}

/// Test driver over `ratatui::backend::TestBackend`.
pub struct TestDriver {
    terminal: Terminal<TestBackend>,
}

impl TestDriver {
    pub fn new(width: u16, height: u16) -> Self {
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).expect("TestBackend terminal");
        Self { terminal }
    }

    /// Full screen text, rows joined by `\n`.
    pub fn screen_text(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area;
        let mut rows = Vec::new();
        for y in area.y..area.y + area.height {
            let mut row = String::new();
            for x in area.x..area.x + area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            rows.push(row);
        }
        rows.join("\n")
    }

    /// Text of a single buffer row (trailing spaces trimmed).
    pub fn row_text(&self, y: u16) -> String {
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area;
        let mut row = String::new();
        for x in area.x..area.x + area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        row.trim_end().to_string()
    }

    pub fn size(&self) -> Rect {
        self.terminal.backend().buffer().area
    }
}

impl TerminalDriver for TestDriver {
    fn redraw(&mut self, app: &mut App) -> Result<()> {
        self.terminal.draw(|f| draw(f, app))?;
        Ok(())
    }

    fn clear_screen(&mut self) -> Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    fn suspend_for_viewer(&mut self, run: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        run()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Search,
    Miller,
    Grid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AskReply {
    Text(String),
    Error(String),
}

struct AskAi {
    prompt: String,
    reply: Option<AskReply>,
    waiting_from: Option<Instant>,
    job: Option<AskHandle>,
    generation: u64,
    selection: Vec<String>,
    scroll: u16,
}

/// Ask AI waiting on an OpenRouter API key before the first (or retried) request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingAsk {
    prompt: String,
    files: Vec<PathBuf>,
    generation: u64,
}

struct CredentialsOverlay {
    error: Option<String>,
}

struct CreateFolderOverlay {
    parent_relpath: PathBuf,
    input: String,
    error: Option<String>,
}

impl AskAi {
    fn new() -> Self {
        Self {
            prompt: String::new(),
            reply: None,
            waiting_from: None,
            job: None,
            generation: 0,
            selection: Vec::new(),
            scroll: 0,
        }
    }

    fn waiting(&self) -> bool {
        self.waiting_from.is_some()
    }

    fn cancel_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel();
        }
        self.waiting_from = None;
    }

    fn clear_thread(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel_job();
        self.prompt.clear();
        self.reply = None;
        self.scroll = 0;
    }

    fn second_paragraph(&self, now: Instant) -> Option<String> {
        if let Some(started) = self.waiting_from {
            let phase = self
                .job
                .as_ref()
                .map(|job| job.progress())
                .unwrap_or(ai::AskProgress::Analyzing);
            return Some(ai::waiting_text(phase, started, now));
        }
        match &self.reply {
            Some(AskReply::Text(t) | AskReply::Error(t)) => Some(t.clone()),
            None => None,
        }
    }

    fn second_is_error(&self) -> bool {
        self.waiting_from.is_none() && matches!(self.reply, Some(AskReply::Error(_)))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MillerHit {
    inner: Rect,
    scroll_offset: usize,
    item_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GridHit {
    inner: Rect,
    cell_w: u16,
    cell_h: u16,
    cols: usize,
    scroll_first: usize,
}

#[derive(Clone, Debug, Default)]
struct HitRegions {
    search: Rect,
    miller: Vec<MillerHit>,
    grid: Option<GridHit>,
}

pub fn run(root: PathBuf) -> Result<()> {
    let mut app = App::new(root)?;
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    enter_tui(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    // Query after the alternate screen so font-size / protocol detection can work.
    app.picker =
        Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((10, 20))));
    let result = event_loop(&mut terminal, &mut app);
    app.shutdown_ask();
    disable_raw_mode()?;
    leave_tui(terminal.backend_mut())?;
    terminal.show_cursor()?;
    result
}

fn enter_tui<W: std::io::Write>(out: &mut W) -> Result<()> {
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Ok(())
}

fn leave_tui<W: std::io::Write>(out: &mut W) -> Result<()> {
    execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    Ok(())
}

pub struct App {
    root: PathBuf,
    conn: Connection,
    full_tree: Folder,
    view_tree: Folder,
    cursor: Vec<usize>,
    miller_focus: usize,
    focus: Focus,
    query: String,
    photos: Vec<Photo>,
    grid_idx: usize,
    grid_cols: usize,
    grid_scroll: usize,
    picker: Option<Picker>,
    protocols: HashMap<String, StatefulProtocol>,
    status: String,
    miller_states: Vec<ListState>,
    hit: HitRegions,
    last_grid_click: Option<(usize, Instant, bool)>,
    marked: HashSet<String>,
    pending_delete: Option<Vec<String>>,
    clipboard: Option<clipboard::Clipboard>,
    pending_ask: Option<PendingAsk>,
    credentials_overlay: Option<CredentialsOverlay>,
    create_folder_overlay: Option<CreateFolderOverlay>,
    ask: AskAi,
    viewer: Arc<dyn ViewerOpener>,
    clock: Arc<dyn Clock>,
    ask_post: PostFn,
}

impl App {
    fn new(root: PathBuf) -> Result<Self> {
        Self::new_with(root, Arc::new(RealViewerOpener), Arc::new(SystemClock))
    }

    fn new_with(
        root: PathBuf,
        viewer: Arc<dyn ViewerOpener>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let conn = catalog::open(&root, false)?;
        let full_tree = library::scan_tree(&root)?;
        let view_tree = full_tree.clone();
        let mut app = Self {
            root,
            conn,
            full_tree,
            view_tree,
            cursor: vec![0],
            miller_focus: 0,
            focus: Focus::Miller,
            query: String::new(),
            photos: Vec::new(),
            grid_idx: 0,
            grid_cols: 1,
            grid_scroll: 0,
            picker: None,
            protocols: HashMap::new(),
            status: STATUS_HINT.into(),
            miller_states: Vec::new(),
            hit: HitRegions::default(),
            last_grid_click: None,
            marked: HashSet::new(),
            pending_delete: None,
            clipboard: None,
            pending_ask: None,
            credentials_overlay: None,
            create_folder_overlay: None,
            ask: AskAi::new(),
            viewer,
            clock,
            ask_post: image_edit::post_openrouter,
        };
        app.reload_photos();
        Ok(app)
    }

    fn ask_active(&self) -> bool {
        ai::ask_ai_active(&self.ask.selection)
    }

    fn sync_ask_selection(&mut self) {
        let stills = ai::marked_still_rels(&self.photos, &self.marked);
        if stills == self.ask.selection {
            return;
        }
        self.ask.generation = self.ask.generation.wrapping_add(1);
        self.ask.cancel_job();
        self.ask.reply = None;
        self.ask.scroll = 0;
        self.ask.selection = stills;
    }

    fn poll_ask(&mut self) {
        let outcome = self.ask.job.as_mut().and_then(|job| job.try_recv());
        let Some(outcome) = outcome else {
            return;
        };
        self.ask.job = None;
        if ask_outcome_is_stale(self.ask.generation, outcome.id) {
            return;
        }
        self.ask.waiting_from = None;
        match outcome.result {
            Ok(ai::AskValue::Answer(text)) => {
                self.ask.reply = Some(AskReply::Text(text));
            }
            Ok(ai::AskValue::Saved(saved)) => {
                self.pending_ask = None;
                self.credentials_overlay = None;
                self.focus_saved_edit(&saved.relpath);
                let message = image_edit::saved_message(&saved.filename);
                self.status = message.clone();
                self.ask.reply = Some(AskReply::Text(message));
            }
            Err(err) => {
                if credentials::is_invalid_saved_key_error(&err) {
                    let stills = ai::marked_still_rels(&self.photos, &self.marked);
                    let files = ai::abs_stills(&self.root, &stills);
                    self.pending_ask = Some(PendingAsk {
                        prompt: self.ask.prompt.trim().to_string(),
                        files,
                        generation: self.ask.generation,
                    });
                    self.credentials_overlay = Some(CredentialsOverlay {
                        error: Some(
                            "OpenRouter rejected the saved key. Run `hallward credentials set` again."
                                .into(),
                        ),
                    });
                    return;
                }
                self.pending_ask = None;
                self.credentials_overlay = None;
                self.ask.reply = Some(AskReply::Error(err));
            }
        }
    }

    fn start_ask_job(
        &mut self,
        key: ResolvedKey,
        prompt: String,
        files: Vec<PathBuf>,
        generation: u64,
    ) {
        if ask_outcome_is_stale(self.ask.generation, generation) {
            return;
        }
        self.pending_ask = None;
        self.credentials_overlay = None;
        self.ask.waiting_from = Some(self.clock.now());
        self.ask.job = Some(ai::spawn_with(
            generation,
            prompt,
            files,
            self.root.clone(),
            key,
            Timeouts::default(),
            self.ask_post,
        ));
    }

    fn dismiss_credentials_overlay(&mut self) {
        self.pending_ask = None;
        self.credentials_overlay = None;
    }

    fn send_ask(&mut self) {
        if self.ask.waiting() {
            return;
        }
        let prompt = self.ask.prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let stills = ai::marked_still_rels(&self.photos, &self.marked);
        if stills.is_empty() {
            self.ask.reply = Some(AskReply::Error(ai::no_images_message()));
            return;
        }
        let files = ai::abs_stills(&self.root, &stills);
        self.ask.generation = self.ask.generation.wrapping_add(1);
        self.ask.cancel_job();
        self.ask.reply = None;
        self.ask.scroll = 0;
        let generation = self.ask.generation;
        if let Some(key) = credentials::resolve() {
            self.start_ask_job(key, prompt, files, generation);
        } else {
            self.pending_ask = Some(PendingAsk {
                prompt,
                files,
                generation,
            });
            self.credentials_overlay = Some(CredentialsOverlay { error: None });
        }
    }

    fn focus_saved_edit(&mut self, relpath: &str) {
        self.protocols.remove(relpath);
        self.reload_photos_focusing(Some(relpath));
    }

    fn shutdown_ask(&mut self) {
        self.ask.generation = self.ask.generation.wrapping_add(1);
        self.ask.cancel_job();
    }

    fn miller_columns(&self) -> Vec<Vec<&Folder>> {
        let mut cols: Vec<Vec<&Folder>> = vec![self.view_tree.children.iter().collect()];
        loop {
            let i = cols.len() - 1;
            let sel = self.cursor.get(i).copied().unwrap_or(0);
            let Some(&node) = cols[i].get(sel) else {
                break;
            };
            if node.kind == Kind::Collection && !node.children.is_empty() {
                cols.push(node.children.iter().collect());
            } else {
                break;
            }
        }
        cols
    }

    fn selected_folder(&self) -> Option<&Folder> {
        let cols = self.miller_columns();
        let col = self.miller_focus.min(cols.len().saturating_sub(1));
        let sel = self.cursor.get(col).copied().unwrap_or(0);
        cols.get(col).and_then(|c| c.get(sel).copied())
    }

    fn current_album(&self) -> Option<&Folder> {
        let cols = self.miller_columns();
        for col in (0..cols.len()).rev() {
            let sel = self.cursor.get(col).copied().unwrap_or(0);
            if let Some(f) = cols.get(col).and_then(|c| c.get(sel)).copied() {
                if f.kind == Kind::Album {
                    return Some(f);
                }
            }
        }
        None
    }

    /// Folder shown in the gallery pane: an album, or a collection the user opened (empty or in grid).
    fn gallery_folder(&self) -> Option<&Folder> {
        if let Some(album) = self.current_album() {
            return Some(album);
        }
        let folder = self.selected_folder()?;
        if folder.kind == Kind::Collection
            && (self.focus == Focus::Grid || folder.children.is_empty())
        {
            Some(folder)
        } else {
            None
        }
    }

    fn album_key(folder: &Folder) -> String {
        let s = folder.relpath.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            ".".into()
        } else {
            s
        }
    }

    fn reload_photos(&mut self) {
        self.reload_photos_focusing(None);
    }

    fn reload_photos_focusing(&mut self, focus_rel: Option<&str>) {
        self.photos = if let Some(album) = self.current_album() {
            let key = Self::album_key(album);
            catalog::photos_in_album(&self.conn, &key).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.grid_idx = focused_photo_index(&self.photos, self.grid_idx, focus_rel);
        if focus_rel.is_none() {
            self.grid_scroll = 0;
        }
        self.marked.clear();
        self.sync_ask_selection();
    }

    fn apply_query(&mut self) {
        self.view_tree = search::filter_tree(&self.full_tree, &self.query);
        self.cursor = vec![0];
        self.miller_focus = 0;
        self.grid_idx = 0;
        self.reload_photos();
    }

    fn selected_photo(&self) -> Option<&Photo> {
        self.photos.get(self.grid_idx)
    }

    fn ensure_protocol(&mut self, rel: &str) {
        if self.protocols.contains_key(rel) {
            return;
        }
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let path = thumbs::thumb_path(&self.root, rel);
        let Ok(img) = image::open(&path) else {
            return;
        };
        self.protocols
            .insert(rel.to_string(), picker.new_resize_protocol(img));
    }
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let mut driver = CrosstermDriver::new(terminal);
    event_loop_with(&mut driver, app)
}

fn event_loop_with(driver: &mut dyn TerminalDriver, app: &mut App) -> Result<()> {
    loop {
        app.sync_ask_selection();
        app.poll_ask();
        driver.redraw(app)?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(app, key, driver)? {
                    break;
                }
            }
            Event::Paste(text) if app.create_folder_overlay.is_some() => {
                append_create_folder_input(app, &text);
            }
            Event::Mouse(mouse) => {
                handle_mouse(app, mouse, driver)?;
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent, terminal: &mut dyn TerminalDriver) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.credentials_overlay.is_some() {
        handle_credentials_overlay_key(app, key);
        return Ok(false);
    }

    if app.create_folder_overlay.is_some() {
        handle_create_folder_overlay_key(app, key);
        return Ok(false);
    }

    if app.pending_delete.is_some() {
        match classify_confirm_key(key.code) {
            ConfirmKey::Yes => confirm_pending_delete(app, terminal)?,
            ConfirmKey::No => {
                app.pending_delete = None;
            }
            ConfirmKey::Ignore => {}
        }
        return Ok(false);
    }

    if is_shift_tab(&key) {
        app.focus = shift_tab_focus(app.focus);
        return Ok(false);
    }

    if app.focus == Focus::Search {
        if app.ask_active() {
            handle_ask_ai_key(app, key.code);
        } else {
            match key.code {
                KeyCode::Esc => {
                    app.query.clear();
                    app.focus = Focus::Miller;
                    app.apply_query();
                }
                KeyCode::Tab => {
                    app.focus = Focus::Miller;
                    app.miller_focus = 0;
                }
                KeyCode::Enter => {
                    app.focus = Focus::Miller;
                    open_viewer(app, terminal)?;
                }
                KeyCode::Backspace => {
                    app.query.pop();
                    app.apply_query();
                    app.focus = Focus::Search;
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.apply_query();
                    app.focus = Focus::Search;
                }
                _ => {}
            }
        }
        return Ok(false);
    }

    let ask_ai = app.ask_active();
    match key.code {
        KeyCode::Char('q') if !ask_ai => return Ok(true),
        KeyCode::Char('r') if !ask_ai => reindex(app, terminal)?,
        KeyCode::Char('d') if !selected_targets(app).is_empty() => {
            app.pending_delete = Some(selected_targets(app));
        }
        KeyCode::Char('c') if !selected_targets(app).is_empty() => {
            set_clipboard(app, ClipboardOp::Copy);
        }
        KeyCode::Char('x') if !selected_targets(app).is_empty() => {
            set_clipboard(app, ClipboardOp::Cut);
        }
        KeyCode::Char('p') if app.clipboard.is_some() => paste_clipboard(app, terminal)?,
        KeyCode::Char('a') if !ask_ai && can_create_folder(app) => {
            app.create_folder_overlay = Some(CreateFolderOverlay {
                parent_relpath: create_folder_parent(app),
                input: String::new(),
                error: None,
            });
        }
        KeyCode::Esc => match classify_esc(
            !app.marked.is_empty(),
            app.clipboard.is_some(),
            !app.query.is_empty(),
        ) {
            EscTarget::Marks => {
                app.marked.clear();
                app.sync_ask_selection();
            }
            EscTarget::Clipboard => {
                app.clipboard = None;
                app.status = STATUS_HINT.into();
            }
            EscTarget::Query => {
                app.query.clear();
                app.apply_query();
            }
            EscTarget::None => {}
        },
        KeyCode::Tab => {
            if library_tab_focuses_ask(ask_ai, !app.query.is_empty()) {
                app.focus = Focus::Search;
            }
        }
        KeyCode::Enter => open_viewer(app, terminal)?,
        KeyCode::Char(' ') => {
            if app.focus == Focus::Grid {
                if let Some(p) = app.photos.get(app.grid_idx) {
                    toggle_mark(&mut app.marked, &p.relpath);
                    app.sync_ask_selection();
                }
            }
        }
        KeyCode::Char(c) if !c.is_control() && ask_ai => {
            let waiting = app.ask.waiting();
            type_into_ask_prompt(&mut app.ask.prompt, waiting, c);
            app.focus = Focus::Search;
        }
        KeyCode::Up => move_up(app),
        KeyCode::Down => move_down(app),
        KeyCode::Left => move_left(app),
        KeyCode::Right => move_right(app),
        _ => {}
    }
    Ok(false)
}

fn handle_ask_ai_key(app: &mut App, code: KeyCode) {
    let waiting = app.ask.waiting();
    match classify_ask_field_key(code, waiting) {
        AskFieldKey::ExitClear => {
            app.ask.clear_thread();
            app.focus = Focus::Miller;
            app.miller_focus = 0;
        }
        AskFieldKey::ExitKeep => {
            app.focus = Focus::Miller;
            app.miller_focus = 0;
        }
        AskFieldKey::Send => app.send_ask(),
        AskFieldKey::Backspace => {
            app.ask.prompt.pop();
        }
        AskFieldKey::Char(c) => {
            app.ask.prompt.push(c);
        }
        AskFieldKey::ScrollUp
        | AskFieldKey::ScrollDown
        | AskFieldKey::PageUp
        | AskFieldKey::PageDown => {
            let max = ask_scroll_max(
                &app.ask.prompt,
                app.ask.second_paragraph(app.clock.now()).as_deref(),
                app.hit.search,
            );
            let page = ask_scroll_page(app.hit.search.height);
            app.ask.scroll = apply_ask_scroll(
                app.ask.scroll,
                classify_ask_field_key(code, waiting),
                page,
                max,
            );
        }
        AskFieldKey::Ignore => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskFieldKey {
    ExitKeep,
    ExitClear,
    Send,
    Backspace,
    Char(char),
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Ignore,
}

fn classify_ask_field_key(code: KeyCode, waiting: bool) -> AskFieldKey {
    match code {
        KeyCode::Esc => AskFieldKey::ExitClear,
        KeyCode::Tab => AskFieldKey::ExitKeep,
        KeyCode::Enter => AskFieldKey::Send,
        KeyCode::Backspace if !waiting => AskFieldKey::Backspace,
        KeyCode::Char(c) if !waiting => AskFieldKey::Char(c),
        KeyCode::Up => AskFieldKey::ScrollUp,
        KeyCode::Down => AskFieldKey::ScrollDown,
        KeyCode::PageUp => AskFieldKey::PageUp,
        KeyCode::PageDown => AskFieldKey::PageDown,
        _ => AskFieldKey::Ignore,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmKey {
    Yes,
    No,
    Ignore,
}

fn classify_confirm_key(code: KeyCode) -> ConfirmKey {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ConfirmKey::Yes,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ConfirmKey::No,
        _ => ConfirmKey::Ignore,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscTarget {
    Marks,
    Clipboard,
    Query,
    None,
}

fn classify_esc(has_marks: bool, has_clipboard: bool, has_query: bool) -> EscTarget {
    if has_marks {
        EscTarget::Marks
    } else if has_clipboard {
        EscTarget::Clipboard
    } else if has_query {
        EscTarget::Query
    } else {
        EscTarget::None
    }
}

fn selected_targets(app: &App) -> Vec<String> {
    let photos = delete::delete_rels(
        &app.photos,
        &app.marked,
        app.focus == Focus::Grid,
        app.grid_idx,
    );
    if !photos.is_empty() {
        return photos;
    }
    miller_folder_target(app.focus, app.selected_folder().map(App::album_key))
        .into_iter()
        .collect()
}

fn miller_folder_target(focus: Focus, folder_key: Option<String>) -> Option<String> {
    if focus != Focus::Miller {
        return None;
    }
    folder_key.filter(|key| key != ".")
}

fn folder_dest_key(rel: &Path) -> String {
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

fn set_clipboard(app: &mut App, op: ClipboardOp) {
    let extra = selected_targets(app);
    app.clipboard = clipboard::Clipboard::from_key(app.clipboard.take(), extra, op);
    app.marked.clear();
    app.sync_ask_selection();
    let Some(clip) = app.clipboard.as_ref() else {
        app.status = STATUS_HINT.into();
        return;
    };
    app.status = match clip.op {
        ClipboardOp::Copy => clipboard::copied_message(clip.rels.len()),
        ClipboardOp::Cut => clipboard::cut_message(clip.rels.len()),
    };
}

fn absorb_marks_into_clipboard(app: &mut App) {
    if app.marked.is_empty() {
        return;
    }
    let extra = selected_targets(app);
    if let Some(clip) = app.clipboard.take() {
        app.clipboard = Some(clip.absorbing_marks(&extra));
    }
    app.marked.clear();
    app.sync_ask_selection();
}

fn paste_clipboard(app: &mut App, terminal: &mut dyn TerminalDriver) -> Result<()> {
    absorb_marks_into_clipboard(app);
    let Some(clip) = app.clipboard.clone() else {
        return Ok(());
    };
    let dest_files = app.gallery_folder().map(App::album_key);
    let dest_folders = folder_dest_key(&create_folder_parent(app));
    let has_folder = clip.rels.iter().any(|rel| app.root.join(rel).is_dir());
    let has_file = clip.rels.iter().any(|rel| app.root.join(rel).is_file());
    if has_file && dest_files.is_none() && !has_folder {
        app.status = "select an album to paste".into();
        return Ok(());
    }
    match clipboard::paste_into(&app.root, &clip, dest_files.as_deref(), &dest_folders) {
        Ok(result) if result.same_album_cut => {
            app.status = "already here".into();
        }
        Ok(result) if result.items.is_empty() => {
            app.status = "nothing to paste".into();
        }
        Ok(result) => {
            if clip.op == ClipboardOp::Cut {
                app.clipboard = None;
            }
            let focus = result.pasted().first().cloned();
            refresh_after_paste(app, terminal, &result, &clip, focus.as_deref())?;
        }
        Err(e) => {
            // Partial copies may already be on disk without catalog rows.
            // Fall back to a full scan so nothing stays invisible.
            let msg = format!("paste failed: {e:#}");
            match index::index_library(&app.root) {
                Ok(stats) => app.status = format!("{msg} · {}", stats.summary()),
                Err(ie) => app.status = format!("{msg} · index failed: {ie:#}"),
            }
            if let Err(e) = refresh_tree_only(app, None) {
                app.status = format!("{} · tree scan failed: {e:#}", app.status);
            }
        }
    }
    Ok(())
}

fn ask_scroll_page(pane_height: u16) -> u16 {
    pane_height.saturating_sub(3).max(1)
}

fn library_tab_focuses_ask(ask_active: bool, query_nonempty: bool) -> bool {
    ask_active || query_nonempty
}

fn type_into_ask_prompt(prompt: &mut String, waiting: bool, c: char) {
    if !waiting {
        prompt.push(c);
    }
}

fn apply_ask_scroll(scroll: u16, key: AskFieldKey, page: u16, max: u16) -> u16 {
    let next = match key {
        AskFieldKey::ScrollUp => scroll.saturating_sub(1),
        AskFieldKey::ScrollDown => scroll.saturating_add(1),
        AskFieldKey::PageUp => scroll.saturating_sub(page),
        AskFieldKey::PageDown => scroll.saturating_add(page),
        _ => scroll,
    };
    next.min(max)
}

fn ask_outcome_is_stale(generation: u64, outcome_id: u64) -> bool {
    generation != outcome_id
}

fn focused_photo_index(photos: &[Photo], current: usize, focus_rel: Option<&str>) -> usize {
    if let Some(rel) = focus_rel {
        if let Some(index) = photos.iter().position(|photo| photo.relpath == rel) {
            return index;
        }
    }
    if photos.is_empty() {
        0
    } else if current >= photos.len() {
        photos.len() - 1
    } else {
        current
    }
}

fn is_shift_tab(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::BackTab)
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
}

fn shift_tab_focus(from: Focus) -> Focus {
    match from {
        Focus::Miller | Focus::Grid | Focus::Search => Focus::Search,
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, terminal: &mut dyn TerminalDriver) -> Result<()> {
    if app.pending_delete.is_some()
        || app.credentials_overlay.is_some()
        || app.create_folder_overlay.is_some()
    {
        return Ok(());
    }
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return Ok(());
    }
    let pos = Position {
        x: mouse.column,
        y: mouse.row,
    };

    if app.hit.search.contains(pos) {
        app.focus = Focus::Search;
        return Ok(());
    }

    for (col, hit) in app.hit.miller.iter().enumerate() {
        if let Some(row) = miller_row_at(*hit, pos) {
            select_miller_row(app, col, row);
            return Ok(());
        }
    }

    if let Some(hit) = app.hit.grid {
        if let Some(idx) = grid_index_at(hit, pos, app.photos.len()) {
            let now = app.clock.now();
            let last = app.last_grid_click.map(|(i, t, _)| (i, t));
            if is_double_click(last, idx, now, DOUBLE_CLICK) {
                let was_marked = app.last_grid_click.map(|(_, _, m)| m).unwrap_or(false);
                app.last_grid_click = None;
                app.grid_idx = idx;
                app.focus = Focus::Grid;
                if was_marked {
                    if let Some(rel) = app.photos.get(idx).map(|p| p.relpath.as_str()) {
                        restore_mark_for_double_click(&mut app.marked, rel);
                    }
                }
                open_viewer(app, terminal)?;
            } else {
                let rel = app.photos.get(idx).map(|p| p.relpath.clone());
                let was_marked = rel.as_deref().is_some_and(|r| app.marked.contains(r));
                app.last_grid_click = Some((idx, now, was_marked));
                app.grid_idx = idx;
                app.focus = Focus::Grid;
                apply_grid_click(&mut app.marked, rel.as_deref());
            }
        } else if hit.inner.contains(pos) {
            apply_grid_click(&mut app.marked, None);
        }
        app.sync_ask_selection();
    }
    Ok(())
}

fn select_miller_row(app: &mut App, col: usize, row: usize) {
    if app.cursor.len() <= col {
        app.cursor.resize(col + 1, 0);
    }
    app.cursor[col] = row;
    app.cursor.truncate(col + 1);
    app.miller_focus = col;
    app.focus = Focus::Miller;
    app.grid_idx = 0;
    app.last_grid_click = None;
    app.reload_photos();
}

fn miller_row_at(hit: MillerHit, pos: Position) -> Option<usize> {
    if !hit.inner.contains(pos) || hit.item_count == 0 {
        return None;
    }
    let row = (pos.y.saturating_sub(hit.inner.y)) as usize;
    let idx = hit.scroll_offset.saturating_add(row);
    if idx < hit.item_count {
        Some(idx)
    } else {
        None
    }
}

fn grid_index_at(hit: GridHit, pos: Position, photo_count: usize) -> Option<usize> {
    if !hit.inner.contains(pos) || photo_count == 0 || hit.cell_w == 0 || hit.cell_h == 0 {
        return None;
    }
    let col = ((pos.x.saturating_sub(hit.inner.x)) / hit.cell_w) as usize;
    let row = ((pos.y.saturating_sub(hit.inner.y)) / hit.cell_h) as usize;
    if col >= hit.cols {
        return None;
    }
    let n = row.saturating_mul(hit.cols).saturating_add(col);
    let idx = hit.scroll_first.saturating_add(n);
    if idx < photo_count {
        Some(idx)
    } else {
        None
    }
}

fn is_double_click(
    last: Option<(usize, Instant)>,
    idx: usize,
    now: Instant,
    threshold: Duration,
) -> bool {
    match last {
        Some((last_idx, last_time)) if last_idx == idx => now.duration_since(last_time) < threshold,
        _ => false,
    }
}

fn toggle_mark(marked: &mut HashSet<String>, rel: &str) {
    if !marked.remove(rel) {
        marked.insert(rel.to_string());
    }
}

fn apply_grid_click(marked: &mut HashSet<String>, clicked: Option<&str>) {
    match clicked {
        Some(rel) if marked.contains(rel) => {
            marked.remove(rel);
        }
        _ => marked.clear(),
    }
}

fn restore_mark_for_double_click(marked: &mut HashSet<String>, rel: &str) {
    marked.insert(rel.to_string());
}

fn viewer_playlist(
    photos: &[Photo],
    marked: &HashSet<String>,
    grid_idx: usize,
) -> (Vec<String>, usize) {
    if marked.is_empty() {
        let rels: Vec<String> = photos.iter().map(|p| p.relpath.clone()).collect();
        let start = if photos.is_empty() {
            0
        } else {
            grid_idx.min(photos.len() - 1)
        };
        return (rels, start);
    }
    let rels: Vec<String> = photos
        .iter()
        .filter(|p| marked.contains(&p.relpath))
        .map(|p| p.relpath.clone())
        .collect();
    let start = photos
        .get(grid_idx)
        .filter(|p| marked.contains(&p.relpath))
        .and_then(|p| rels.iter().position(|r| r == &p.relpath))
        .unwrap_or(0);
    (rels, start)
}

fn grid_cell_focused(focus: Focus, grid_idx: usize, idx: usize) -> bool {
    focus == Focus::Grid && idx == grid_idx
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellBorder {
    Focus,
    Copied,
    Cut,
    Marked,
    Idle,
}

fn cell_border(focused: bool, clip: Option<ClipboardOp>, marked: bool) -> CellBorder {
    if focused {
        CellBorder::Focus
    } else if clip == Some(ClipboardOp::Copy) {
        CellBorder::Copied
    } else if clip == Some(ClipboardOp::Cut) {
        CellBorder::Cut
    } else if marked {
        CellBorder::Marked
    } else {
        CellBorder::Idle
    }
}

fn clipboard_op_for(clip: Option<&clipboard::Clipboard>, rel: &str) -> Option<ClipboardOp> {
    clip.filter(|c| c.rels.iter().any(|r| r == rel))
        .map(|c| c.op)
}

fn grid_cell_border_style(kind: CellBorder) -> Style {
    match kind {
        CellBorder::Focus => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        CellBorder::Copied | CellBorder::Cut | CellBorder::Marked => {
            Style::default().fg(Color::Cyan)
        }
        CellBorder::Idle => Style::default().fg(Color::Rgb(40, 40, 40)),
    }
}

fn grid_cell_block(kind: CellBorder) -> Block<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(grid_cell_border_style(kind));
    match kind {
        CellBorder::Copied => block.border_type(BorderType::Double),
        CellBorder::Cut => block.border_set(DASHED_BORDER),
        _ => block,
    }
}

fn album_grid_heading(album_name: &str) -> String {
    album_name.to_string()
}

fn album_grid_footer(
    pos: usize,
    total: usize,
    marked_count: usize,
    clip_label: Option<String>,
    photo_focused: bool,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if photo_focused {
        parts.push(format!("{pos}/{total}"));
    }
    if marked_count > 0 {
        parts.push(format!("{marked_count} marked"));
    }
    if let Some(clip) = clip_label {
        parts.push(clip);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn clipboard_footer_label(photos: &[Photo], clip: Option<&clipboard::Clipboard>) -> Option<String> {
    let clip = clip?;
    let n = photos
        .iter()
        .filter(|photo| clip.rels.iter().any(|rel| rel == &photo.relpath))
        .count();
    if n == 0 {
        return None;
    }
    Some(match clip.op {
        ClipboardOp::Copy => format!("{n} copied"),
        ClipboardOp::Cut => format!("{n} cut"),
    })
}

fn album_media_summary(photos: &[Photo]) -> Vec<String> {
    let photo_count = photos
        .iter()
        .filter(|p| is_image(Path::new(&p.relpath)))
        .count();
    let video_count = photos
        .iter()
        .filter(|p| is_video(Path::new(&p.relpath)))
        .count();
    let mut lines = Vec::new();
    if photo_count > 0 || video_count == 0 {
        lines.push(count_noun(photo_count, "photo", "photos"));
    }
    lines.push(count_noun(video_count, "video", "videos"));
    lines
}

fn count_noun(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

fn column_len(app: &App, col: usize) -> usize {
    app.miller_columns().get(col).map(|c| c.len()).unwrap_or(0)
}

fn can_create_folder(app: &App) -> bool {
    match app.focus {
        Focus::Search => false,
        Focus::Grid => app
            .selected_folder()
            .is_some_and(|f| f.kind == Kind::Collection),
        Focus::Miller => true,
    }
}

fn move_up(app: &mut App) {
    if app.focus == Focus::Grid {
        if app.grid_cols > 0 && app.grid_idx >= app.grid_cols {
            app.grid_idx -= app.grid_cols;
        }
        return;
    }
    let col = app.miller_focus;
    if let Some(sel) = app.cursor.get_mut(col) {
        if *sel > 0 {
            *sel -= 1;
            app.cursor.truncate(col + 1);
            app.grid_idx = 0;
            app.reload_photos();
        }
    }
}

fn move_down(app: &mut App) {
    if app.focus == Focus::Grid {
        let next = app.grid_idx + app.grid_cols.max(1);
        if next < app.photos.len() {
            app.grid_idx = next;
        }
        return;
    }
    let col = app.miller_focus;
    let n = column_len(app, col);
    if n == 0 {
        return;
    }
    if app.cursor.len() <= col {
        app.cursor.resize(col + 1, 0);
    }
    if app.cursor[col] + 1 < n {
        app.cursor[col] += 1;
        app.cursor.truncate(col + 1);
        app.grid_idx = 0;
        app.reload_photos();
    }
}

fn move_left(app: &mut App) {
    if app.focus == Focus::Grid {
        let cols = app.grid_cols.max(1);
        if app.grid_idx % cols == 0 {
            app.focus = Focus::Miller;
            let n = app.miller_columns().len();
            app.miller_focus = n.saturating_sub(1);
        } else if app.grid_idx > 0 {
            app.grid_idx -= 1;
        }
        return;
    }
    if app.miller_focus > 0 {
        app.miller_focus -= 1;
        app.cursor.truncate(app.miller_focus + 1);
        app.grid_idx = 0;
        app.reload_photos();
    }
}

fn move_right(app: &mut App) {
    if app.focus == Focus::Grid {
        if app.grid_idx + 1 < app.photos.len() {
            app.grid_idx += 1;
        }
        return;
    }
    let kind = app.selected_folder().map(|f| f.kind);
    match kind {
        Some(Kind::Album) => {
            app.focus = Focus::Grid;
        }
        Some(Kind::Collection) => {
            let folder = app.selected_folder().unwrap();
            if folder.children.is_empty() {
                app.focus = Focus::Grid;
            } else {
                let next = app.miller_focus + 1;
                let cols = app.miller_columns();
                if next < cols.len() && !cols[next].is_empty() {
                    app.miller_focus = next;
                    if app.cursor.len() <= next {
                        app.cursor.resize(next + 1, 0);
                    }
                }
            }
        }
        None => {}
    }
}

fn confirm_pending_delete(app: &mut App, terminal: &mut dyn TerminalDriver) -> Result<()> {
    let Some(rels) = app.pending_delete.take() else {
        return Ok(());
    };
    match delete::unlink_media(&app.root, &rels) {
        Ok(()) => reindex(app, terminal)?,
        Err(e) => app.status = format!("delete failed: {e:#}"),
    }
    Ok(())
}

fn reindex(app: &mut App, terminal: &mut dyn TerminalDriver) -> Result<()> {
    reindex_focusing(app, terminal, None)
}

fn reindex_focusing(
    app: &mut App,
    terminal: &mut dyn TerminalDriver,
    focus_rel: Option<&str>,
) -> Result<()> {
    app.status = "indexing…".into();
    terminal.redraw(app)?;
    match index::index_library(&app.root) {
        Ok(stats) => {
            app.status = stats.summary();
        }
        Err(e) => app.status = format!("index failed: {e:#}"),
    }
    match library::scan_tree(&app.root) {
        Ok(tree) => {
            app.full_tree = tree;
            app.view_tree = if app.query.is_empty() {
                app.full_tree.clone()
            } else {
                search::filter_tree(&app.full_tree, &app.query)
            };
            if let Some(rel) = focus_rel.filter(|rel| app.root.join(rel).is_dir()) {
                focus_folder_path(app, Path::new(rel));
            } else {
                clamp_cursor(app);
            }
        }
        Err(e) => app.status = format!("{} · tree scan failed: {e:#}", app.status),
    }
    app.protocols.clear();
    if focus_rel.is_some_and(|rel| app.root.join(rel).is_dir()) {
        app.reload_photos();
    } else {
        app.reload_photos_focusing(focus_rel);
    }
    Ok(())
}

fn refresh_after_paste(
    app: &mut App,
    terminal: &mut dyn TerminalDriver,
    result: &clipboard::PasteResult,
    clip: &clipboard::Clipboard,
    focus_rel: Option<&str>,
) -> Result<()> {
    app.status = "indexing…".into();
    terminal.redraw(app)?;
    match index::index_pasted(&app.root, &result.items, &clip.rels, clip.op) {
        Ok(stats) => {
            app.status = stats.summary();
        }
        Err(e) => app.status = format!("index failed: {e:#}"),
    }
    refresh_tree_only(app, focus_rel)?;
    Ok(())
}

fn refresh_tree_only(app: &mut App, focus_rel: Option<&str>) -> Result<()> {
    match library::scan_tree(&app.root) {
        Ok(tree) => {
            app.full_tree = tree;
            app.view_tree = if app.query.is_empty() {
                app.full_tree.clone()
            } else {
                search::filter_tree(&app.full_tree, &app.query)
            };
            if let Some(rel) = focus_rel.filter(|rel| app.root.join(rel).is_dir()) {
                focus_folder_path(app, Path::new(rel));
            } else {
                clamp_cursor(app);
            }
        }
        Err(e) => app.status = format!("{} · tree scan failed: {e:#}", app.status),
    }
    app.protocols.clear();
    if focus_rel.is_some_and(|rel| app.root.join(rel).is_dir()) {
        app.reload_photos();
    } else {
        app.reload_photos_focusing(focus_rel);
    }
    Ok(())
}

fn clamp_cursor(app: &mut App) {
    if app.cursor.is_empty() {
        app.cursor.push(0);
    }
    let n_cols = app.miller_columns().len().max(1);
    if app.miller_focus >= n_cols {
        app.miller_focus = n_cols - 1;
    }
    app.cursor.truncate(n_cols);
    for col in 0..app.cursor.len() {
        let n = column_len(app, col);
        if n == 0 {
            app.cursor[col] = 0;
        } else if app.cursor[col] >= n {
            app.cursor[col] = n - 1;
        }
    }
}

fn open_viewer(app: &mut App, terminal: &mut dyn TerminalDriver) -> Result<()> {
    if app.photos.is_empty() {
        app.status = "no photos in this album".into();
        return Ok(());
    }
    let (rels, start) = viewer_playlist(&app.photos, &app.marked, app.grid_idx);
    if rels.is_empty() {
        app.status = "no photos in this album".into();
        return Ok(());
    }
    let files = viewer::abs_files(&app.root, &rels);
    let viewer = app.viewer.clone();
    let open = {
        let mut run = || viewer.open(&files, start);
        terminal.suspend_for_viewer(&mut run)
    };
    terminal.clear_screen()?;
    match open {
        Ok(()) => {
            app.status = STATUS_HINT.into();
        }
        Err(e) => app.status = format!("{e:#}"),
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    app.hit = HitRegions::default();
    let area = frame.area();
    let ask = app.ask_active();
    let now = app.clock.now();
    let second = app.ask.second_paragraph(now);
    let top_h = ai::search_pane_height(ask, &app.ask.prompt, second.as_deref(), area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_h),
            Constraint::Min(8),
            Constraint::Length(7),
        ])
        .split(area);

    app.hit.search = chunks[0];
    draw_search(frame, app, chunks[0], ask, second.as_deref());

    let col_count = app.miller_columns().len();
    let n_miller = col_count.max(1);
    let mut constraints = vec![Constraint::Length(26); n_miller];
    constraints.push(Constraint::Min(20));
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(chunks[1]);

    if col_count == 0 {
        frame.render_widget(
            Paragraph::new("(no matching folders)")
                .block(Block::default().borders(Borders::ALL).title("Library")),
            main[0],
        );
    } else {
        app.hit.miller.resize(col_count, MillerHit::default());
        for i in 0..col_count {
            draw_miller_col(frame, app, i, main[i]);
        }
    }
    draw_grid(frame, app, main[n_miller]);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[2]);
    draw_exif(frame, app, bottom[0]);
    draw_status(frame, app, bottom[1]);
    if let Some(rels) = app.pending_delete.as_deref() {
        draw_delete_confirm(frame, &app.root, rels);
    }
    if app.credentials_overlay.is_some() {
        draw_credentials_overlay(frame, app);
    }
    if app.create_folder_overlay.is_some() {
        draw_create_folder_overlay(frame, app);
    }
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect, ask_ai: bool, second: Option<&str>) {
    let focused = app.focus == Focus::Search;
    let title = search_pane_title(ask_ai, focused);
    let style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let text = if ask_ai {
        ask_ai_lines(&app.ask.prompt, second, app.ask.second_is_error(), focused)
    } else {
        vec![Line::from(app.query.clone())]
    };
    let max_scroll = ask_scroll_max(&app.ask.prompt, second, area);
    let scroll = app.ask.scroll.min(max_scroll);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title);
    let mut p = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(block);
    if !ask_ai {
        p = p.style(style);
    }
    frame.render_widget(p, area);
}

fn search_pane_title(ask_ai: bool, focused: bool) -> &'static str {
    match (ask_ai, focused) {
        (true, true) => "Ask AI (Enter send · Tab tree · Esc clear)",
        (true, false) => "Ask AI (Shift+Tab · type to prompt)",
        (false, true) => "Search (Tab tree · Esc clear)",
        (false, false) => "Search (Shift+Tab)",
    }
}

fn ask_ai_lines(
    prompt: &str,
    second: Option<&str>,
    is_error: bool,
    focused: bool,
) -> Vec<Line<'static>> {
    let prompt_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let mut lines = vec![Line::from(Span::styled(prompt.to_string(), prompt_style))];
    if let Some(s) = second {
        lines.push(Line::from(""));
        let style = if is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        for part in s.split('\n') {
            lines.push(Line::from(Span::styled(part.to_string(), style)));
        }
    }
    lines
}

fn ask_scroll_max(prompt: &str, second: Option<&str>, area: Rect) -> u16 {
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2).max(1);
    let prompt_lines = ai::wrap_line_count(prompt, inner_w);
    let extra = match second {
        None => 0,
        Some(s) => 1 + ai::wrap_line_count(s, inner_w),
    };
    (prompt_lines + extra).saturating_sub(inner_h)
}

fn draw_miller_col(frame: &mut Frame, app: &mut App, col: usize, area: Rect) {
    let cols = app.miller_columns();
    let items: Vec<ListItem> = cols
        .get(col)
        .into_iter()
        .flatten()
        .map(|f| {
            let mark = match f.kind {
                Kind::Collection => "▸ ",
                Kind::Album => "  ",
            };
            let rel = App::album_key(f);
            let clip = clipboard_op_for(app.clipboard.as_ref(), &rel);
            let style = match clip {
                Some(_) => Style::default().fg(Color::Cyan),
                None => Style::default(),
            };
            ListItem::new(format!("{mark}{}", f.display_name())).style(style)
        })
        .collect();
    let focused = app.focus == Focus::Miller && app.miller_focus == col;
    let border = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let title = if col == 0 { "Library" } else { "Folders" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let inner = block.inner(area);
    let list = List::new(items.clone())
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .block(block);
    if app.miller_states.len() <= col {
        app.miller_states.resize(col + 1, ListState::default());
    }
    let state = &mut app.miller_states[col];
    state.select(app.cursor.get(col).copied());
    frame.render_stateful_widget(list, area, state);
    app.hit.miller[col] = MillerHit {
        inner,
        scroll_offset: state.offset(),
        item_count: items.len(),
    };
}

fn draw_grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Grid;
    let border = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let album_name = app
        .gallery_folder()
        .map(|a| a.display_name().to_string())
        .unwrap_or_else(|| "—".into());
    let heading = album_grid_heading(&album_name);
    let pos = if app.photos.is_empty() {
        0
    } else {
        app.grid_idx + 1
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(heading);
    let clip_label = clipboard_footer_label(&app.photos, app.clipboard.as_ref());
    let block =
        match album_grid_footer(pos, app.photos.len(), app.marked.len(), clip_label, focused) {
            Some(footer) => block.title_bottom(footer),
            None => block,
        };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.gallery_folder().is_none() {
        frame.render_widget(
            Paragraph::new("Select an album with the right arrow.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        app.hit.grid = None;
        return;
    }
    if app.photos.is_empty() {
        let hint = if app.focus == Focus::Grid {
            "Empty · p paste · a new album"
        } else {
            "Press → to open"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        app.hit.grid = None;
        return;
    }

    let (fw, fh) = app
        .picker
        .as_ref()
        .map(|p| p.font_size())
        .unwrap_or((10, 20));
    let (cell_w, cell_h) = square_cell_size(fw, fh, CELL_INNER_H);
    let cols = (inner.width / cell_w).max(1) as usize;
    let rows = (inner.height / cell_h).max(1) as usize;
    app.grid_cols = cols;
    let vis = cols * rows;
    if vis == 0 {
        return;
    }
    let row = if cols == 0 { 0 } else { app.grid_idx / cols };
    if row < app.grid_scroll {
        app.grid_scroll = row;
    }
    if row >= app.grid_scroll + rows {
        app.grid_scroll = row + 1 - rows;
    }
    let first = app.grid_scroll * cols;
    app.hit.grid = Some(GridHit {
        inner,
        cell_w,
        cell_h,
        cols,
        scroll_first: first,
    });

    let rels: Vec<String> = app
        .photos
        .iter()
        .skip(first)
        .take(vis)
        .map(|p| p.relpath.clone())
        .collect();
    for rel in &rels {
        app.ensure_protocol(rel);
    }

    for (n, rel) in rels.iter().enumerate() {
        let idx = first + n;
        let c = (n % cols) as u16;
        let r = (n / cols) as u16;
        let cell = Rect {
            x: inner.x + c * cell_w,
            y: inner.y + r * cell_h,
            width: cell_w.min(inner.width.saturating_sub(c * cell_w)),
            height: cell_h.min(inner.height.saturating_sub(r * cell_h)),
        };
        if cell.width < 3 || cell.height < 3 {
            continue;
        }
        let focused = grid_cell_focused(app.focus, app.grid_idx, idx);
        let marked = app.marked.contains(rel);
        let clip = clipboard_op_for(app.clipboard.as_ref(), rel);
        let cell_block = grid_cell_block(cell_border(focused, clip, marked));
        let img_area = cell_block.inner(cell);
        frame.render_widget(cell_block, cell);
        if let Some(proto) = app.protocols.get_mut(rel) {
            frame.render_stateful_widget(StatefulImage::default(), img_area, proto);
        } else {
            let name = Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            frame.render_widget(
                Paragraph::new(name)
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: true }),
                img_area,
            );
        }
    }
}

fn draw_exif(frame: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = if app.focus == Focus::Grid {
        if let Some(p) = app.selected_photo() {
            ("EXIF", exif_lines(p))
        } else {
            ("Album", album_pane_lines(app))
        }
    } else {
        ("Album", album_pane_lines(app))
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn exif_lines(p: &Photo) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            p.filename.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(
            p.captured_at
                .clone()
                .unwrap_or_else(|| "date unknown".into()),
        ),
        Line::from(p.camera.clone().unwrap_or_else(|| "camera unknown".into())),
        Line::from(match (p.width, p.height) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => String::new(),
        }),
        Line::from(p.relpath.clone()),
    ]
}

fn album_pane_lines(app: &App) -> Vec<Line<'static>> {
    if app.gallery_folder().is_none() {
        vec![Line::from("Select an album")]
    } else if app.photos.is_empty() {
        vec![Line::from("Empty")]
    } else {
        album_media_summary(&app.photos)
            .into_iter()
            .map(Line::from)
            .collect()
    }
}

fn protocol_name(picker: Option<&Picker>) -> &'static str {
    match picker.map(|p| p.protocol_type()) {
        Some(ProtocolType::Kitty) => "kitty",
        Some(ProtocolType::Sixel) => "sixel",
        Some(ProtocolType::Iterm2) => "iterm2",
        Some(ProtocolType::Halfblocks) | None => "halfblocks",
    }
}

fn status_ai_models() -> String {
    let (ask, edit) = credentials::effective_models();
    format!("ask: {ask} · edit: {edit}")
}

fn status_tools_line(viewer: &str, video: &str, thumbs: &str) -> String {
    format!(
        "viewer: {viewer} · video: {video} · thumbs: {thumbs} · {}",
        status_ai_models()
    )
}

fn draw_delete_confirm(frame: &mut Frame, root: &Path, rels: &[String]) {
    let area = frame.area();
    let width = area.width.min(52).max(28.min(area.width));
    let height = 5.min(area.height);
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);
    let text = format!(
        "{}\n{}",
        delete::confirm_prompt_at(root, rels),
        delete::CONFIRM_HINT
    );
    frame.render_widget(
        Paragraph::new(text).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Delete")
                .border_style(Style::default().fg(Color::LightRed)),
        ),
        popup,
    );
}

const CREDENTIALS_BIND: &str = "Esc — back to grid";

fn credentials_setup_body() -> String {
    let path = credentials::credentials_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.config/hallward/credentials".into());
    format!(
        "Photos are sent to OpenRouter and may use paid quota.\n\n\
         Run:\n  hallward credentials set\n\n\
         Or set HALLWARD_OPENROUTER_API_KEY.\n\n\
         Credentials file:\n  {path}"
    )
}

fn credentials_insecure_body(chmod_hint: &str) -> String {
    format!(
        "Credentials file permissions are too open.\n\nRun:\n  {chmod_hint}"
    )
}

fn draw_credentials_overlay(frame: &mut Frame, app: &App) {
    let Some(overlay) = app.credentials_overlay.as_ref() else {
        return;
    };
    let area = frame.area();
    let width = area.width.min(64).max(36.min(area.width));
    let issue = credentials::credential_issue();
    let body = match issue {
        Some(CredentialIssue::InsecurePermissions { chmod_hint }) => {
            credentials_insecure_body(&chmod_hint)
        }
        Some(CredentialIssue::Missing) | None => credentials_setup_body(),
    };
    let height = if overlay.error.is_some() { 14 } else { 12 };
    let height = height.min(area.height);
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Ask AI")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(body)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(CREDENTIALS_BIND).alignment(Alignment::Center),
        chunks[1],
    );

    if let Some(error) = &overlay.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::LightRed)),
            chunks[2],
        );
    }
}

fn handle_credentials_overlay_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Esc && key.modifiers.is_empty() {
        app.dismiss_credentials_overlay();
    }
}

const CREATE_FOLDER_BIND: &str = "Enter save · Esc cancel";

fn create_folder_parent(app: &App) -> PathBuf {
    if let Some(folder) = app.selected_folder() {
        if folder.kind == Kind::Collection
            && (app.focus == Focus::Grid || folder.children.is_empty())
        {
            return folder.relpath.clone();
        }
    }
    if app.miller_focus == 0 {
        return PathBuf::new();
    }
    let cols = app.miller_columns();
    let parent_col = app.miller_focus.saturating_sub(1);
    let sel = app.cursor.get(parent_col).copied().unwrap_or(0);
    cols.get(parent_col)
        .and_then(|c| c.get(sel))
        .map(|f| f.relpath.clone())
        .unwrap_or_default()
}

fn rescan_tree_focusing(app: &mut App, focus_rel: Option<&str>) {
    match library::scan_tree(&app.root) {
        Ok(tree) => {
            app.full_tree = tree;
            app.view_tree = if app.query.is_empty() {
                app.full_tree.clone()
            } else {
                search::filter_tree(&app.full_tree, &app.query)
            };
            if let Some(rel) = focus_rel {
                focus_folder_path(app, Path::new(rel));
            } else {
                clamp_cursor(app);
            }
        }
        Err(e) => app.status = format!("tree scan failed: {e:#}"),
    }
    app.reload_photos();
}

fn focus_folder_path(app: &mut App, relpath: &Path) {
    use std::path::Component;
    let components: Vec<String> = relpath
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if components.is_empty() {
        return;
    }

    let mut cursor = vec![0usize];
    let mut parent: &Folder = &app.view_tree;
    let mut depth = 0usize;

    for (i, comp) in components.iter().enumerate() {
        let Some(idx) = parent.children.iter().position(|c| c.name == *comp) else {
            break;
        };
        cursor.resize(depth + 1, 0);
        cursor[depth] = idx;
        if i == components.len() - 1 {
            app.cursor = cursor;
            app.miller_focus = depth;
            app.focus = Focus::Miller;
            clamp_cursor(app);
            return;
        }
        let child = &parent.children[idx];
        if child.kind != Kind::Collection {
            break;
        }
        depth += 1;
        parent = child;
    }
}

fn draw_create_folder_overlay(frame: &mut Frame, app: &App) {
    let Some(overlay) = app.create_folder_overlay.as_ref() else {
        return;
    };
    let area = frame.area();
    let width = area.width.min(48).max(28.min(area.width));
    let height = if overlay.error.is_some() { 9 } else { 8 };
    let height = height.min(area.height);
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("New folder")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Folder name").alignment(Alignment::Center),
        chunks[0],
    );

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let input_inner = input_block.inner(chunks[1]);
    frame.render_widget(input_block, chunks[1]);
    frame.render_widget(
        Paragraph::new(overlay.input.as_str()).alignment(Alignment::Center),
        input_inner,
    );

    frame.render_widget(
        Paragraph::new(CREATE_FOLDER_BIND).alignment(Alignment::Center),
        chunks[2],
    );

    if let Some(error) = &overlay.error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::LightRed)),
            chunks[3],
        );
    }
}

fn handle_create_folder_overlay_key(app: &mut App, key: KeyEvent) {
    match classify_create_folder_key(key) {
        CreateFolderKey::Cancel => {
            app.create_folder_overlay = None;
        }
        CreateFolderKey::Save => {
            let Some(overlay) = app.create_folder_overlay.take() else {
                return;
            };
            let name = overlay.input.trim().to_string();
            match library::create_folder(&app.root, &overlay.parent_relpath, &name) {
                Ok(relpath) => {
                    let rel_str = relpath.to_string_lossy().replace('\\', "/");
                    rescan_tree_focusing(app, Some(&rel_str));
                    app.status = format!("created folder {name}");
                    app.focus = Focus::Miller;
                }
                Err(e) => {
                    app.create_folder_overlay = Some(CreateFolderOverlay {
                        parent_relpath: overlay.parent_relpath,
                        input: overlay.input,
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }
        CreateFolderKey::Backspace => {
            let Some(overlay) = app.create_folder_overlay.as_mut() else {
                return;
            };
            overlay.input.pop();
            overlay.error = None;
        }
        CreateFolderKey::Char(c) => {
            let Some(overlay) = app.create_folder_overlay.as_mut() else {
                return;
            };
            overlay.input.push(c);
            overlay.error = None;
        }
        CreateFolderKey::Ignore => {}
    }
}

fn append_create_folder_input(app: &mut App, text: &str) {
    let Some(overlay) = app.create_folder_overlay.as_mut() else {
        return;
    };
    for ch in text.chars().filter(|c| !c.is_control()) {
        overlay.input.push(ch);
    }
    overlay.error = None;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateFolderKey {
    Save,
    Cancel,
    Backspace,
    Char(char),
    Ignore,
}

fn classify_create_folder_key(key: KeyEvent) -> CreateFolderKey {
    match key.code {
        KeyCode::Enter if key.modifiers.is_empty() => CreateFolderKey::Save,
        KeyCode::Esc if key.modifiers.is_empty() => CreateFolderKey::Cancel,
        KeyCode::Backspace if key.modifiers.is_empty() => CreateFolderKey::Backspace,
        KeyCode::Char(c) if allows_text_char(key.modifiers) => CreateFolderKey::Char(c),
        _ => CreateFolderKey::Ignore,
    }
}

fn allows_text_char(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let viewer_name = viewer::detect()
        .map(|v| v.bin().to_string())
        .unwrap_or_else(|| "no viewer".into());
    let video_name = viewer::detect_video_player()
        .map(|v| v.bin().to_string())
        .unwrap_or_else(|| "no player".into());
    let thumbs = protocol_name(app.picker.as_ref());
    let text = format!(
        "{}\n{}",
        app.status,
        status_tools_line(&viewer_name, &video_name, thumbs)
    );
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Status")),
        area,
    );
}

/// Character-cell size whose inner area (inside the border) is as square as the font allows.
fn square_cell_size(font_w: u16, font_h: u16, inner_h: u16) -> (u16, u16) {
    let font_w = font_w.max(1) as u32;
    let font_h = font_h.max(1) as u32;
    let inner_h = inner_h.max(1) as u32;
    let inner_w = (inner_h * font_h + font_w / 2) / font_w;
    let inner_w = inner_w.max(1) as u16;
    let inner_h = inner_h as u16;
    (
        inner_w.saturating_add(2).max(3),
        inner_h.saturating_add(2).max(3),
    )
}

//---------------------------------------------------------------------------
// Reusable headless e2e test API. Production `run()` is unaffected: it keeps
// using `RealViewerOpener`, `SystemClock`, and `post_openrouter`.
//---------------------------------------------------------------------------

impl App {
    /// Headless constructor for integration tests: fixed font-size picker
    /// (no TTY query like `Picker::from_query_stdio`), injectable viewer,
    /// clock, and Ask AI transport.
    pub fn new_for_test(
        root: PathBuf,
        viewer: Arc<dyn ViewerOpener>,
        clock: Arc<dyn Clock>,
        ask_post: PostFn,
    ) -> Result<Self> {
        let mut app = Self::new_with(root, viewer, clock)?;
        app.ask_post = ask_post;
        app.picker = Some(Picker::from_fontsize((10, 20)));
        Ok(app)
    }

    /// Drive one key event through the same `handle_key` as production.
    /// Returns `true` when the app requests quit (q / Ctrl-C).
    pub fn test_key(&mut self, key: KeyEvent, driver: &mut dyn TerminalDriver) -> Result<bool> {
        handle_key(self, key, driver)
    }

    /// Drive one left mouse click at the given screen cell.
    pub fn test_mouse(
        &mut self,
        column: u16,
        row: u16,
        driver: &mut dyn TerminalDriver,
    ) -> Result<()> {
        handle_mouse(
            self,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            driver,
        )
    }

    /// Click the grid cell showing photo `idx`. Redraw first so hit regions
    /// are populated. Returns `false` when the photo is not on screen.
    pub fn click_grid_photo(
        &mut self,
        idx: usize,
        driver: &mut dyn TerminalDriver,
    ) -> Result<bool> {
        let Some(hit) = self.hit.grid else {
            return Ok(false);
        };
        if idx >= self.photos.len() || idx < hit.scroll_first {
            return Ok(false);
        }
        let n = idx - hit.scroll_first;
        let col = n % hit.cols.max(1);
        let row = n / hit.cols.max(1);
        if hit.cell_w == 0 || hit.cell_h == 0 {
            return Ok(false);
        }
        let column = hit.inner.x + col as u16 * hit.cell_w + hit.cell_w / 2;
        let row = hit.inner.y + row as u16 * hit.cell_h + hit.cell_h / 2;
        if !hit.inner.contains(Position { x: column, y: row }) {
            return Ok(false);
        }
        self.test_mouse(column, row, driver)?;
        Ok(true)
    }

    /// Pump one pending Ask AI outcome (the job runs on a background thread).
    pub fn pump_ask(&mut self) {
        self.poll_ask();
    }

    pub fn redraw_for_test(&mut self, driver: &mut dyn TerminalDriver) -> Result<()> {
        driver.redraw(self)
    }

    // -- semantic state readers for assertions --------------------------

    pub fn focus_name(&self) -> &'static str {
        match self.focus {
            Focus::Search => "search",
            Focus::Miller => "miller",
            Focus::Grid => "grid",
        }
    }

    pub fn grid_index(&self) -> usize {
        self.grid_idx
    }

    pub fn photo_count(&self) -> usize {
        self.photos.len()
    }

    pub fn photo_rels(&self) -> Vec<String> {
        self.photos.iter().map(|p| p.relpath.clone()).collect()
    }

    pub fn marked_sorted(&self) -> Vec<String> {
        let mut marked: Vec<String> = self.marked.iter().cloned().collect();
        marked.sort();
        marked
    }

    pub fn is_marked(&self, rel: &str) -> bool {
        self.marked.contains(rel)
    }

    pub fn status_text(&self) -> String {
        self.status.clone()
    }

    pub fn query_text(&self) -> String {
        self.query.clone()
    }

    pub fn ask_prompt_text(&self) -> String {
        self.ask.prompt.clone()
    }

    pub fn ask_reply_text(&self) -> Option<String> {
        match &self.ask.reply {
            Some(AskReply::Text(t) | AskReply::Error(t)) => Some(t.clone()),
            None => None,
        }
    }

    pub fn ask_waiting(&self) -> bool {
        self.ask.waiting()
    }

    pub fn is_ask_active(&self) -> bool {
        self.ask_active()
    }

    pub fn clipboard_rels(&self) -> Vec<String> {
        self.clipboard
            .as_ref()
            .map(|c| {
                let mut rels = c.rels.clone();
                rels.sort();
                rels
            })
            .unwrap_or_default()
    }

    pub fn pending_delete_rels(&self) -> Vec<String> {
        self.pending_delete.clone().unwrap_or_default()
    }

    pub fn current_album_name(&self) -> Option<String> {
        self.current_album().map(|f| f.display_name().to_string())
    }

    pub fn clock_now(&self) -> Instant {
        self.clock.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_cell_matches_1_by_2_font() {
        // 10×20px cells: 6 rows → 12 cols of image, plus a 1-cell border.
        assert_eq!(square_cell_size(10, 20, 6), (14, 8));
    }

    #[test]
    fn square_cell_matches_taller_font() {
        // Ghostty-like 8×22: 6×22 / 8 = 16.5 → 17 image columns.
        assert_eq!(square_cell_size(8, 22, 6), (19, 8));
    }

    #[test]
    fn miller_click_maps_row_with_scroll_offset() {
        let hit = MillerHit {
            inner: Rect::new(1, 4, 20, 10),
            scroll_offset: 3,
            item_count: 8,
        };
        let pos = Position { x: 2, y: 5 };
        assert_eq!(miller_row_at(hit, pos), Some(4));
    }

    #[test]
    fn miller_click_ignores_border_and_empty_lists() {
        let hit = MillerHit {
            inner: Rect::new(1, 4, 20, 10),
            scroll_offset: 0,
            item_count: 2,
        };
        assert_eq!(miller_row_at(hit, Position { x: 0, y: 5 }), None);
        assert_eq!(
            miller_row_at(
                MillerHit {
                    item_count: 0,
                    ..hit
                },
                Position { x: 2, y: 5 }
            ),
            None
        );
    }

    #[test]
    fn grid_click_maps_scrolled_cell() {
        let hit = GridHit {
            inner: Rect::new(30, 2, 40, 20),
            cell_w: 10,
            cell_h: 8,
            cols: 3,
            scroll_first: 6,
        };
        // column 1, row 1 → local index 4 → photo 10
        let pos = Position { x: 41, y: 10 };
        assert_eq!(grid_index_at(hit, pos, 20), Some(10));
    }

    #[test]
    fn grid_click_ignores_out_of_range_cells() {
        let hit = GridHit {
            inner: Rect::new(0, 0, 30, 16),
            cell_w: 10,
            cell_h: 8,
            cols: 3,
            scroll_first: 18,
        };
        assert_eq!(grid_index_at(hit, Position { x: 5, y: 1 }, 20), Some(18));
        assert_eq!(grid_index_at(hit, Position { x: 25, y: 9 }, 20), None);
    }

    #[test]
    fn double_click_requires_same_index_within_threshold() {
        let now = Instant::now();
        let last = Some((2, now - Duration::from_millis(200)));
        assert!(is_double_click(last, 2, now, DOUBLE_CLICK));
        assert!(!is_double_click(last, 3, now, DOUBLE_CLICK));
        assert!(!is_double_click(
            Some((2, now - Duration::from_millis(600))),
            2,
            now,
            DOUBLE_CLICK
        ));
    }

    fn photo(rel: &str) -> Photo {
        Photo {
            relpath: rel.into(),
            album: "album".into(),
            filename: rel.into(),
            mtime: 0,
            size: 0,
            captured_at: None,
            camera: None,
            width: None,
            height: None,
        }
    }

    fn album() -> Vec<Photo> {
        vec![photo("a.jpg"), photo("b.jpg"), photo("c.jpg")]
    }

    #[test]
    fn viewer_playlist_empty_marks_is_full_album() {
        let photos = album();
        let marked = HashSet::new();
        let (rels, start) = viewer_playlist(&photos, &marked, 2);
        assert_eq!(rels, vec!["a.jpg", "b.jpg", "c.jpg"]);
        assert_eq!(start, 2);
    }

    #[test]
    fn viewer_playlist_marks_keep_album_order_and_remap_start() {
        let photos = album();
        let marked = HashSet::from(["c.jpg".into(), "a.jpg".into()]);
        let (rels, start) = viewer_playlist(&photos, &marked, 2);
        assert_eq!(rels, vec!["a.jpg", "c.jpg"]);
        assert_eq!(start, 1);
    }

    #[test]
    fn viewer_playlist_unmarked_focus_starts_at_first_mark() {
        let photos = album();
        let marked = HashSet::from(["c.jpg".into(), "a.jpg".into()]);
        let (rels, start) = viewer_playlist(&photos, &marked, 1);
        assert_eq!(rels, vec!["a.jpg", "c.jpg"]);
        assert_eq!(start, 0);
    }

    #[test]
    fn toggle_mark_adds_then_removes_without_touching_others() {
        let mut marked = HashSet::from(["a.jpg".into()]);
        toggle_mark(&mut marked, "b.jpg");
        assert_eq!(marked, HashSet::from(["a.jpg".into(), "b.jpg".into()]));
        toggle_mark(&mut marked, "b.jpg");
        assert_eq!(marked, HashSet::from(["a.jpg".into()]));
    }

    #[test]
    fn apply_grid_click_unmarks_only_the_clicked_mark() {
        let mut marked = HashSet::from(["a.jpg".into(), "b.jpg".into(), "c.jpg".into()]);
        apply_grid_click(&mut marked, Some("b.jpg"));
        assert_eq!(marked, HashSet::from(["a.jpg".into(), "c.jpg".into()]));
    }

    #[test]
    fn apply_grid_click_outside_clears_all_marks() {
        let mut marked = HashSet::from(["a.jpg".into(), "b.jpg".into()]);
        apply_grid_click(&mut marked, Some("c.jpg"));
        assert!(marked.is_empty());
        marked = HashSet::from(["a.jpg".into()]);
        apply_grid_click(&mut marked, None);
        assert!(marked.is_empty());
    }

    #[test]
    fn restore_mark_after_click_unmark_keeps_the_set() {
        let mut marked = HashSet::from(["a.jpg".into(), "b.jpg".into()]);
        apply_grid_click(&mut marked, Some("a.jpg"));
        assert_eq!(marked, HashSet::from(["b.jpg".into()]));
        restore_mark_for_double_click(&mut marked, "a.jpg");
        assert_eq!(marked, HashSet::from(["a.jpg".into(), "b.jpg".into()]));
        let photos = album();
        let (rels, start) = viewer_playlist(&photos, &marked, 0);
        assert_eq!(rels, vec!["a.jpg", "b.jpg"]);
        assert_eq!(start, 0);
    }

    #[test]
    fn grid_cell_focused_only_when_grid_pane_is_active() {
        assert!(grid_cell_focused(Focus::Grid, 0, 0));
        assert!(!grid_cell_focused(Focus::Miller, 0, 0));
        assert!(!grid_cell_focused(Focus::Search, 0, 0));
        assert!(!grid_cell_focused(Focus::Grid, 0, 1));
    }

    #[test]
    fn grid_cell_border_focus_wins_over_mark() {
        let focused = grid_cell_border_style(CellBorder::Focus);
        assert_eq!(focused.fg, Some(Color::Yellow));
        assert!(focused.add_modifier.contains(Modifier::BOLD));
        let marked = grid_cell_border_style(CellBorder::Marked);
        assert_eq!(marked.fg, Some(Color::Cyan));
        let idle = grid_cell_border_style(CellBorder::Idle);
        assert_eq!(idle.fg, Some(Color::Rgb(40, 40, 40)));
    }

    #[test]
    fn cell_border_copy_is_double_cut_is_dashed() {
        assert_eq!(
            cell_border(false, Some(ClipboardOp::Copy), true),
            CellBorder::Copied
        );
        assert_eq!(
            cell_border(false, Some(ClipboardOp::Cut), true),
            CellBorder::Cut
        );
        assert_eq!(
            cell_border(true, Some(ClipboardOp::Cut), true),
            CellBorder::Focus
        );
        assert_eq!(DASHED_BORDER.horizontal_top, "╌");
        assert_eq!(DASHED_BORDER.vertical_left, "╎");
        let _copied = grid_cell_block(CellBorder::Copied);
        let _cut = grid_cell_block(CellBorder::Cut);
    }

    #[test]
    fn album_heading_is_the_album_name() {
        assert_eq!(album_grid_heading("Trip"), "Trip");
    }

    #[test]
    fn album_footer_index_when_gallery_is_focused() {
        assert_eq!(album_grid_footer(1, 3, 0, None, true), Some("1/3".into()));
        assert_eq!(
            album_grid_footer(1, 3, 3, None, true),
            Some("1/3 · 3 marked".into())
        );
        assert_eq!(album_grid_footer(1, 3, 0, None, false), None);
        assert_eq!(
            album_grid_footer(1, 3, 3, None, false),
            Some("3 marked".into())
        );
        assert_eq!(
            album_grid_footer(1, 3, 0, Some("2 copied".into()), true),
            Some("1/3 · 2 copied".into())
        );
        assert_eq!(
            album_grid_footer(1, 3, 2, Some("2 cut".into()), true),
            Some("1/3 · 2 marked · 2 cut".into())
        );
    }

    #[test]
    fn album_media_summary_counts_photos_and_videos() {
        assert_eq!(
            album_media_summary(&[]),
            vec!["0 photos".to_string(), "0 videos".into()]
        );
        assert_eq!(
            album_media_summary(&[photo("a.jpg"), photo("b.HEIC"), photo("c.png")]),
            vec!["3 photos".to_string(), "0 videos".into()]
        );
        assert_eq!(
            album_media_summary(&[photo("clip.MOV")]),
            vec!["1 video".to_string()]
        );
        assert_eq!(
            album_media_summary(&[photo("a.jpg"), photo("clip.mp4"), photo("b.MOV")]),
            vec!["1 photo".to_string(), "2 videos".into()]
        );
    }

    #[test]
    fn status_hint_describes_mark_and_enter() {
        assert!(STATUS_HINT.contains("Space mark"));
        assert!(STATUS_HINT.contains("Esc unmark"));
        assert!(STATUS_HINT.contains("Enter opens"));
        assert!(STATUS_HINT.contains("Shift+Tab search"));
        assert!(STATUS_HINT.contains("d delete"));
        assert!(STATUS_HINT.contains("c copy"));
        assert!(STATUS_HINT.contains("x cut"));
        assert!(STATUS_HINT.contains("p paste"));
    }

    #[test]
    fn create_folder_overlay_keys_save_cancel_and_type() {
        assert_eq!(
            classify_create_folder_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            CreateFolderKey::Save
        );
        assert_eq!(
            classify_create_folder_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            CreateFolderKey::Cancel
        );
        assert_eq!(
            classify_create_folder_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            CreateFolderKey::Char('a')
        );
        assert_eq!(
            classify_create_folder_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            CreateFolderKey::Char('A')
        );
        assert_eq!(
            classify_create_folder_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            CreateFolderKey::Ignore
        );
    }

    #[test]
    fn credentials_overlay_esc_dismisses() {
        assert_eq!(
            KeyCode::Esc,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE).code
        );
        let body = credentials_setup_body();
        assert!(body.contains("hallward credentials set"));
        assert!(body.contains("HALLWARD_OPENROUTER_API_KEY"));
        let insecure = credentials_insecure_body("chmod 600 ~/.config/hallward/credentials");
        assert!(insecure.contains("chmod 600"));
        assert!(!insecure.contains("hallward credentials set"));
    }

    #[test]
    fn confirm_overlay_keys_are_yes_no_or_ignored() {
        assert_eq!(classify_confirm_key(KeyCode::Char('y')), ConfirmKey::Yes);
        assert_eq!(classify_confirm_key(KeyCode::Char('Y')), ConfirmKey::Yes);
        assert_eq!(classify_confirm_key(KeyCode::Char('n')), ConfirmKey::No);
        assert_eq!(classify_confirm_key(KeyCode::Char('N')), ConfirmKey::No);
        assert_eq!(classify_confirm_key(KeyCode::Esc), ConfirmKey::No);
        assert_eq!(classify_confirm_key(KeyCode::Char('d')), ConfirmKey::Ignore);
        assert_eq!(classify_confirm_key(KeyCode::Enter), ConfirmKey::Ignore);
    }

    #[test]
    fn esc_clears_marks_then_clipboard_then_search() {
        assert_eq!(classify_esc(true, true, true), EscTarget::Marks);
        assert_eq!(classify_esc(false, true, true), EscTarget::Clipboard);
        assert_eq!(classify_esc(false, false, true), EscTarget::Query);
        assert_eq!(classify_esc(false, false, false), EscTarget::None);
    }

    #[test]
    fn miller_folder_target_is_the_focused_folder_when_the_tree_has_focus() {
        assert_eq!(
            miller_folder_target(Focus::Miller, Some("2024/Rome".into())).as_deref(),
            Some("2024/Rome")
        );
        assert_eq!(miller_folder_target(Focus::Grid, Some("Rome".into())), None);
        assert_eq!(
            miller_folder_target(Focus::Search, Some("Rome".into())),
            None
        );
        assert_eq!(miller_folder_target(Focus::Miller, Some(".".into())), None);
        assert_eq!(miller_folder_target(Focus::Miller, None), None);
    }

    #[test]
    fn folder_dest_key_uses_dot_for_the_library_root() {
        assert_eq!(folder_dest_key(Path::new("")), ".");
        assert_eq!(folder_dest_key(Path::new("2024")), "2024");
        assert_eq!(folder_dest_key(Path::new("2024/Rome")), "2024/Rome");
    }

    #[test]
    fn status_tools_line_includes_openrouter_models() {
        let (ask, edit) = credentials::effective_models();
        assert_eq!(
            status_tools_line("Preview", "mpv", "kitty"),
            format!("viewer: Preview · video: mpv · thumbs: kitty · ask: {ask} · edit: {edit}")
        );
    }

    #[test]
    fn shift_tab_is_backtab_or_tab_with_shift() {
        assert!(is_shift_tab(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::NONE
        )));
        assert!(is_shift_tab(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT
        )));
        assert!(!is_shift_tab(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE
        )));
        assert!(!is_shift_tab(&KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn shift_tab_focuses_search_from_library_folders_and_gallery() {
        for from in [Focus::Miller, Focus::Grid, Focus::Search] {
            assert_eq!(shift_tab_focus(from), Focus::Search);
        }
    }

    #[test]
    fn ask_ai_pane_title_switches_from_search() {
        assert_eq!(
            search_pane_title(true, true),
            "Ask AI (Enter send · Tab tree · Esc clear)"
        );
        assert_eq!(
            search_pane_title(true, false),
            "Ask AI (Shift+Tab · type to prompt)"
        );
        assert_eq!(
            search_pane_title(false, true),
            "Search (Tab tree · Esc clear)"
        );
        assert_eq!(search_pane_title(false, false), "Search (Shift+Tab)");
    }

    #[test]
    fn ask_ai_lines_are_two_paragraphs() {
        let lines = ask_ai_lines("describe this", Some("Waiting..."), false, false);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "describe this");
        assert_eq!(lines[1].spans.len(), 0);
        assert_eq!(lines[2].spans[0].content, "Waiting...");
    }

    #[test]
    fn video_only_marks_do_not_activate_ask_ai() {
        let photos = vec![photo("clip.mov"), photo("a.jpg")];
        let video_only = HashSet::from(["clip.mov".into()]);
        let stills = ai::marked_still_rels(&photos, &video_only);
        assert!(stills.is_empty());
        assert!(!ai::ask_ai_active(&stills));
        let mixed = HashSet::from(["clip.mov".into(), "a.jpg".into()]);
        assert!(ai::ask_ai_active(&ai::marked_still_rels(&photos, &mixed)));
    }

    #[test]
    fn typing_with_ask_ai_does_not_change_query() {
        let mut query = "rome".to_string();
        let mut prompt = String::new();
        type_into_ask_prompt(&mut prompt, false, 'w');
        assert_eq!(prompt, "w");
        assert_eq!(query, "rome");
        type_into_ask_prompt(&mut prompt, true, 'x');
        assert_eq!(prompt, "w");
        query.push('x');
        assert_eq!(query, "romex");
    }

    #[test]
    fn tab_from_tree_focuses_ask_ai_even_when_prompt_is_empty() {
        assert!(library_tab_focuses_ask(true, false));
        assert!(library_tab_focuses_ask(true, true));
        assert!(library_tab_focuses_ask(false, true));
        assert!(!library_tab_focuses_ask(false, false));
    }

    #[test]
    fn ask_field_tab_keeps_thread_esc_clears() {
        assert_eq!(
            classify_ask_field_key(KeyCode::Tab, false),
            AskFieldKey::ExitKeep
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Esc, false),
            AskFieldKey::ExitClear
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Esc, true),
            AskFieldKey::ExitClear
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Enter, false),
            AskFieldKey::Send
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Backspace, true),
            AskFieldKey::Ignore
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Char('a'), true),
            AskFieldKey::Ignore
        );
        assert_eq!(
            classify_ask_field_key(KeyCode::Backspace, false),
            AskFieldKey::Backspace
        );
    }

    #[test]
    fn changed_selection_invalidates_stale_ask_output() {
        assert!(ask_outcome_is_stale(2, 1));
        assert!(!ask_outcome_is_stale(2, 2));
    }

    #[test]
    fn saved_edit_focuses_the_new_sibling() {
        let photos = vec![
            photo("album/a.jpg"),
            photo("album/a-edited.png"),
            photo("album/b.jpg"),
        ];
        assert_eq!(
            focused_photo_index(&photos, 0, Some("album/a-edited.png")),
            1
        );
        assert_eq!(focused_photo_index(&photos, 2, None), 2);
        assert_eq!(focused_photo_index(&[], 3, None), 0);
        assert_eq!(focused_photo_index(&photos, 9, None), 2);
    }

    #[test]
    fn ask_response_scrolling_clamps_to_content() {
        assert_eq!(apply_ask_scroll(0, AskFieldKey::ScrollUp, 4, 10), 0);
        assert_eq!(apply_ask_scroll(0, AskFieldKey::ScrollDown, 4, 10), 1);
        assert_eq!(apply_ask_scroll(8, AskFieldKey::PageDown, 4, 10), 10);
        assert_eq!(apply_ask_scroll(3, AskFieldKey::PageUp, 4, 10), 0);
        let area = Rect::new(0, 0, 20, 8);
        let long = "line\n".repeat(20);
        assert!(ask_scroll_max("prompt", Some(&long), area) > 0);
    }
}
