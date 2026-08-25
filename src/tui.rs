use std::collections::HashMap;
use std::io::Stdout;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
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
use crate::search;
use crate::thumbs;
use crate::viewer;

const CELL_W: u16 = 16;
const CELL_H: u16 = 8;
const STATUS_HINT: &str = "arrows move · type to search · Enter opens viewer · r reindex · q quit";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Search,
    Miller,
    Grid,
}

pub fn run(root: PathBuf) -> Result<()> {
    let mut app = App::new(root)?;
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, &mut app);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
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
    graphics: bool,
    protocols: HashMap<String, StatefulProtocol>,
    status: String,
}

impl App {
    fn new(root: PathBuf) -> Result<Self> {
        let conn = catalog::open(&root, false)?;
        let full_tree = library::scan_tree(&root)?;
        let view_tree = full_tree.clone();
        let (picker, graphics) = match Picker::from_query_stdio() {
            Ok(p) => {
                let ok = p.protocol_type() != ProtocolType::Halfblocks;
                (Some(p), ok)
            }
            Err(_) => (None, false),
        };
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
            picker,
            graphics,
            protocols: HashMap::new(),
            status: STATUS_HINT.into(),
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
        if !self.graphics || self.protocols.contains_key(rel) {
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
            if !app.query.is_empty() {
                app.query.clear();
                app.apply_query();
            }
        }
        KeyCode::Tab => {
            if !app.query.is_empty() {
                app.focus = Focus::Search;
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
        KeyCode::Enter => open_viewer(app, terminal)?,
        _ => {}
    }
    Ok(false)
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
            app.status = format!(
                "indexed {} files (updated {}, skipped {}, removed {})",
                stats.total, stats.added_or_updated, stats.skipped, stats.removed
            );
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
    let rels: Vec<String> = app.photos.iter().map(|p| p.relpath.clone()).collect();
    let files = viewer::abs_files(&app.root, &rels);
    let start = app.grid_idx;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    let open = viewer::open(&files, start);
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
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
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
        ])
        .split(area);

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

fn draw_miller_col(frame: &mut Frame, app: &App, col: usize, area: Rect) {
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
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        );
    let mut state = ListState::default();
    state.select(app.cursor.get(col).copied());
    frame.render_stateful_widget(list, area, &mut state);
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
    let pos = if app.photos.is_empty() {
        0
    } else {
        app.grid_idx + 1
    };
    let title = format!("Album {album_name}  {pos}/{}", app.photos.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.current_album().is_none() {
        frame.render_widget(
            Paragraph::new("Select an album with the right arrow.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    if app.photos.is_empty() {
        frame.render_widget(Paragraph::new("No still images in this album."), inner);
        return;
    }
    if !app.graphics {
        let name = app
            .selected_photo()
            .map(|p| p.filename.as_str())
            .unwrap_or("");
        frame.render_widget(
            Paragraph::new(format!(
                "No Kitty/Sixel graphics in this terminal.\nUse Kitty or Ghostty for the thumbnail grid.\nEnter still opens the external viewer.\n\nSelected: {name}"
            ))
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let cols = (inner.width / CELL_W).max(1) as usize;
    let rows = (inner.height / CELL_H).max(1) as usize;
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
            x: inner.x + c * CELL_W,
            y: inner.y + r * CELL_H,
            width: CELL_W.min(inner.width.saturating_sub(c * CELL_W)),
            height: CELL_H.min(inner.height.saturating_sub(r * CELL_H)),
        };
        if cell.width < 3 || cell.height < 3 {
            continue;
        }
        let selected = idx == app.grid_idx;
        let cell_block = Block::default()
            .borders(Borders::ALL)
            .border_style(if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(40, 40, 40))
            });
        let img_area = cell_block.inner(cell);
        frame.render_widget(cell_block, cell);
        if let Some(proto) = app.protocols.get_mut(rel) {
            frame.render_stateful_widget(StatefulImage::default(), img_area, proto);
        }
    }
}

fn draw_exif(frame: &mut Frame, app: &App, area: Rect) {
    let lines = if let Some(p) = app.selected_photo() {
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
    } else {
        vec![Line::from("No photo selected")]
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("EXIF")),
        area,
    );
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let viewer_name = viewer::detect()
        .map(|v| v.bin().to_string())
        .unwrap_or_else(|| "no viewer".into());
    let text = format!("{}\nviewer: {viewer_name}", app.status);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Status")),
        area,
    );
}
