use std::cmp::Ordering;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use walkdir::{DirEntry, WalkDir};

const BG: Color = Color::Rgb(22, 22, 30);
const SURFACE: Color = Color::Rgb(30, 30, 40);
const TEXT_PRIMARY: Color = Color::Rgb(234, 234, 242);
const TEXT_SECONDARY: Color = Color::Rgb(177, 177, 192);
const TEXT_MUTED: Color = Color::Rgb(122, 122, 137);
const BORDER: Color = Color::Rgb(50, 50, 65);
const ACCENT_PRIMARY: Color = Color::Rgb(110, 168, 254);
const ACCENT_SUCCESS: Color = Color::Rgb(116, 198, 157);
const ACCENT_WARNING: Color = Color::Rgb(255, 183, 77);
const ACCENT_ERROR: Color = Color::Rgb(255, 107, 107);

fn main() -> io::Result<()> {
    let root = parse_root_argument()?;
    let mut terminal = TerminalSession::new()?;
    run_app(terminal.terminal_mut(), root)
}

fn parse_root_argument() -> io::Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let root = args.next().unwrap_or_else(|| ".".into());
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: purgee [PATH]",
        ));
    }

    fs::canonicalize(root)
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
                let _ = terminal::disable_raw_mode();
                return Err(error);
            }
        };

        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            cursor::Show
        );
        let _ = self.terminal.show_cursor();
    }
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, root: PathBuf) -> io::Result<()> {
    let (worker_tx, worker_rx) = mpsc::channel();
    let mut app = App::new(root, worker_tx, worker_rx);
    app.start_scan();

    loop {
        app.drain_worker_messages();
        terminal.draw(|frame| render(frame, &mut app))?;

        if app.should_quit {
            return Ok(());
        }

        let timeout = if app.needs_animation() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        };
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }

        if app.needs_animation() && app.last_tick.elapsed() >= Duration::from_millis(100) {
            app.animation_tick = (app.animation_tick + 1) % SPINNER.len();
            app.last_tick = Instant::now();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetEntry {
    project_root: PathBuf,
    target_path: PathBuf,
    display_name: String,
    relative_path: String,
    size_bytes: u64,
    original_size_bytes: u64,
    modified_at: SystemTime,
    delete_status: DeleteStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortField {
    Size,
    Name,
    Modified,
    Path,
}

impl SortField {
    const ALL: [Self; 4] = [Self::Size, Self::Name, Self::Modified, Self::Path];

    fn label(self) -> &'static str {
        match self {
            Self::Size => "Size",
            Self::Name => "Name",
            Self::Modified => "Modified",
            Self::Path => "Path",
        }
    }

    fn default_order(self) -> SortOrder {
        match self {
            Self::Size | Self::Modified => SortOrder::Desc,
            Self::Name | Self::Path => SortOrder::Asc,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    fn symbol(self) -> &'static str {
        match self {
            Self::Asc => "↑",
            Self::Desc => "↓",
        }
    }

    fn toggle(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppMode {
    Scanning,
    Browsing,
    Searching,
    SortMenu { index: usize },
    Help,
}

#[derive(Clone, Debug, Default)]
struct ScanSummary {
    discovered: usize,
    total_bytes: u64,
    current_path: Option<PathBuf>,
    elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteStatus {
    Ready,
    Deleting,
    Deleted,
    Failed(String),
}

#[derive(Clone, Debug)]
enum WorkerMessage {
    ScanProgress {
        discovered: usize,
        total_bytes: u64,
        current_path: PathBuf,
    },
    ScanComplete {
        entries: Vec<TargetEntry>,
        warnings: Vec<String>,
        elapsed: Duration,
    },
    DeleteProgress {
        path: PathBuf,
        status: DeleteStatus,
    },
}

struct App {
    root: PathBuf,
    entries: Vec<TargetEntry>,
    filtered_indices: Vec<usize>,
    cursor: usize,
    selected: HashSet<PathBuf>,
    search_query: String,
    sort_field: SortField,
    sort_order: SortOrder,
    mode: AppMode,
    scan_summary: ScanSummary,
    scan_warnings: Vec<String>,
    table_state: TableState,
    worker_tx: Sender<WorkerMessage>,
    worker_rx: Receiver<WorkerMessage>,
    should_quit: bool,
    animation_tick: usize,
    last_tick: Instant,
}

impl App {
    fn new(
        root: PathBuf,
        worker_tx: Sender<WorkerMessage>,
        worker_rx: Receiver<WorkerMessage>,
    ) -> Self {
        Self {
            root,
            entries: Vec::new(),
            filtered_indices: Vec::new(),
            cursor: 0,
            selected: HashSet::new(),
            search_query: String::new(),
            sort_field: SortField::Size,
            sort_order: SortOrder::Desc,
            mode: AppMode::Scanning,
            scan_summary: ScanSummary::default(),
            scan_warnings: Vec::new(),
            table_state: TableState::default(),
            worker_tx,
            worker_rx,
            should_quit: false,
            animation_tick: 0,
            last_tick: Instant::now(),
        }
    }

    fn start_scan(&mut self) {
        self.mode = AppMode::Scanning;
        self.entries.clear();
        self.filtered_indices.clear();
        self.cursor = 0;
        self.scan_summary = ScanSummary::default();
        self.scan_warnings.clear();

        let root = self.root.clone();
        let tx = self.worker_tx.clone();
        thread::spawn(move || scan_targets(root, tx));
    }

    fn drain_worker_messages(&mut self) {
        while let Ok(message) = self.worker_rx.try_recv() {
            match message {
                WorkerMessage::ScanProgress {
                    discovered,
                    total_bytes,
                    current_path,
                } => {
                    self.scan_summary.discovered = discovered;
                    self.scan_summary.total_bytes = total_bytes;
                    self.scan_summary.current_path = Some(current_path);
                }
                WorkerMessage::ScanComplete {
                    entries,
                    warnings,
                    elapsed,
                } => {
                    self.entries = entries;
                    self.scan_summary.discovered = self.entries.len();
                    self.scan_summary.total_bytes =
                        self.entries.iter().map(|entry| entry.size_bytes).sum();
                    self.scan_summary.current_path = None;
                    self.scan_summary.elapsed = elapsed;
                    self.scan_warnings = warnings;
                    let known_paths: HashSet<_> = self
                        .entries
                        .iter()
                        .map(|entry| entry.target_path.clone())
                        .collect();
                    self.selected.retain(|path| known_paths.contains(path));
                    self.recompute_view();
                    self.mode = AppMode::Browsing;
                }
                WorkerMessage::DeleteProgress { path, status } => {
                    if let Some(entry) = self
                        .entries
                        .iter_mut()
                        .find(|entry| entry.target_path == path)
                    {
                        if matches!(status, DeleteStatus::Deleted) {
                            entry.size_bytes = 0;
                            self.selected.remove(&path);
                        }
                        entry.delete_status = status;
                        self.scan_summary.total_bytes = self.cached_total_bytes();
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }

        match self.mode.clone() {
            AppMode::Scanning => {
                if matches!(key.code, KeyCode::Char('q')) {
                    self.should_quit = true;
                }
            }
            AppMode::Browsing => self.handle_browsing_key(key),
            AppMode::Searching => self.handle_search_key(key),
            AppMode::SortMenu { index } => self.handle_sort_menu_key(key, index),
            AppMode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.mode = AppMode::Browsing;
                }
            }
        }
    }

    fn handle_browsing_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Char('g') => {
                self.cursor = 0;
                self.sync_table_state();
            }
            KeyCode::Char('G') => {
                self.cursor = self.filtered_indices.len().saturating_sub(1);
                self.sync_table_state();
            }
            KeyCode::Char(' ') => self.toggle_current_selection(),
            KeyCode::Char('a') => self.toggle_all_filtered(),
            KeyCode::Char('i') => self.invert_filtered_selection(),
            KeyCode::Char('/') => self.mode = AppMode::Searching,
            KeyCode::Char('s') => {
                let index = SortField::ALL
                    .iter()
                    .position(|field| *field == self.sort_field)
                    .unwrap_or(0);
                self.mode = AppMode::SortMenu { index };
            }
            KeyCode::Char('d') => self.start_delete_current(),
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('?') => self.mode = AppMode::Help,
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.mode = AppMode::Browsing,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.recompute_view();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.search_query.push(character);
                self.recompute_view();
            }
            _ => {}
        }
    }

    fn handle_sort_menu_key(&mut self, key: KeyEvent, mut index: usize) {
        match key.code {
            KeyCode::Esc => self.mode = AppMode::Browsing,
            KeyCode::Down | KeyCode::Char('j') => {
                index = (index + 1) % SortField::ALL.len();
                self.mode = AppMode::SortMenu { index };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                index = if index == 0 {
                    SortField::ALL.len() - 1
                } else {
                    index - 1
                };
                self.mode = AppMode::SortMenu { index };
            }
            KeyCode::Char('s') => {
                self.sort_order = self.sort_order.toggle();
                self.recompute_view();
            }
            KeyCode::Enter => {
                self.apply_sort_field(SortField::ALL[index]);
                self.mode = AppMode::Browsing;
            }
            _ => {}
        }
    }

    fn start_delete_current(&mut self) {
        let Some(entry) = self.current_entry().cloned() else {
            return;
        };
        if matches!(
            entry.delete_status,
            DeleteStatus::Deleting | DeleteStatus::Deleted
        ) {
            return;
        }
        if let Some(cached_entry) = self
            .entries
            .iter_mut()
            .find(|cached_entry| cached_entry.target_path == entry.target_path)
        {
            cached_entry.delete_status = DeleteStatus::Deleting;
        }

        let root = self.root.clone();
        let tx = self.worker_tx.clone();
        thread::spawn(move || delete_target(root, entry, tx));
    }

    fn apply_sort_field(&mut self, field: SortField) {
        if self.sort_field == field {
            self.sort_order = self.sort_order.toggle();
        } else {
            self.sort_field = field;
            self.sort_order = field.default_order();
        }
        self.recompute_view();
    }

    fn recompute_view(&mut self) {
        self.entries
            .sort_by(|left, right| compare_entries(left, right, self.sort_field, self.sort_order));
        self.filtered_indices = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry_matches_query(entry, &self.search_query).then_some(index)
            })
            .collect();
        self.cursor = self
            .cursor
            .min(self.filtered_indices.len().saturating_sub(1));
        self.sync_table_state();
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered_indices.is_empty() {
            self.cursor = 0;
            self.sync_table_state();
            return;
        }

        if delta.is_negative() {
            self.cursor = self.cursor.saturating_sub(delta.unsigned_abs());
        } else {
            self.cursor = (self.cursor + delta as usize).min(self.filtered_indices.len() - 1);
        }
        self.sync_table_state();
    }

    fn toggle_current_selection(&mut self) {
        let Some(entry) = self.current_entry() else {
            return;
        };
        if !entry.is_selectable() {
            return;
        }
        let path = entry.target_path.clone();
        if !self.selected.insert(path.clone()) {
            self.selected.remove(&path);
        }
    }

    fn toggle_all_filtered(&mut self) {
        let visible_paths: Vec<_> = self
            .filtered_indices
            .iter()
            .filter_map(|index| {
                let entry = &self.entries[*index];
                entry.is_selectable().then_some(entry.target_path.clone())
            })
            .collect();
        let all_selected = visible_paths
            .iter()
            .all(|path| self.selected.contains(path));
        for path in visible_paths {
            if all_selected {
                self.selected.remove(&path);
            } else {
                self.selected.insert(path);
            }
        }
    }

    fn invert_filtered_selection(&mut self) {
        for path in self.filtered_indices.iter().filter_map(|index| {
            let entry = &self.entries[*index];
            entry.is_selectable().then_some(entry.target_path.clone())
        }) {
            if !self.selected.insert(path.clone()) {
                self.selected.remove(&path);
            }
        }
    }

    fn current_entry(&self) -> Option<&TargetEntry> {
        self.filtered_indices
            .get(self.cursor)
            .and_then(|index| self.entries.get(*index))
    }

    fn selected_entries(&self) -> Vec<&TargetEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.is_selectable() && self.selected.contains(&entry.target_path))
            .collect()
    }

    fn selected_total_bytes(&self) -> u64 {
        self.selected_entries()
            .into_iter()
            .map(TargetEntry::effective_size_bytes)
            .sum()
    }

    fn cached_total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .map(TargetEntry::effective_size_bytes)
            .sum()
    }

    fn remaining_target_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !matches!(entry.delete_status, DeleteStatus::Deleted))
            .count()
    }

    fn cleaned_session_summary(&self) -> Option<(usize, u64)> {
        let deleted: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.delete_status, DeleteStatus::Deleted))
            .collect();
        (!deleted.is_empty()).then(|| {
            (
                deleted.len(),
                deleted.iter().map(|entry| entry.original_size_bytes).sum(),
            )
        })
    }

    fn needs_animation(&self) -> bool {
        matches!(self.mode, AppMode::Scanning)
            || self
                .entries
                .iter()
                .any(|entry| matches!(entry.delete_status, DeleteStatus::Deleting))
    }

    fn sync_table_state(&mut self) {
        self.table_state
            .select((!self.filtered_indices.is_empty()).then_some(self.cursor));
    }
}

