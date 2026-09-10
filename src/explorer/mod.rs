//! Terminal UI (D1): ratatui + crossterm tree explorer with keyboard + mouse
//! navigation, per-kind highlighting (C02/C03/C17) and a live log panel at the
//! bottom (C18: npm/ng output is streamed, nothing fails silently). File icons
//! come from Google's Material Icons font (C19). Selection is delegated to the
//! same serve/watcher path used headlessly, so hot-swap is identical (R7).

pub(crate) mod finder;
mod nav;

pub use nav::{Nav, Row};

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use devicons::{Theme, icon_for_file};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::cli::{self, Cli};
use crate::detect::ComponentKind;
use crate::logs::LogSink;
use crate::pipeline;
use crate::serve::{self, ServeHandle, ServeOptions};

use nav::Entry;

/// Visual highlighting per kind (C03/C16): Angular cyan/magenta, future
/// frameworks in their own hue, dirs bold, everything else plain.
pub fn style_for_kind(kind: ComponentKind) -> Style {
    match kind {
        ComponentKind::AngularStandalone => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ComponentKind::AngularModule => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        ComponentKind::AngularModuleFile => Style::default().fg(Color::Blue),
        ComponentKind::React => Style::default().fg(Color::Yellow),
        ComponentKind::Svelte => Style::default().fg(Color::Red),
        ComponentKind::Vue => Style::default().fg(Color::Green),
        ComponentKind::Other => Style::default(),
    }
}

/// File-type icon per row (C19) via `rust-devicons` (Nerd Font glyphs + the
/// icon's brand color per theme). Directories use the Nerd Font folder glyphs.
/// The terminal needs a Nerd Font to display them; without one they show as
/// boxes, which is purely cosmetic.
fn icon_and_color_for(row: &Row) -> (char, Color) {
    if row.entry.is_dir {
        // nf-fa-folder / nf-fa-folder-open
        let glyph = if row.expanded { '\u{f07c}' } else { '\u{f07b}' };
        return (glyph, Color::Rgb(0x6C, 0x99, 0xD9));
    }
    let file_icon = icon_for_file(&row.entry.path, &Some(Theme::Dark));
    let color = parse_hex(&file_icon.color).unwrap_or(Color::White);
    (file_icon.icon, color)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let digits = hex.trim_start_matches('#');
    if digits.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(Color::Rgb(
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
    ))
}

fn list_item(row: &Row) -> ListItem<'static> {
    let (icon, icon_color) = icon_and_color_for(row);
    let label = if row.entry.is_dir {
        let arrow = if row.expanded { "▾ " } else { "▸ " };
        format!("{arrow}{}/", row.entry.name)
    } else {
        row.entry.name.clone()
    };
    ListItem::new(Line::from(vec![
        Span::raw("  ".repeat(row.depth)),
        Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
        Span::styled(label, style_for_kind(row.entry.kind)),
    ]))
}

