use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
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
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap,
};
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
    Browsing,
    Searching,
    SortMenu { index: usize },
    Help,
}

#[derive(Clone, Debug, Default)]
struct ScanSummary {
    found: usize,
    measured: usize,
    in_flight: usize,
    total_bytes: u64,
    current_path: Option<PathBuf>,
    elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteStatus {
    Ready,
    Queued,
    Deleting,
    Deleted,
    Failed(String),
}

#[derive(Clone, Debug)]
enum WorkerMessage {
    ScanDiscovered {
        found: usize,
        in_flight: usize,
        current_path: PathBuf,
    },
    ScanMeasured {
        entry: TargetEntry,
        measured: usize,
        in_flight: usize,
    },
    ScanMeasurementFailed {
        warning: String,
        measured: usize,
        in_flight: usize,
        current_path: PathBuf,
    },
    ScanWarning {
        warning: String,
    },
    ScanComplete {
        elapsed: Duration,
    },
    DeleteProgress {
        path: PathBuf,
        status: DeleteStatus,
    },
}

type MeasureTargetFn = Arc<dyn Fn(&Path, &Path, &Path) -> io::Result<TargetEntry> + Send + Sync>;
type DeleteRunner = Arc<dyn Fn(PathBuf, TargetEntry, Sender<WorkerMessage>) + Send + Sync>;

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
    scan_in_progress: bool,
    scan_cursor_moved: bool,
    scan_summary: ScanSummary,
    scan_warnings: Vec<String>,
    delete_queue: VecDeque<PathBuf>,
    active_deletes: usize,
    delete_worker_limit: usize,
    delete_runner: DeleteRunner,
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
            mode: AppMode::Browsing,
            scan_in_progress: false,
            scan_cursor_moved: false,
            scan_summary: ScanSummary::default(),
            scan_warnings: Vec::new(),
            delete_queue: VecDeque::new(),
            active_deletes: 0,
            delete_worker_limit: delete_worker_limit(),
            delete_runner: Arc::new(delete_target),
            table_state: TableState::default(),
            worker_tx,
            worker_rx,
            should_quit: false,
            animation_tick: 0,
            last_tick: Instant::now(),
        }
    }

    fn start_scan(&mut self) {
        if !self.can_start_scan() {
            return;
        }
        self.scan_in_progress = true;
        self.scan_cursor_moved = false;
        self.entries.clear();
        self.filtered_indices.clear();
        self.cursor = 0;
        self.scan_summary = ScanSummary::default();
        self.scan_warnings.clear();

        let root = self.root.clone();
        let tx = self.worker_tx.clone();
        thread::spawn(move || scan_targets(root, tx, scan_worker_limit()));
    }

    fn drain_worker_messages(&mut self) {
        while let Ok(message) = self.worker_rx.try_recv() {
            match message {
                WorkerMessage::ScanDiscovered {
                    found,
                    in_flight,
                    current_path,
                } => {
                    self.scan_summary.found = found;
                    self.scan_summary.in_flight = in_flight;
                    self.scan_summary.current_path = Some(current_path);
                }
                WorkerMessage::ScanMeasured {
                    entry,
                    measured,
                    in_flight,
                } => {
                    let focused_path = self.current_entry().map(|entry| entry.target_path.clone());
                    self.scan_summary.current_path = Some(entry.target_path.clone());
                    self.entries.push(entry);
                    self.scan_summary.measured = measured;
                    self.scan_summary.in_flight = in_flight;
                    self.scan_summary.total_bytes = self.cached_total_bytes();
                    self.recompute_view_with_focus(focused_path);
                }
                WorkerMessage::ScanMeasurementFailed {
                    warning,
                    measured,
                    in_flight,
                    current_path,
                } => {
                    self.scan_warnings.push(warning);
                    self.scan_summary.measured = measured;
                    self.scan_summary.in_flight = in_flight;
                    self.scan_summary.current_path = Some(current_path);
                }
                WorkerMessage::ScanWarning { warning } => {
                    self.scan_warnings.push(warning);
                }
                WorkerMessage::ScanComplete { elapsed } => {
                    self.scan_in_progress = false;
                    self.scan_summary.current_path = None;
                    self.scan_summary.elapsed = elapsed;
                    let known_paths: HashSet<_> = self
                        .entries
                        .iter()
                        .map(|entry| entry.target_path.clone())
                        .collect();
                    self.selected.retain(|path| known_paths.contains(path));
                    let focused_path = self
                        .scan_cursor_moved
                        .then(|| self.current_entry().map(|entry| entry.target_path.clone()))
                        .flatten();
                    if focused_path.is_none() {
                        self.cursor = 0;
                    }
                    self.recompute_view_with_focus(focused_path);
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
                    self.active_deletes = self.active_deletes.saturating_sub(1);
                    self.start_queued_deletes();
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
                self.set_cursor(0);
            }
            KeyCode::Char('G') => {
                self.set_cursor(self.filtered_indices.len().saturating_sub(1));
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
            KeyCode::Char('D') => self.start_delete_selected(),
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
        let Some(path) = self.current_entry().map(|entry| entry.target_path.clone()) else {
            return;
        };
        self.enqueue_delete_paths([path]);
    }

    fn start_delete_selected(&mut self) {
        let paths = self
            .selected_entries()
            .into_iter()
            .map(|entry| entry.target_path.clone())
            .collect::<Vec<_>>();
        self.enqueue_delete_paths(paths);
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
        let focused_path = self.current_entry().map(|entry| entry.target_path.clone());
        self.recompute_view_with_focus(focused_path);
    }

    fn recompute_view_with_focus(&mut self, focused_path: Option<PathBuf>) {
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
        self.cursor = focused_path
            .as_ref()
            .and_then(|path| {
                self.filtered_indices.iter().position(|index| {
                    self.entries
                        .get(*index)
                        .is_some_and(|entry| entry.target_path == *path)
                })
            })
            .unwrap_or_else(|| {
                self.cursor
                    .min(self.filtered_indices.len().saturating_sub(1))
            });
        self.sync_table_state();
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered_indices.is_empty() {
            self.set_cursor(0);
            return;
        }

        let cursor = if delta.is_negative() {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            (self.cursor + delta as usize).min(self.filtered_indices.len() - 1)
        };
        self.set_cursor(cursor);
    }

    fn set_cursor(&mut self, cursor: usize) {
        if self.cursor != cursor && self.scan_in_progress {
            self.scan_cursor_moved = true;
        }
        self.cursor = cursor;
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

    fn can_start_scan(&self) -> bool {
        !self.scan_in_progress && self.active_deletes == 0 && self.delete_queue.is_empty()
    }

    fn enqueue_delete_paths<I>(&mut self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        for path in paths {
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.target_path == path)
            else {
                continue;
            };
            if !entry.is_selectable() {
                continue;
            }
            entry.delete_status = DeleteStatus::Queued;
            self.delete_queue.push_back(path);
        }
        self.start_queued_deletes();
    }

    fn start_queued_deletes(&mut self) {
        while self.active_deletes < self.delete_worker_limit {
            let Some(path) = self.delete_queue.pop_front() else {
                return;
            };
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.target_path == path)
            else {
                continue;
            };
            if !matches!(entry.delete_status, DeleteStatus::Queued) {
                continue;
            }
            entry.delete_status = DeleteStatus::Deleting;
            let entry = entry.clone();
            self.active_deletes += 1;
            let root = self.root.clone();
            let tx = self.worker_tx.clone();
            let delete_runner = Arc::clone(&self.delete_runner);
            thread::spawn(move || delete_runner(root, entry, tx));
        }
    }

    fn needs_animation(&self) -> bool {
        self.scan_in_progress
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
            DeleteStatus::Queued | DeleteStatus::Deleting | DeleteStatus::Deleted
        )
    }
}