fn compare_entries(
    left: &TargetEntry,
    right: &TargetEntry,
    field: SortField,
    order: SortOrder,
) -> Ordering {
    let ordering = match field {
        SortField::Size => left
            .effective_size_bytes()
            .cmp(&right.effective_size_bytes())
            .then_with(|| left.display_name.cmp(&right.display_name)),
        SortField::Name => left
            .display_name
            .cmp(&right.display_name)
            .then_with(|| left.relative_path.cmp(&right.relative_path)),
        SortField::Modified => left
            .modified_at
            .cmp(&right.modified_at)
            .then_with(|| left.display_name.cmp(&right.display_name)),
        SortField::Path => left
            .relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.display_name.cmp(&right.display_name)),
    };

    match order {
        SortOrder::Asc => ordering,
        SortOrder::Desc => ordering.reverse(),
    }
}

fn entry_matches_query(entry: &TargetEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let query = query.to_ascii_lowercase();
    entry.display_name.to_ascii_lowercase().contains(&query)
        || entry.relative_path.to_ascii_lowercase().contains(&query)
}

impl TargetEntry {
    fn effective_size_bytes(&self) -> u64 {
        if matches!(self.delete_status, DeleteStatus::Deleted) {
            0
        } else {
            self.size_bytes
        }
    }