fn log_line_style(line: &str) -> Style {
    let lower = line.to_lowercase();
    if lower.contains("error") || lower.contains("failed") || line.contains("ERR!") {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Result of a background `serve::start` sent back to the event loop.
type StartResult = anyhow::Result<ServeHandle>;
type StartSender = tokio::sync::mpsc::UnboundedSender<StartResult>;

/// Status bar hint shown when no other message is active.
const DEFAULT_STATUS: &str =
    "↑/↓ move · / find files · Enter select · PgUp/PgDn logs · l save logs · q quit";

pub struct TuiApp {
    nav: Nav,
    list_state: ListState,
    status: String,
    root: PathBuf,
    selection_file: PathBuf,
    server: Option<ServeHandle>,
    sink: LogSink,
    /// True while `serve::start` runs in the background (no second start).
    starting: bool,
    /// Aborted on quit if the startup is still in flight (drops the ng child).
    start_task: Option<tokio::task::JoinHandle<()>>,
    /// Lines scrolled up from the bottom of the logs panel (0 = live follow).
    log_scroll: usize,
    /// Last rendered logs-panel inner height (for page scrolling).
    log_view_height: usize,
    /// Telescope-style fuzzy finder popup; None when closed.
    finder: Option<finder::Finder>,
}

impl TuiApp {
    pub fn new(root: PathBuf, selection_file: PathBuf, sink: LogSink) -> io::Result<TuiApp> {
        let nav = Nav::open(&root)?;
        Ok(TuiApp {
            nav,
            list_state: ListState::default(),
            status: DEFAULT_STATUS.into(),
            root,
            selection_file,
            server: None,
            sink,
            starting: false,
            start_task: None,
            log_scroll: 0,
            log_view_height: 10,
            finder: None,
        })
    }

    fn sync_list_state(&mut self) {
        self.list_state.select(Some(self.nav.cursor()));
    }

    fn root_label(&self) -> String {
        self.root.display().to_string()
    }
}

fn ui(f: &mut Frame, app: &mut TuiApp) {
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),      // header
            ratatui::layout::Constraint::Min(3),         // tree explorer
            ratatui::layout::Constraint::Percentage(30), // logs panel (C18/C21)
            ratatui::layout::Constraint::Length(1),      // status
        ])
        .split(f.area());

    let header = Line::from(vec![
        Span::styled(
            "render-component",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(app.root_label()),
    ]);
    f.render_widget(ratatui::widgets::Paragraph::new(header), chunks[0]);

    let items: Vec<ListItem<'static>> = app.nav.rows().iter().map(list_item).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" explorer "))
        .highlight_style(Style::default().bg(Color::DarkGray));
    app.sync_list_state();
    f.render_stateful_widget(list, chunks[1], &mut app.list_state);

    // Logs panel: windowed view of the shared sink, newest at the bottom
    // (C18). Scrollable (C21): `log_scroll` counts lines up from the bottom;
    // 0 = live follow (new logs appear automatically).
    let log_height = chunks[2].height.saturating_sub(2) as usize;
    app.log_view_height = log_height.max(1);
    let total = app.sink.len();
    let max_scroll = total.saturating_sub(log_height);
    let scroll = app.log_scroll.min(max_scroll);
    let start = total.saturating_sub(log_height + scroll);
    let log_lines: Vec<Line<'static>> = app
        .sink
        .range(start, start + log_height)
        .into_iter()
        .map(|l| {
            let style = log_line_style(&l);
            Line::from(Span::styled(l, style))
        })
        .collect();
    let title = if scroll > 0 {
        format!(" logs — {} lines up (PgDn to follow) ", scroll)
    } else {
        " logs (PgUp/PgDn scroll · l saves full log) ".to_string()
    };
    let logs = ratatui::widgets::Paragraph::new(log_lines)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(logs, chunks[2]);

    let status_style = if app.status.starts_with("error")
        || app.status.contains("not supported")
        || app.status.contains("invalid")
    {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let status = Line::from(Span::styled(app.status.clone(), status_style));
    f.render_widget(ratatui::widgets::Paragraph::new(status), chunks[3]);

    render_finder(f, app);
}

/// Telescope-style popup: centered box with a `> ` prompt and the fuzzy
/// results, matched chars highlighted, selected row inverted. Rendered last so
/// it floats over the tree and the logs.
fn render_finder(f: &mut Frame, app: &TuiApp) {
    let Some(finder) = &app.finder else { return };

    let area = f.area();
    let width = (area.width * 60 / 100)
        .clamp(30, area.width.saturating_sub(2))
        .max(20);
    let height = (area.height * 40 / 100)
        .clamp(6, area.height.saturating_sub(2))
        .max(5);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 3; // slightly above center
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " find files (Enter select · Esc cancel) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Prompt line with a real blinking terminal cursor.
    let prompt = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(finder.query.clone()),
    ]);
    let prompt_area = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(prompt), prompt_area);
    let cursor_col =
        (inner.x + 2 + finder.query.chars().count() as u16).min(inner.x + inner.width - 1);
    let _ = f.set_cursor_position((cursor_col, inner.y));

    // Results window (everything below the prompt), scrolled to keep the
    // cursor visible.
    let list_area = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if list_area.height == 0 {
        return;
    }
    let visible = list_area.height as usize;
    let offset = finder.cursor().saturating_sub(visible - 1);
    let rows: Vec<Line> = finder
        .results
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
        .map(|(n, r)| {
            let selected = offset + n == finder.cursor();
            finder::result_line(finder.item_display(r.index), &r.indices, selected)
        })
        .collect();
    f.render_widget(Paragraph::new(rows), list_area);
}