#[derive(Clone, Debug)]
struct ScanJob {
    project_root: PathBuf,
    target_path: PathBuf,
}

fn scan_targets(root: PathBuf, tx: Sender<WorkerMessage>, worker_limit: usize) {
    scan_targets_with(
        root,
        tx,
        worker_limit,
        Arc::new(measure_target as fn(&Path, &Path, &Path) -> io::Result<TargetEntry>),
    );
}

fn scan_targets_with(
    root: PathBuf,
    tx: Sender<WorkerMessage>,
    worker_limit: usize,
    measure_target: MeasureTargetFn,
) {
    let started = Instant::now();
    let worker_limit = worker_limit.max(1);
    let (job_tx, job_rx) = mpsc::sync_channel::<ScanJob>(worker_limit.saturating_mul(2).max(1));
    let job_rx = Arc::new(Mutex::new(job_rx));
    let found = Arc::new(AtomicUsize::new(0));
    let measured = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::with_capacity(worker_limit);
    for _ in 0..worker_limit {
        let job_rx = Arc::clone(&job_rx);
        let found = Arc::clone(&found);
        let measured = Arc::clone(&measured);
        let root = root.clone();
        let tx = tx.clone();
        let measure_target = Arc::clone(&measure_target);
        workers.push(thread::spawn(move || {
            loop {
                let job = {
                    let receiver = job_rx.lock().expect("scan job receiver poisoned");
                    receiver.recv()
                };
                let Ok(job) = job else {
                    return;
                };

                match measure_target(&job.project_root, &job.target_path, &root) {
                    Ok(entry) => {
                        let measured = measured.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                        let in_flight =
                            found.load(AtomicOrdering::Relaxed).saturating_sub(measured);
                        let _ = tx.send(WorkerMessage::ScanMeasured {
                            entry,
                            measured,
                            in_flight,
                        });
                    }
                    Err(error) => {
                        let measured = measured.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                        let in_flight =
                            found.load(AtomicOrdering::Relaxed).saturating_sub(measured);
                        let _ = tx.send(WorkerMessage::ScanMeasurementFailed {
                            warning: format!("{}: {error}", job.target_path.display()),
                            measured,
                            in_flight,
                            current_path: job.target_path,
                        });
                    }
                }
            }
        }));
    }

    let mut seen_targets = HashSet::new();

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
                        let _ = tx.send(WorkerMessage::ScanWarning {
                            warning: format!("{}: {error}", target_path.display()),
                        });
                        continue;
                    }
                };
                if !seen_targets.insert(canonical_target.clone()) {
                    continue;
                }

                let job = ScanJob {
                    project_root: project_root.to_path_buf(),
                    target_path: canonical_target.clone(),
                };
                if job_tx.send(job).is_err() {
                    break;
                }
                let found = found.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                let in_flight = found.saturating_sub(measured.load(AtomicOrdering::Relaxed));
                let _ = tx.send(WorkerMessage::ScanDiscovered {
                    found,
                    in_flight,
                    current_path: canonical_target,
                });
            }
            Ok(_) => {}
            Err(error) => {
                let _ = tx.send(WorkerMessage::ScanWarning {
                    warning: error.to_string(),
                });
            }
        }
    }

    drop(job_tx);
    for worker in workers {
        let _ = worker.join();
    }
    let _ = tx.send(WorkerMessage::ScanComplete {
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

fn scan_worker_limit() -> usize {
    auto_worker_limit(8)
}

fn delete_worker_limit() -> usize {
    auto_worker_limit(4)
}

fn auto_worker_limit(maximum: usize) -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(1, maximum)
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
    let area = inset_rect(frame.area(), 2, 1);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_stats(frame, layout[2], app);
    render_search(frame, layout[4], app);
    render_table_or_state(frame, layout[5], app);
    render_footer(frame, layout[6], app);

    match app.mode {
        AppMode::SortMenu { index } => render_sort_menu(frame, app, index),
        AppMode::Help => render_help(frame),
        _ => {}
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let deleting_count = app
        .entries
        .iter()
        .filter(|entry| matches!(entry.delete_status, DeleteStatus::Deleting))
        .count();
    let queued_count = app
        .entries
        .iter()
        .filter(|entry| matches!(entry.delete_status, DeleteStatus::Queued))
        .count();
    let status = if app.scan_in_progress {
        let mut status = format!(
            "{} scanning {}/{} measured · {} active · {} so far",
            spinner(app.animation_tick),
            app.scan_summary.measured,
            app.scan_summary.found,
            app.scan_summary.in_flight,
            format_bytes(app.scan_summary.total_bytes)
        );
        if deleting_count > 0 || queued_count > 0 {
            status.push_str(&format!(
                " · {deleting_count} deleting · {queued_count} queued"
            ));
        }
        status
    } else if deleting_count > 0 || queued_count > 0 {
        format!(
            "{} deleting {} · {} queued",
            spinner(app.animation_tick),
            deleting_count,
            queued_count
        )
    } else if app.scan_warnings.is_empty() {
        format!(
            "{} scanned · {} remaining · {}",
            app.entries.len(),
            app.remaining_target_count(),
            format_duration(app.scan_summary.elapsed)
        )
    } else {
        format!(
            "{} scanned · {} remaining · {} warnings · {}",
            app.entries.len(),
            app.remaining_target_count(),
            app.scan_warnings.len(),
            format_duration(app.scan_summary.elapsed)
        )
    };
    let status = truncate_end_text(&status, (area.width as usize / 2).max(12));
    let root = truncate_middle_text(
        &app.root.display().to_string(),
        (area.width as usize)
            .saturating_sub(status.chars().count())
            .saturating_sub("purgee   ".chars().count())
            .max(8),
    );
    let left = Line::from(vec![
        Span::styled(
            "purgee",
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(root, Style::default().fg(TEXT_SECONDARY)),
    ]);
    let right = Line::from(Span::styled(status, Style::default().fg(TEXT_MUTED)));
    render_spaced_line(frame, area, left, right);
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
        .style(Style::default().bg(SURFACE))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content = vec![
        Line::from(Span::styled(
            title.to_uppercase(),
            Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
        )),
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
    let left = truncate_end_text(
        &left,
        (area.width as usize)
            .saturating_sub(right.chars().count())
            .saturating_sub(3)
            .max(8),
    );
    let search_style = if app.search_query.is_empty() && !matches!(app.mode, AppMode::Searching) {
        Style::default().fg(TEXT_MUTED).bg(BG)
    } else {
        Style::default().fg(TEXT_PRIMARY).bg(BG)
    };
    let prefix = if matches!(app.mode, AppMode::Searching) {
        Span::styled("▎ ", Style::default().fg(ACCENT_PRIMARY))
    } else {
        Span::raw("  ")
    };
    let left = Line::from(vec![prefix, Span::styled(left, search_style)]);
    let right = Line::from(Span::styled(right, Style::default().fg(TEXT_SECONDARY)));
    render_spaced_line(frame, area, left, right);
}

fn render_table_or_state(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if app.scan_in_progress && app.entries.is_empty() {
        render_scan_state(frame, area, app);
        return;
    }

    if app.entries.is_empty() {
        render_empty_state(frame, area);
        return;
    }

    let duplicate_names = duplicate_display_names(&app.entries);
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
                DeleteStatus::Queued => " queued".to_string(),
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
                    &format!(
                        "{}{}",
                        display_label(entry, &duplicate_names),
                        status_suffix
                    ),
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

    let widths = [
        Constraint::Length(4),
        Constraint::Percentage(28),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Min(18),
    ];
    let has_session_summary = app.cleaned_session_summary().is_some();
    let mut constraints = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ];
    if has_session_summary {
        constraints.push(Constraint::Length(1));
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let header = Table::new(
        vec![
            Row::new(vec!["", "PROJECT", "SIZE", "MODIFIED", "TARGET"])
                .style(Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD)),
        ],
        widths,
    )
    .column_spacing(1);
    frame.render_widget(header, sections[0]);
    render_rule(frame, sections[1]);

    let table = Table::new(rows, widths).column_spacing(1);
    app.sync_table_state();
    frame.render_stateful_widget(table, sections[2], &mut app.table_state);
    if has_session_summary {
        render_rule(frame, sections[3]);
    }
}

fn duplicate_display_names(entries: &[TargetEntry]) -> HashSet<&str> {
    let mut counts = HashMap::new();
    for entry in entries {
        *counts.entry(entry.display_name.as_str()).or_insert(0_usize) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

fn display_label(entry: &TargetEntry, duplicate_names: &HashSet<&str>) -> String {
    if !duplicate_names.contains(entry.display_name.as_str()) {
        return entry.display_name.clone();
    }

    let qualifier = entry
        .project_root
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(short_worktree_qualifier)
        .filter(|qualifier| !qualifier.is_empty());
    match qualifier {
        Some(qualifier) => format!("{} · {qualifier}", entry.display_name),
        None => entry.display_name.clone(),
    }
}

fn short_worktree_qualifier(name: &str) -> String {
    name.strip_prefix("codex.fcoury-")
        .or_else(|| name.strip_prefix("codex-"))
        .unwrap_or(name)
        .to_string()
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
                "{} targets found · {} measured · {} so far",
                app.scan_summary.found,
                app.scan_summary.measured,
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
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
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
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = match app.mode {
        AppMode::Searching => footer_spans(&[
            ("esc", "cancel", true),
            ("enter", "apply", true),
            ("type", "filter", true),
        ]),
        AppMode::SortMenu { .. } => footer_spans(&[
            ("↑↓", "pick", true),
            ("enter", "apply", true),
            ("s", "toggle", true),
            ("esc", "close", true),
        ]),
        _ => {
            let can_rescan = app.can_start_scan();
            footer_spans(&[
                ("↑↓", "navigate", true),
                ("space", "select", true),
                ("a", "all", true),
                ("i", "invert", true),
                ("/", "search", true),
                ("s", "sort", true),
                ("d", "delete focused", true),
                ("D", "delete selected", true),
                ("r", if can_rescan { "rescan" } else { "busy" }, can_rescan),
                ("?", "help", true),
                ("q", "quit", true),
            ])
        }
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
        "D           Delete selected rows immediately",
        "r           Rescan when idle",
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

fn footer_spans(pairs: &[(&str, &str, bool)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (key, action, enabled)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let key_style = if *enabled {
            Style::default()
                .fg(ACCENT_PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_MUTED)
        };
        spans.push(Span::styled((*key).to_string(), key_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*action).to_string(),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    spans
}

fn render_spaced_line(frame: &mut Frame<'_>, area: Rect, left: Line<'_>, right: Line<'_>) {
    let spaces = (area.width as usize)
        .saturating_sub(left.width() + right.width())
        .max(1);
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(spaces)));
    spans.extend(right.spans);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_rule(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(BORDER),
        )),
        area,
    );
}

fn inset_rect(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(horizontal),
        area.y.saturating_add(vertical),
        area.width.saturating_sub(horizontal.saturating_mul(2)),
        area.height.saturating_sub(vertical.saturating_mul(2)),
    )
}

fn truncate_end_text(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let keep = max_width - 1;
    format!("{}…", text.chars().take(keep).collect::<String>())
}

fn truncate_middle_text(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let keep = max_width - 1;
    let start = keep.div_ceil(2);
    let end = keep / 2;
    let suffix = text
        .chars()
        .rev()
        .take(end)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{}…{suffix}", text.chars().take(start).collect::<String>())
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
        DeleteStatus::Queued => Span::styled(
            "…".to_string(),
            Style::default()
                .fg(ACCENT_WARNING)
                .add_modifier(Modifier::BOLD),
        ),
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
    fn duplicate_project_names_show_parent_worktree_qualifier() {
        let entries = vec![
            TargetEntry {
                project_root: PathBuf::from("/tmp/codex.fcoury-suggest-next/codex-rs"),
                target_path: PathBuf::from("/tmp/codex.fcoury-suggest-next/codex-rs/target"),
                display_name: "codex-rs".to_string(),
                relative_path: "./codex.fcoury-suggest-next/codex-rs/target".to_string(),
                size_bytes: 10,
                original_size_bytes: 10,
                modified_at: UNIX_EPOCH,
                delete_status: DeleteStatus::Ready,
            },
            TargetEntry {
                project_root: PathBuf::from("/tmp/codex.fcoury-startup-suggest/codex-rs"),
                target_path: PathBuf::from("/tmp/codex.fcoury-startup-suggest/codex-rs/target"),
                display_name: "codex-rs".to_string(),
                relative_path: "./codex.fcoury-startup-suggest/codex-rs/target".to_string(),
                size_bytes: 20,
                original_size_bytes: 20,
                modified_at: UNIX_EPOCH,
                delete_status: DeleteStatus::Ready,
            },
        ];

        let duplicates = duplicate_display_names(&entries);

        assert_eq!(
            display_label(&entries[0], &duplicates),
            "codex-rs · suggest-next"
        );
        assert_eq!(
            display_label(&entries[1], &duplicates),
            "codex-rs · startup-suggest"
        );
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
        app.scan_summary.found = app.entries.len();
        app.scan_summary.measured = app.entries.len();
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
        assert_eq!(app.scan_summary.found, 2);
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
        app.scan_summary.found = 1;
        app.scan_summary.measured = 1;
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

    #[test]
    fn scan_measurements_stream_before_scan_completion() {
        let temp = TestDir::new();
        create_project(&temp.path().join("alpha"), 1024);
        create_project(&temp.path().join("beta"), 2048);
        let measure_target = Arc::new(
            |project_root: &Path, target_path: &Path, scan_root: &Path| {
                if project_root.file_name().and_then(|name| name.to_str()) == Some("alpha") {
                    thread::sleep(Duration::from_millis(40));
                }
                measure_target(project_root, target_path, scan_root)
            },
        );
        let (tx, rx) = mpsc::channel();

        scan_targets_with(temp.path().to_path_buf(), tx, 2, measure_target);

        let messages = rx.try_iter().collect::<Vec<_>>();
        let measured_names = messages
            .iter()
            .filter_map(|message| match message {
                WorkerMessage::ScanMeasured { entry, .. } => Some(entry.display_name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(measured_names, vec!["beta", "alpha"]);
        assert!(matches!(
            messages.last(),
            Some(WorkerMessage::ScanComplete { .. })
        ));
    }

    #[test]
    fn measured_scan_rows_are_interactive_before_scan_completion() {
        let temp = TestDir::new();
        let project = temp.path().join("alpha");
        create_project(&project, 1024);
        let entry = measure_target(&project, &project.join("target"), temp.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.scan_in_progress = true;
        app.worker_tx
            .send(WorkerMessage::ScanMeasured {
                entry,
                measured: 1,
                in_flight: 0,
            })
            .unwrap();

        app.drain_worker_messages();
        app.toggle_current_selection();
        app.start_delete_current();
        wait_for_delete(&mut app);

        assert!(matches!(
            app.entries[0].delete_status,
            DeleteStatus::Deleted
        ));
    }

    #[test]
    fn scan_updates_preserve_focused_target_after_resort() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.entries = vec![fake_entry("alpha", 10), fake_entry("beta", 20)];
        app.recompute_view();
        app.cursor = 1;
        let focused_path = app.current_entry().unwrap().target_path.clone();
        app.worker_tx
            .send(WorkerMessage::ScanMeasured {
                entry: fake_entry("gamma", 30),
                measured: 3,
                in_flight: 0,
            })
            .unwrap();

        app.drain_worker_messages();

        assert_eq!(app.current_entry().unwrap().target_path, focused_path);
    }

    #[test]
    fn scan_completion_focuses_largest_row_without_user_navigation() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.scan_in_progress = true;
        app.worker_tx
            .send(WorkerMessage::ScanMeasured {
                entry: fake_entry("alpha", 10),
                measured: 1,
                in_flight: 1,
            })
            .unwrap();
        app.worker_tx
            .send(WorkerMessage::ScanMeasured {
                entry: fake_entry("gamma", 30),
                measured: 2,
                in_flight: 0,
            })
            .unwrap();

        app.drain_worker_messages();
        assert_eq!(app.current_entry().unwrap().display_name, "alpha");

        app.worker_tx
            .send(WorkerMessage::ScanComplete {
                elapsed: Duration::from_millis(5),
            })
            .unwrap();
        app.drain_worker_messages();

        assert_eq!(app.current_entry().unwrap().display_name, "gamma");
    }

    #[test]
    fn scan_completion_preserves_user_navigation() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.scan_in_progress = true;
        for (measured, entry) in [
            (1, fake_entry("alpha", 10)),
            (2, fake_entry("gamma", 30)),
            (3, fake_entry("beta", 20)),
        ] {
            app.worker_tx
                .send(WorkerMessage::ScanMeasured {
                    entry,
                    measured,
                    in_flight: 3 - measured,
                })
                .unwrap();
        }

        app.drain_worker_messages();
        assert_eq!(app.current_entry().unwrap().display_name, "alpha");
        app.move_cursor(-1);
        assert_eq!(app.current_entry().unwrap().display_name, "beta");

        app.worker_tx
            .send(WorkerMessage::ScanComplete {
                elapsed: Duration::from_millis(5),
            })
            .unwrap();
        app.drain_worker_messages();

        assert_eq!(app.current_entry().unwrap().display_name, "beta");
    }

    #[test]
    fn rescan_is_ignored_while_scan_is_busy() {
        let temp = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), tx, rx);
        app.entries = vec![fake_entry("alpha", 10)];
        app.recompute_view();
        app.scan_in_progress = true;

        app.handle_browsing_key(KeyEvent::from(KeyCode::Char('r')));

        assert_eq!(app.entries.len(), 1);
        assert!(app.scan_in_progress);
    }

    #[test]
    fn selected_delete_uses_bounded_queue() {
        let temp = TestDir::new();
        let alpha = temp.path().join("alpha");
        let beta = temp.path().join("beta");
        create_project(&alpha, 1024);
        create_project(&beta, 2048);
        let entries = vec![
            measure_target(&alpha, &alpha.join("target"), temp.path()).unwrap(),
            measure_target(&beta, &beta.join("target"), temp.path()).unwrap(),
        ];
        let (worker_tx, worker_rx) = mpsc::channel();
        let mut app = App::new(temp.path().to_path_buf(), worker_tx, worker_rx);
        app.entries = entries;
        app.recompute_view();
        app.delete_worker_limit = 1;
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        app.delete_runner = Arc::new(move |_root, entry, tx| {
            started_tx.send(entry.target_path.clone()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            tx.send(WorkerMessage::DeleteProgress {
                path: entry.target_path,
                status: DeleteStatus::Deleted,
            })
            .unwrap();
        });
        for entry in &app.entries {
            app.selected.insert(entry.target_path.clone());
        }

        app.start_delete_selected();

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            app.entries
                .iter()
                .filter(|entry| matches!(entry.delete_status, DeleteStatus::Deleting))
                .count(),
            1
        );
        assert_eq!(
            app.entries
                .iter()
                .filter(|entry| matches!(entry.delete_status, DeleteStatus::Queued))
                .count(),
            1
        );

        release_tx.send(()).unwrap();
        wait_for_delete_count(&mut app, 1);
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        wait_for_delete_count(&mut app, 2);

        assert!(
            app.entries
                .iter()
                .all(|entry| matches!(entry.delete_status, DeleteStatus::Deleted))
        );
    }

    fn scan_sync(root: &Path) -> Vec<TargetEntry> {
        let (tx, rx) = mpsc::channel();
        scan_targets(root.to_path_buf(), tx, 2);
        rx.try_iter()
            .filter_map(|message| match message {
                WorkerMessage::ScanMeasured { entry, .. } => Some(entry),
                _ => None,
            })
            .collect()
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

    fn wait_for_delete_count(app: &mut App, count: usize) {
        for _ in 0..50 {
            app.drain_worker_messages();
            if app
                .entries
                .iter()
                .filter(|entry| matches!(entry.delete_status, DeleteStatus::Deleted))
                .count()
                >= count
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("expected {count} deletes to finish");
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