    fn is_selectable(&self) -> bool {
        !matches!(
            self.delete_status,
            DeleteStatus::Deleting | DeleteStatus::Deleted
        )
    }
}

fn scan_targets(root: PathBuf, tx: Sender<WorkerMessage>) {
    let started = Instant::now();
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_targets = HashSet::new();
    let mut total_bytes = 0;

    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(entry, &root))
    {
        match entry {
            Ok(entry) if entry.file_type().is_file() && entry.file_name() == "Cargo.toml" => {
                let Some(project_root) = entry.path().parent() else {
                    continue;
                };
                let target_path = project_root.join("target");
                if !is_real_directory(&target_path) {
                    continue;
                }
                let canonical_target = match fs::canonicalize(&target_path) {
                    Ok(path) => path,
                    Err(error) => {
                        warnings.push(format!("{}: {error}", target_path.display()));
                        continue;
                    }
                };
                if !seen_targets.insert(canonical_target.clone()) {
                    continue;
                }

                match measure_target(project_root, &canonical_target, &root) {
                    Ok(target_entry) => {
                        total_bytes += target_entry.size_bytes;
                        entries.push(target_entry);
                        let _ = tx.send(WorkerMessage::ScanProgress {
                            discovered: entries.len(),
                            total_bytes,
                            current_path: canonical_target,
                        });
                    }
                    Err(error) => warnings.push(format!("{}: {error}", target_path.display())),
                }
            }
            Ok(_) => {}
            Err(error) => warnings.push(error.to_string()),
        }
    }

    let _ = tx.send(WorkerMessage::ScanComplete {
        entries,
        warnings,
        elapsed: started.elapsed(),
    });
}