/// Apply the current selection (Enter / click): dirs expand/collapse in place
/// (C17), Angular components serve, unsupported kinds get their message, plain
/// files are inert (C08/C16). The server starts in a BACKGROUND task (the
/// result comes back through `start_tx`), so the event loop keeps drawing the
/// streaming logs while npm/ng spin up.
async fn activate(app: &mut TuiApp, start_tx: &StartSender) -> anyhow::Result<()> {
    let Some(entry) = app.nav.toggle()? else {
        return Ok(()); // folder toggled in place
    };
    activate_entry(app, start_tx, entry).await
}

/// Same selection flow for an arbitrary entry — used by both the tree (Enter)
/// and the fuzzy finder (Enter on a result).
async fn activate_entry(
    app: &mut TuiApp,
    start_tx: &StartSender,
    entry: Entry,
) -> anyhow::Result<()> {
    if !entry.kind.highlighted() {
        app.status = "not a renderable file".into();
        return Ok(());
    }
    if entry.kind.renderable() {
        match pipeline::validate(&entry.path, &app.root) {
            Ok(_info) => {
                if app.server.is_some() {
                    serve::write_selection_file(&app.selection_file, &entry.path, &app.root)?;
                    app.status = format!("switched to {} (hot-swap)", entry.name);
                } else if app.starting {
                    app.status = "already starting — watch the logs panel...".into();
                } else {
                    app.starting = true;
                    app.status = "starting Angular host — watch the logs panel...".into();
                    let opts = ServeOptions {
                        component: entry.path.clone(),
                        root: app.root.clone(),
                        port: None,
                        selection_file: app.selection_file.clone(),
                        open_browser: true,
                        sink: app.sink.clone(),
                    };
                    let tx = start_tx.clone();
                    app.start_task = Some(tokio::spawn(async move {
                        let result = serve::start(opts).await;
                        let _ = tx.send(result);
                    }));
                }
            }
            Err(e) => app.status = format!("invalid component: {e}"),
        }
    } else if let Some(message) = entry.kind.unsupported_message() {
        app.status = message.to_string();
    }
    Ok(())
}

