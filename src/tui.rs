use std::collections::{HashMap, HashSet};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use rusqlite::Connection;

use crate::catalog::{self, Photo};
use crate::index;
use crate::library::{self, Folder, Kind};
use crate::media::{is_image, is_video};
use crate::search;
use crate::thumbs;
use crate::viewer;

/// Inner image height in rows. Width is derived from the terminal font so the photo is square.
const CELL_INNER_H: u16 = 6;
const STATUS_HINT: &str =
    "arrows move · Space mark · Esc unmark · Enter opens · click toggles mark · double-click opens · type to search · r reindex · q quit";
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Search,
    Miller,
    Grid,
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
    disable_raw_mode()?;
    leave_tui(terminal.backend_mut())?;
    terminal.show_cursor()?;
    result
}

fn enter_tui<W: std::io::Write>(out: &mut W) -> Result<()> {
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

fn leave_tui<W: std::io::Write>(out: &mut W) -> Result<()> {
    execute!(out, DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}

struct App {
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
}

impl App {
    fn new(root: PathBuf) -> Result<Self> {
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
        };
        app.reload_photos();
        Ok(app)
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

    fn album_key(folder: &Folder) -> String {
        let s = folder.relpath.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            ".".into()
        } else {
            s
        }
    }

    fn reload_photos(&mut self) {
        self.photos = if let Some(album) = self.current_album() {
            let key = Self::album_key(album);
            catalog::photos_in_album(&self.conn, &key).unwrap_or_default()
        } else {
            Vec::new()
        };
        if self.photos.is_empty() {
            self.grid_idx = 0;
        } else if self.grid_idx >= self.photos.len() {
            self.grid_idx = self.photos.len() - 1;
        }
        self.grid_scroll = 0;
        self.marked.clear();
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
    loop {
        terminal.draw(|f| draw(f, app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(app, key, terminal)? {
                    break;
                }
            }
            Event::Mouse(mouse) => {
                handle_mouse(app, mouse, terminal)?;
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.focus == Focus::Search {
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
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('r') => reindex(app, terminal)?,
        KeyCode::Esc => {
            if !app.marked.is_empty() {
                app.marked.clear();
            } else if !app.query.is_empty() {
                app.query.clear();
                app.apply_query();
            }
        }
        KeyCode::Tab => {
            if !app.query.is_empty() {
                app.focus = Focus::Search;
            }
        }
        KeyCode::Enter => open_viewer(app, terminal)?,
        KeyCode::Char(' ') => {
            if app.focus == Focus::Grid {
                if let Some(p) = app.photos.get(app.grid_idx) {
                    toggle_mark(&mut app.marked, &p.relpath);
                }
            }
        }
        KeyCode::Char(c) if !c.is_control() => {
            app.query.push(c);
            app.apply_query();
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

fn handle_mouse(
    app: &mut App,
    mouse: MouseEvent,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
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
            let now = Instant::now();
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

fn grid_cell_border_style(focused: bool, marked: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if marked {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Rgb(40, 40, 40))
    }
}

fn album_grid_heading(album_name: &str) -> String {
    album_name.to_string()
}

fn album_grid_footer(
    pos: usize,
    total: usize,
    marked_count: usize,
    photo_focused: bool,
) -> Option<String> {
    let marked = (marked_count > 0).then(|| format!("{marked_count} marked"));
    if photo_focused {
        let index = format!("{pos}/{total}");
        Some(match marked {
            Some(m) => format!("{index} · {m}"),
            None => index,
        })
    } else {
        marked
    }
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
            if !app.photos.is_empty() {
                app.focus = Focus::Grid;
            }
        }
        Some(Kind::Collection) => {
            let next = app.miller_focus + 1;
            let cols = app.miller_columns();
            if next < cols.len() && !cols[next].is_empty() {
                app.miller_focus = next;
                if app.cursor.len() <= next {
                    app.cursor.resize(next + 1, 0);
                }
            }
        }
        None => {}
    }
}

fn reindex(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    app.status = "indexing…".into();
    terminal.draw(|f| draw(f, app))?;
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
            clamp_cursor(app);
        }
        Err(e) => app.status = format!("{} · tree scan failed: {e:#}", app.status),
    }
    app.protocols.clear();
    app.reload_photos();
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

fn open_viewer(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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

    disable_raw_mode()?;
    leave_tui(terminal.backend_mut())?;
    terminal.show_cursor()?;
    let open = viewer::open(&files, start);
    enable_raw_mode()?;
    enter_tui(terminal.backend_mut())?;
    terminal.clear()?;
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
        ])
        .split(area);

    app.hit.search = chunks[0];
    draw_search(frame, app, chunks[0]);

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
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.focus == Focus::Search {
        "Search (Tab tree · Esc clear)"
    } else {
        "Search (type to filter albums)"
    };
    let style = if app.focus == Focus::Search {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let p = Paragraph::new(app.query.clone())
        .style(style)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(p, area);
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
            ListItem::new(format!("{mark}{}", f.display_name()))
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
        .current_album()
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
    let block = match album_grid_footer(pos, app.photos.len(), app.marked.len(), focused) {
        Some(footer) => block.title_bottom(footer),
        None => block,
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.current_album().is_none() {
        frame.render_widget(
            Paragraph::new("Select an album with the right arrow.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        app.hit.grid = None;
        return;
    }
    if app.photos.is_empty() {
        frame.render_widget(Paragraph::new("No still images in this album."), inner);
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
        let cell_block = Block::default()
            .borders(Borders::ALL)
            .border_style(grid_cell_border_style(focused, marked));
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
    if app.current_album().is_none() {
        vec![Line::from("Select an album")]
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

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let viewer_name = viewer::detect()
        .map(|v| v.bin().to_string())
        .unwrap_or_else(|| "no viewer".into());
    let video_name = viewer::detect_video_player()
        .map(|v| v.bin().to_string())
        .unwrap_or_else(|| "no player".into());
    let thumbs = protocol_name(app.picker.as_ref());
    let text = format!(
        "{}\nviewer: {viewer_name} · video: {video_name} · thumbs: {thumbs}",
        app.status
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
        let focused = grid_cell_border_style(true, true);
        assert_eq!(focused.fg, Some(Color::Yellow));
        assert!(focused.add_modifier.contains(Modifier::BOLD));
        let marked = grid_cell_border_style(false, true);
        assert_eq!(marked.fg, Some(Color::Cyan));
        let idle = grid_cell_border_style(false, false);
        assert_eq!(idle.fg, Some(Color::Rgb(40, 40, 40)));
    }

    #[test]
    fn album_heading_is_the_album_name() {
        assert_eq!(album_grid_heading("Trip"), "Trip");
    }

    #[test]
    fn album_footer_index_when_gallery_is_focused() {
        assert_eq!(album_grid_footer(1, 3, 0, true), Some("1/3".into()));
        assert_eq!(
            album_grid_footer(1, 3, 3, true),
            Some("1/3 · 3 marked".into())
        );
        assert_eq!(album_grid_footer(1, 3, 0, false), None);
        assert_eq!(album_grid_footer(1, 3, 3, false), Some("3 marked".into()));
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
    }
}