fn should_descend(entry: &DirEntry, root: &Path) -> bool {
    if entry.path() == root {
        return true;
    }
    if !entry.file_type().is_dir() {
        return true;
    }

    !matches!(entry.file_name().to_str(), Some(".git" | "target"))
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

fn measure_target(
    project_root: &Path,
    target_path: &Path,
    scan_root: &Path,
) -> io::Result<TargetEntry> {
    let mut size_bytes = 0;
    let mut modified_at = fs::metadata(target_path)?.modified().unwrap_or(UNIX_EPOCH);

    for entry in WalkDir::new(target_path).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() {
            size_bytes += metadata.len();
        }
        if let Ok(modified) = metadata.modified() {
            modified_at = modified_at.max(modified);
        }
    }

    Ok(TargetEntry {
        project_root: project_root.to_path_buf(),
        target_path: target_path.to_path_buf(),
        display_name: project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".")
            .to_string(),
        relative_path: display_relative_path(scan_root, target_path),
        size_bytes,
        original_size_bytes: size_bytes,
        modified_at,
        delete_status: DeleteStatus::Ready,
    })
}

fn delete_target(root: PathBuf, entry: TargetEntry, tx: Sender<WorkerMessage>) {
    let path = entry.target_path.clone();
    let status = match validate_delete_path(&root, &path).and_then(|()| fs::remove_dir_all(&path)) {
        Ok(()) => DeleteStatus::Deleted,
        Err(error) => DeleteStatus::Failed(error.to_string()),
    };
    let _ = tx.send(WorkerMessage::DeleteProgress { path, status });
}