/// Handle a keypress while the finder popup is open: edit the query, navigate
/// results, or activate/close. Global keys do NOT apply while it is open.
async fn finder_key(app: &mut TuiApp, key: KeyEvent, start_tx: &StartSender) -> anyhow::Result<()> {
    let close = |app: &mut TuiApp| {
        app.finder = None;
        app.status = DEFAULT_STATUS.into();
    };
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => close(app),
        KeyCode::Enter => {
            let selected = app.finder.as_ref().and_then(finder::Finder::selected_path);
            if let Some(path) = selected {
                let path = path.to_path_buf();
                close(app);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let kind = crate::detect::classify(&path);
                let entry = Entry {
                    name,
                    path,
                    is_dir: false,
                    kind,
                };
                activate_entry(app, start_tx, entry).await?;
            }
        }
        KeyCode::Down | KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(f) = app.finder.as_mut() {
                f.move_down();
            }
        }
        KeyCode::Down => {
            if let Some(f) = app.finder.as_mut() {
                f.move_down();
            }
        }
        KeyCode::Up | KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(f) = app.finder.as_mut() {
                f.move_up();
            }
        }
        KeyCode::Up => {
            if let Some(f) = app.finder.as_mut() {
                f.move_up();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(f) = app.finder.as_mut() {
                f.clear();
            }
        }
        KeyCode::Backspace => {
            if let Some(f) = app.finder.as_mut() {
                f.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(f) = app.finder.as_mut() {
                f.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let root_arg = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let root = cli::validate_root(&root_arg).context("invalid project root")?;
    let selection_file = std::env::temp_dir().join("render-component-selection.json");
    let sink = LogSink::new(false);

    let mut app = TuiApp::new(root, selection_file, sink)?;
    let (start_tx, start_rx) = tokio::sync::mpsc::unbounded_channel::<StartResult>();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut app, &mut terminal, start_tx, start_rx).await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // If a startup is still in flight, abort it (kill_on_drop stops the ng child).
    if let Some(task) = app.start_task.take() {
        task.abort();
        app.sink.push("[tui] startup aborted");
    }
    if let Some(handle) = app.server.take() {
        handle.shutdown().await;
    }
    result
}

async fn event_loop(
    app: &mut TuiApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    start_tx: StartSender,
    mut start_rx: tokio::sync::mpsc::UnboundedReceiver<StartResult>,
) -> anyhow::Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    terminal.draw(|f| ui(f, app))?;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // Periodic redraw so streamed logs appear without user input.
                terminal.draw(|f| ui(f, app))?;
            }
            Some(result) = start_rx.recv() => {
                app.starting = false;
                match result {
                    Ok(handle) => {
                        app.status = format!("serving at {} — select another component to hot-swap", handle.vite_url);
                        app.server = Some(handle);
                    }
                    Err(e) => {
                        app.status = format!("error starting server: {e}");
                        app.sink.push(format!("[tui] startup failed: {e}"));
                    }
                }
                terminal.draw(|f| ui(f, app))?;
            }
            maybe_event = events.next() => {
                let Some(ev) = maybe_event else { break; };
                match ev? {
                    CEvent::Key(key) if key.kind == KeyEventKind::Press => {
                        if app.finder.is_some() {
                            finder_key(app, key, &start_tx).await?;
                        } else {
                            match key.code {
                                KeyCode::Char('q') | KeyCode::Char('Q') => break,
                                KeyCode::Char('/') => {
                                    // Telescope-style fuzzy finder over the
                                    // whole project tree.
                                    app.finder = Some(finder::Finder::open(&app.root));
                                }
                                KeyCode::Char('j') | KeyCode::Down => { app.nav.move_down(); }
                                KeyCode::Char('k') | KeyCode::Up => { app.nav.move_up(); }
                                KeyCode::Enter => activate(app, &start_tx).await?,
                            KeyCode::Right => app.nav.expand()?,
                            KeyCode::Left => {
                                app.nav.collapse_or_parent();
                            }
                            KeyCode::PageUp => {
                                app.log_scroll = app.log_scroll.saturating_add(app.log_view_height.saturating_sub(1));
                            }
                            KeyCode::PageDown => {
                                app.log_scroll = app.log_scroll.saturating_sub(app.log_view_height.saturating_sub(1));
                            }
                            KeyCode::Home => {
                                app.log_scroll = usize::MAX; // clamped to the top at render time
                            }
                            KeyCode::End => {
                                app.log_scroll = 0; // live follow
                            }
                            KeyCode::Char('l') | KeyCode::Char('L') => {
                                let path = std::env::temp_dir().join("render-component-logs.log");
                                match app.sink.dump_to_file(&path) {
                                    Ok(n) => {
                                        app.sink.push(format!("[tui] dumped {n} log lines to {}", path.display()));
                                        app.status = format!("logs saved: {}", path.display());
                                    }
                                    Err(e) => app.status = format!("error saving logs: {e}"),
                                }
                            }
                            _ => {}
                            }
                        }
                    }
                    CEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                        // Clicks go to the tree only when the finder is closed.
                        if app.finder.is_none() {
                            click(app, mouse.column, mouse.row);
                            activate(app, &start_tx).await?;
                        }
                    }
                    CEvent::Resize(_, _) => {}
                    _ => {}
                }
                terminal.draw(|f| ui(f, app))?;
            }
        }
    }
    Ok(())
}