fn validate_delete_path(root: &Path, target_path: &Path) -> io::Result<()> {
    if target_path.file_name().and_then(|name| name.to_str()) != Some("target") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete a non-target directory",
        ));
    }

    let canonical_root = fs::canonicalize(root)?;
    let canonical_target = fs::canonicalize(target_path)?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete a target outside the scan root",
        ));
    }

    Ok(())
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![
            Span::styled(
                " purgee ",
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.root.display().to_string(),
                Style::default().fg(TEXT_SECONDARY),
            ),
            Span::styled(
                format!(
                    "  scanned {} in {} ",
                    app.scan_summary.discovered,
                    format_duration(app.scan_summary.elapsed)
                ),
                Style::default().fg(TEXT_MUTED),
            ),
        ]))
        .style(Style::default().fg(BORDER).bg(BG));
    let area = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_stats(frame, layout[1], app);
    render_search(frame, layout[2], app);
    render_table_or_state(frame, layout[3], app);
    render_footer(frame, layout[4], app);

    match app.mode {
        AppMode::SortMenu { index } => render_sort_menu(frame, app, index),
        AppMode::Help => render_help(frame),
        _ => {}
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let status = match &app.mode {
        AppMode::Scanning => format!(
            "{}  Scanning {}",
            spinner(app.animation_tick),
            app.scan_summary
                .current_path
                .as_ref()
                .map(|path| display_relative_path(&app.root, path))
                .unwrap_or_else(|| app.root.display().to_string())
        ),
        _ => {
            let deleting_count = app
                .entries
                .iter()
                .filter(|entry| matches!(entry.delete_status, DeleteStatus::Deleting))
                .count();
            if deleting_count > 0 {
                format!(
                    "{}  Deleting {} target folder{}",
                    spinner(app.animation_tick),
                    deleting_count,
                    if deleting_count == 1 { "" } else { "s" }
                )
            } else if app.scan_warnings.is_empty() {
                format!(
                    "{} scanned · {} remaining",
                    app.entries.len(),
                    app.remaining_target_count()
                )
            } else {
                format!(
                    "{} scanned · {} remaining · {} scan warnings",
                    app.entries.len(),
                    app.remaining_target_count(),
                    app.scan_warnings.len()
                )
            }
        }
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(TEXT_SECONDARY).bg(BG)),
        area,
    );
}

fn render_stats(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(28),
            Constraint::Percentage(44),
        ])
        .split(area);

    render_stat_card(
        frame,
        chunks[0],
        "Reclaimable",
        format_bytes(app.scan_summary.total_bytes),
        format!("across {} remaining", app.remaining_target_count()),
        TEXT_PRIMARY,
    );
    render_stat_card(
        frame,
        chunks[1],
        "Selected",
        format_bytes(app.selected_total_bytes()),
        format!("in {} projects", app.selected_entries().len()),
        ACCENT_WARNING,
    );

    let largest = app
        .entries
        .iter()
        .filter(|entry| !matches!(entry.delete_status, DeleteStatus::Deleted))
        .max_by_key(|entry| entry.effective_size_bytes())
        .map(|entry| {
            format!(
                "{}   {}",
                entry.display_name,
                format_bytes(entry.effective_size_bytes())
            )
        })
        .unwrap_or_else(|| "No target folders".to_string());
    render_stat_card(
        frame,
        chunks[2],
        "Largest",
        largest,
        "largest remaining target".to_string(),
        TEXT_PRIMARY,
    );
}