/// Map a click row to a tree row index (accounting for border + scroll offset).
fn click(app: &mut TuiApp, _column: u16, row: u16) {
    // List area starts one row below the header chunk; borders take one row.
    if row >= 2 {
        let index = (row - 2) as usize + app.list_state.offset();
        if index < app.nav.rows().len() {
            app.nav.set_cursor(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn component_files_are_visually_distinct_from_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("gallery.component.ts"),
            "@Component({}) export class A {}",
        )
        .unwrap();
        fs::write(dir.path().join("helper.ts"), "export const x = 1;").unwrap();
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            LogSink::new(false),
        )
        .unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let style_at = |needle: &str| -> Style {
            for y in 0..buffer.area.height {
                let mut line = String::new();
                for x in 0..buffer.area.width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                if let Some(pos) = line.find(needle) {
                    return buffer[(pos as u16, y)].style();
                }
            }
            panic!("line containing '{needle}' not found");
        };

        let component_style = style_at("gallery.component.ts");
        let plain_style = style_at("helper.ts");
        assert_eq!(
            component_style.fg,
            Some(Color::Cyan),
            "Angular components are cyan"
        );
        assert_ne!(
            component_style, plain_style,
            "component vs plain styles must differ (C03)"
        );
    }

    #[test]
    fn tree_renders_depth_indentation_and_expanded_indicator() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gallery")).unwrap();
        fs::write(
            dir.path().join("gallery/gallery.component.ts"),
            "@Component({}) export class A {}",
        )
        .unwrap();
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            LogSink::new(false),
        )
        .unwrap();
        app.nav.move_down();
        app.nav.toggle().unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let find_line = |needle: &str| -> Option<String> {
            for y in 0..buffer.area.height {
                let mut line = String::new();
                for x in 0..buffer.area.width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                if line.contains(needle) {
                    return Some(line);
                }
            }
            None
        };
        let gallery_line = find_line("gallery/").expect("gallery row rendered");
        assert!(
            gallery_line.contains("▾"),
            "expanded dir shows ▾ indicator: {gallery_line}"
        );
        let child_line = find_line("gallery.component.ts").expect("child row rendered");
        // char-based positions: box-drawing chars are multi-byte in UTF-8.
        // Compare the ICON positions: the dir row carries the ▾/▸ arrow, so
        // the icon column is what reflects the actual tree depth.
        let char_pos = |line: &str, needle: char| {
            line.find(needle)
                .map(|b| line[..b].chars().count())
                .unwrap_or(usize::MAX)
        };
        let folder_open = '\u{f07c}';
        let ts_icon = devicons::icon_for_file("gallery.component.ts", &Some(Theme::Dark)).icon;
        let gallery_icon_pos = char_pos(&gallery_line, folder_open);
        let child_icon_pos = char_pos(&child_line, ts_icon);
        assert!(
            child_icon_pos > gallery_icon_pos,
            "child icon is indented deeper than parent icon (C17): dir={gallery_icon_pos} child={child_icon_pos}"
        );
    }

    #[test]
    fn devicons_render_next_to_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gallery")).unwrap();
        fs::write(
            dir.path().join("gallery/gallery.component.ts"),
            "@Component({}) export class A {}",
        )
        .unwrap();
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            LogSink::new(false),
        )
        .unwrap();
        app.nav.move_down();
        app.nav.toggle().unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let find_line = |needle: &str| -> Option<String> {
            for y in 0..buffer.area.height {
                let mut line = String::new();
                for x in 0..buffer.area.width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                if line.contains(needle) {
                    return Some(line);
                }
            }
            None
        };

        let folder_open = "\u{f07c}";
        let ts_icon = devicons::icon_for_file("gallery.component.ts", &Some(Theme::Dark)).icon;
        let gallery_line = find_line("gallery/").expect("dir row");
        assert!(
            gallery_line.contains(folder_open),
            "expanded dir shows folder-open glyph: {gallery_line}"
        );
        let child_line = find_line("gallery.component.ts").expect("file row");
        assert!(
            child_line.contains(ts_icon),
            "component file shows its devicon glyph: {child_line}"
        );

        // The devicons color parses into an Rgb color for the icon span.
        let expected =
            parse_hex(&devicons::icon_for_file("gallery.component.ts", &Some(Theme::Dark)).color);
        assert!(
            expected.is_some(),
            "devicons color hex parses: {expected:?}"
        );
    }

    #[test]
    fn hex_parsing_handles_material_colors() {
        assert_eq!(parse_hex("#3178C6"), Some(Color::Rgb(0x31, 0x78, 0xC6)));
        assert_eq!(parse_hex("FF5733"), Some(Color::Rgb(0xFF, 0x57, 0x33)));
        assert_eq!(parse_hex("#XYZ"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    #[test]
    fn logs_panel_streams_sink_tail() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("gallery.component.ts"),
            "@Component({}) export class A {}",
        )
        .unwrap();
        let sink = LogSink::new(false);
        sink.push("[npm] added 847 packages in 41s");
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            sink,
        )
        .unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut found = None;
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            if line.contains("added 847 packages") {
                found = Some(line);
                break;
            }
        }
        assert!(
            found.is_some(),
            "log line must render in the logs panel (C18)"
        );
    }

    #[test]
    fn logs_panel_scrolls_through_full_history() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("gallery.component.ts"),
            "@Component({}) export class A {}",
        )
        .unwrap();
        let sink = LogSink::new(false);
        for i in 0..40 {
            sink.push(format!("history line {i}"));
        }
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            sink,
        )
        .unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // At the bottom (live follow): newest visible, oldest not.
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let buffer_text = |terminal: &Terminal<ratatui::backend::TestBackend>| {
            let buffer = terminal.backend().buffer();
            let mut text = String::new();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    text.push_str(buffer[(x, y)].symbol());
                }
                text.push('\n');
            }
            text
        };
        let text = buffer_text(&terminal);
        assert!(
            text.contains("history line 39"),
            "newest line visible at follow"
        );
        assert!(
            !text.contains("history line 0"),
            "history beyond the panel is hidden at follow"
        );

        // Scroll to the very top: the first line becomes visible.
        app.log_scroll = usize::MAX;
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("history line 0"),
            "oldest line visible when scrolled to top"
        );
        assert!(
            !text.contains("history line 39"),
            "newest line hidden when scrolled to top"
        );
    }

    #[test]
    fn error_log_lines_are_red() {
        let style = log_line_style("[ng!] ERROR in src/main.ts: oops");
        assert_eq!(style.fg, Some(Color::Red), "errors are red");
        let ok = log_line_style("[npm] added 847 packages");
        assert_ne!(ok.fg, Some(Color::Red));
    }

    #[test]
    fn finder_popup_renders_prompt_and_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gallery")).unwrap();
        fs::write(dir.path().join("gallery/gallery.component.ts"), "a").unwrap();
        fs::write(dir.path().join("helper.ts"), "b").unwrap();
        let mut app = TuiApp::new(
            dir.path().to_path_buf(),
            PathBuf::from("/tmp/none.json"),
            LogSink::new(false),
        )
        .unwrap();
        let mut f = finder::Finder::open(dir.path());
        f.push('g');
        f.push('a');
        f.push('l');
        app.finder = Some(f);

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, &mut app)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("> gal"), "prompt renders with the query: {text}");
        assert!(
            text.contains("gallery.component.ts"),
            "fuzzy results render inside the popup: {text}"
        );
        assert!(
            text.contains("find files"),
            "popup title renders: {text}"
        );
    }

    #[test]
    fn unsupported_kinds_have_distinct_styles_and_messages() {
        assert_ne!(
            style_for_kind(ComponentKind::React),
            style_for_kind(ComponentKind::Vue)
        );
        assert_eq!(
            ComponentKind::React.unsupported_message(),
            Some("React (.tsx) components are not supported yet")
        );
        assert!(ComponentKind::AngularModule.unsupported_message().is_none());
    }
}