fn render_stat_card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    value: String,
    detail: String,
    value_color: Color,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(BORDER).bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content = vec![
        Line::from(Span::styled(
            value,
            Style::default()
                .fg(value_color)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::default().fg(TEXT_MUTED))),
    ];
    frame.render_widget(
        Paragraph::new(content)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_search(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let border_color = if matches!(app.mode, AppMode::Searching) {
        ACCENT_PRIMARY
    } else {
        BORDER
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(border_color).bg(BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cursor = matches!(app.mode, AppMode::Searching)
        .then_some("█")
        .unwrap_or("");
    let left = if app.search_query.is_empty() && !matches!(app.mode, AppMode::Searching) {
        "Search ›  type to filter…".to_string()
    } else {
        format!("Search › {}{}", app.search_query, cursor)
    };
    let right = format!(
        "Sort: {} {}",
        app.sort_field.label(),
        app.sort_order.symbol()
    );
    let width = inner.width as usize;
    let spaces = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = format!("{left}{}{right}", " ".repeat(spaces.max(1)));
    let search_style = if app.search_query.is_empty() && !matches!(app.mode, AppMode::Searching) {
        Style::default().fg(TEXT_MUTED).bg(BG)
    } else {
        Style::default().fg(TEXT_PRIMARY).bg(BG)
    };
    frame.render_widget(Paragraph::new(line).style(search_style), inner);
}

fn render_table_or_state(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if matches!(app.mode, AppMode::Scanning) && app.entries.is_empty() {
        render_scan_state(frame, area, app);
        return;
    }

    if app.entries.is_empty() {
        render_empty_state(frame, area);
        return;
    }

    let rows: Vec<_> = app
        .filtered_indices
        .iter()
        .enumerate()
        .map(|(visible_index, entry_index)| {
            let entry = &app.entries[*entry_index];
            let is_selected = app.selected.contains(&entry.target_path);
            let is_active = visible_index == app.cursor;
            let row_style = match entry.delete_status {
                DeleteStatus::Deleted => Style::default().bg(BG).fg(TEXT_MUTED),
                DeleteStatus::Failed(_) => Style::default().bg(BG).fg(ACCENT_ERROR),
                _ if is_active => Style::default().bg(Color::Rgb(39, 54, 79)).fg(TEXT_PRIMARY),
                _ => Style::default().bg(BG).fg(TEXT_PRIMARY),
            };
            let size_style = if entry.effective_size_bytes() >= 1024 * 1024 * 1024 {
                row_style.fg(ACCENT_WARNING)
            } else {
                row_style
            };
            let status_suffix = match &entry.delete_status {
                DeleteStatus::Ready => String::new(),
                DeleteStatus::Deleting => " deleting…".to_string(),
                DeleteStatus::Deleted => " deleted".to_string(),
                DeleteStatus::Failed(error) => format!(" failed: {error}"),
            };
            Row::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(
                        if is_active { "▸" } else { " " },
                        Style::default().fg(ACCENT_PRIMARY),
                    ),
                    Span::raw(" "),
                    status_span(entry, is_selected, app.animation_tick),
                ])),
                Cell::from(highlight_text(
                    &format!("{}{}", entry.display_name, status_suffix),
                    &app.search_query,
                )),
                Cell::from(format_size_cell(entry.effective_size_bytes())).style(size_style),
                Cell::from(relative_time(entry.modified_at)),
                Cell::from(highlight_text(
                    &compact_relative_path(&entry.relative_path),
                    &app.search_query,
                )),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Percentage(28),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Min(18),
        ],
    )
    .header(
        Row::new(vec!["", "PROJECT", "SIZE", "MODIFIED", "TARGET"])
            .style(Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(BORDER).bg(BG)),
    )
    .column_spacing(1);
    app.sync_table_state();
    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_scan_state(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = vec![
        Line::from(Span::styled(
            format!(
                "{}  Scanning {}",
                spinner(app.animation_tick),
                app.root.display()
            ),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "{} projects found · {} so far",
                app.scan_summary.discovered,
                format_bytes(app.scan_summary.total_bytes)
            ),
            Style::default().fg(TEXT_SECONDARY),
        )),
        Line::from(Span::styled(
            app.scan_summary
                .current_path
                .as_ref()
                .map(|path| format!("looking at {}", display_relative_path(&app.root, path)))
                .unwrap_or_else(|| "walking the filesystem".to_string()),
            Style::default().fg(TEXT_MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn render_empty_state(frame: &mut Frame<'_>, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("✓", Style::default().fg(ACCENT_SUCCESS))),
        Line::from(""),
        Line::from(Span::styled(
            "No reclaimable target/ found",
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "The current directory is already clean.",
            Style::default().fg(TEXT_SECONDARY),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = match app.mode {
        AppMode::Searching => {
            footer_spans(&[("esc", "cancel"), ("enter", "apply"), ("type", "filter")])
        }
        AppMode::SortMenu { .. } => footer_spans(&[
            ("↑↓", "pick"),
            ("enter", "apply"),
            ("s", "toggle"),
            ("esc", "close"),
        ]),
        _ => footer_spans(&[
            ("↑↓", "navigate"),
            ("space", "select"),
            ("a", "all"),
            ("i", "invert"),
            ("/", "search"),
            ("s", "sort"),
            ("d", "delete focused"),
            ("r", "rescan"),
            ("?", "help"),
            ("q", "quit"),
        ]),
    };
    if let Some((count, bytes)) = app.cleaned_session_summary() {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            format!("{count} cleaned · {} freed", format_bytes(bytes)),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
}

fn render_sort_menu(frame: &mut Frame<'_>, app: &App, index: usize) {
    let area = centered_rect(34, 34, frame.area());
    frame.render_widget(Clear, area);
    let rows: Vec<_> = SortField::ALL
        .iter()
        .enumerate()
        .map(|(row_index, field)| {
            let marker = if row_index == index { "▸" } else { " " };
            let suffix = if *field == app.sort_field {
                app.sort_order.symbol()
            } else {
                " "
            };
            let style = if row_index == index {
                Style::default()
                    .fg(ACCENT_PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_PRIMARY)
            };
            Line::from(Span::styled(
                format!("{marker} {:<10} {suffix}", field.label()),
                style,
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Sort by "))
            .style(Style::default().bg(SURFACE)),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>) {
    let area = centered_rect(74, 72, frame.area());
    frame.render_widget(Clear, area);
    let lines = [
        "↑ ↓ / j k   Move cursor",
        "g / G       Jump to top / bottom",
        "space       Toggle selection",
        "a           Toggle all filtered rows",
        "i           Invert filtered selection",
        "/           Search",
        "s           Sort",
        "d           Delete focused row immediately",
        "r           Rescan",
        "?           Toggle help",
        "q / ctrl-c  Quit",
    ]
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .style(Style::default().fg(TEXT_PRIMARY).bg(SURFACE)),
        area,
    );
}

fn footer_spans(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (key, action)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(ACCENT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*action).to_string(),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    spans
}

fn status_span(entry: &TargetEntry, is_selected: bool, animation_tick: usize) -> Span<'static> {
    match entry.delete_status {
        DeleteStatus::Ready if is_selected => Span::styled(
            "✓".to_string(),
            Style::default()
                .fg(ACCENT_SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        DeleteStatus::Ready => Span::raw(" "),
        DeleteStatus::Deleting => Span::styled(
            spinner(animation_tick).to_string(),
            Style::default()
                .fg(ACCENT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        DeleteStatus::Deleted => Span::styled(
            "✓".to_string(),
            Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
        ),
        DeleteStatus::Failed(_) => Span::styled(
            "!".to_string(),
            Style::default()
                .fg(ACCENT_ERROR)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn highlight_text(text: &str, query: &str) -> Line<'static> {
    if query.is_empty() {
        return Line::from(text.to_string());
    }

    let lower_text = text.to_ascii_lowercase();
    let lower_query = query.to_ascii_lowercase();
    let Some(start) = lower_text.find(&lower_query) else {
        return Line::from(text.to_string());
    };
    let end = start + lower_query.len();
    Line::from(vec![
        Span::raw(text[..start].to_string()),
        Span::styled(
            text[start..end].to_string(),
            Style::default()
                .fg(ACCENT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(text[end..].to_string()),
    ])
}

fn display_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|relative| format!("./{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn compact_relative_path(path: &str) -> String {
    const MAX_CHARS: usize = 28;
    if path.chars().count() <= MAX_CHARS {
        return path.to_string();
    }

    let trimmed = path.trim_start_matches("./").trim_end_matches("/target");
    let tail = trimmed
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    format!("…/{tail}/target")
}

fn format_size_cell(bytes: u64) -> String {
    let formatted = format_bytes(bytes);
    let mut parts = formatted.split_whitespace();
    let value = parts.next().unwrap_or("0");
    let unit = parts.next().unwrap_or("B");
    format!("{value:>6} {unit:<2}")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn relative_time(time: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(time)
        .unwrap_or(Duration::ZERO);
    let days = elapsed.as_secs() / 86_400;
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "1 week ago".to_string(),
        14..=29 => format!("{} weeks ago", days / 7),
        30..=59 => "1 month ago".to_string(),
        _ => format!("{} months ago", days / 30),
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.is_zero() {
        "0.0s".to_string()
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn spinner(index: usize) -> &'static str {
    SPINNER[index % SPINNER.len()]
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    #[test]
    fn scan_finds_recursive_cargo_backed_targets_only() {
        let temp = TestDir::new();
        create_project(&temp.path().join("alpha"), 1024);
        create_project(&temp.path().join("nested/beta"), 2048);
        fs::create_dir_all(temp.path().join("plain/target")).unwrap();
        fs::write(temp.path().join("plain/target/orphan.bin"), vec![0_u8; 512]).unwrap();

        let entries = scan_sync(temp.path());

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.display_name == "alpha"));
        assert!(entries.iter().any(|entry| entry.display_name == "beta"));
        assert!(
            entries
                .iter()
                .all(|entry| entry.relative_path.contains("target"))
        );
    }

    #[test]
    fn measure_target_sums_files_and_uses_latest_mtime() {
        let temp = TestDir::new();
        let project = temp.path().join("alpha");
        create_project(&project, 1024);
        fs::write(project.join("target/second.bin"), vec![0_u8; 2048]).unwrap();

        let entry = measure_target(&project, &project.join("target"), temp.path()).unwrap();

        assert_eq!(entry.size_bytes, 3072);
        assert_eq!(entry.display_name, "alpha");
        assert_eq!(entry.relative_path, "./alpha/target");
    }

    #[test]
    fn filtering_and_selection_stay_scoped_to_visible_rows() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.entries = vec![
            fake_entry("alpha", 10),
            fake_entry("beta", 20),
            fake_entry("gamma", 30),
        ];
        app.recompute_view();
        app.search_query = "a".to_string();
        app.recompute_view();

        app.toggle_all_filtered();
        assert_eq!(app.selected.len(), 3);

        app.search_query = "beta".to_string();
        app.recompute_view();
        app.invert_filtered_selection();
        assert!(!app.selected.contains(&PathBuf::from("/tmp/beta/target")));
        assert!(app.selected.contains(&PathBuf::from("/tmp/alpha/target")));
    }

    #[test]
    fn deleting_focused_target_removes_only_that_directory_without_rescanning() {
        let temp = TestDir::new();
        let project = temp.path().join("alpha");
        create_project(&project, 1024);
        let untouched = temp.path().join("beta");
        create_project(&untouched, 512);
        let entries = vec![
            measure_target(&project, &project.join("target"), temp.path()).unwrap(),
            measure_target(&untouched, &untouched.join("target"), temp.path()).unwrap(),
        ];

        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.entries = entries;
        app.scan_summary.discovered = app.entries.len();
        app.scan_summary.total_bytes = app.cached_total_bytes();
        app.recompute_view();
        app.selected.insert(app.entries[1].target_path.clone());
        app.start_delete_current();
        wait_for_delete(&mut app);

        assert!(!project.join("target").exists());
        assert!(untouched.join("target").exists());
        assert_eq!(app.entries.len(), 2);
        assert!(matches!(
            app.entries[0].delete_status,
            DeleteStatus::Deleted
        ));
        assert!(matches!(app.entries[1].delete_status, DeleteStatus::Ready));
        assert!(app.selected.contains(&app.entries[1].target_path));
        assert_eq!(app.scan_summary.discovered, 2);
        assert_eq!(app.scan_summary.total_bytes, 512);
        assert_eq!(app.selected_total_bytes(), 512);
    }

    #[test]
    fn failed_delete_stays_cached_and_can_be_retried() {
        let temp = TestDir::new();
        let project = temp.path().join("alpha");
        create_project(&project, 512);
        let entry = measure_target(&project, &project.join("target"), temp.path()).unwrap();
        fs::remove_dir_all(project.join("target")).unwrap();

        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.entries = vec![entry];
        app.scan_summary.discovered = 1;
        app.scan_summary.total_bytes = app.cached_total_bytes();
        app.recompute_view();
        app.start_delete_current();
        wait_for_delete(&mut app);
        assert!(matches!(
            app.entries[0].delete_status,
            DeleteStatus::Failed(_)
        ));
        assert_eq!(app.scan_summary.total_bytes, 512);

        create_project(&project, 512);
        app.start_delete_current();
        wait_for_delete(&mut app);
        assert!(matches!(
            app.entries[0].delete_status,
            DeleteStatus::Deleted
        ));
        assert_eq!(app.scan_summary.total_bytes, 0);
    }

    #[test]
    fn render_empty_and_inline_delete_states() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.mode = AppMode::Browsing;

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let empty = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(empty.contains("No reclaimable target/ found"));

        app.entries = vec![fake_entry("alpha", 1024)];
        app.recompute_view();
        app.entries[0].delete_status = DeleteStatus::Deleting;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let deleting = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(deleting.contains("deleting"));

        app.entries[0].delete_status = DeleteStatus::Deleted;
        app.entries[0].size_bytes = 0;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let deleted = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(deleted.contains("deleted"));
        assert!(deleted.contains("0 B"));

        app.entries[0].delete_status = DeleteStatus::Failed("permission denied".to_string());
        app.entries[0].size_bytes = 1024;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let failed = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(failed.contains("failed"));
    }

    fn scan_sync(root: &Path) -> Vec<TargetEntry> {
        let (tx, rx) = mpsc::channel();
        scan_targets(root.to_path_buf(), tx);
        rx.try_iter()
            .find_map(|message| match message {
                WorkerMessage::ScanComplete { entries, .. } => Some(entries),
                _ => None,
            })
            .unwrap()
    }

    fn create_project(project: &Path, bytes: usize) {
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(project.join("target/artifact.bin"), vec![0_u8; bytes]).unwrap();
    }

    fn fake_entry(name: &str, size_bytes: u64) -> TargetEntry {
        TargetEntry {
            project_root: PathBuf::from(format!("/tmp/{name}")),
            target_path: PathBuf::from(format!("/tmp/{name}/target")),
            display_name: name.to_string(),
            relative_path: format!("./{name}/target"),
            size_bytes,
            original_size_bytes: size_bytes,
            modified_at: UNIX_EPOCH,
            delete_status: DeleteStatus::Ready,
        }
    }

    fn wait_for_delete(app: &mut App) {
        for _ in 0..50 {
            app.drain_worker_messages();
            if app.entries.iter().any(|entry| {
                matches!(
                    entry.delete_status,
                    DeleteStatus::Deleted | DeleteStatus::Failed(_)
                )
            }) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("delete did not finish");
    }

    struct TestDir {
        path: PathBuf,
    }

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let id = NEXT_TEST_DIR_ID.fetch_add(1, AtomicOrdering::Relaxed);
            let path = env::temp_dir().join(format!("purgee-test-{unique}-{id}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
