use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::git::build_snapshot;
use crate::model::{RepoStatus, Snapshot, WorktreeRow, WorktreeState};
use crate::ops::{OperationResult, deploy, merge_all, merge_worktree, rebase_all};
use crate::util::compact;

const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(2);
const PULSE_INTERVAL: Duration = Duration::from_millis(350);
const TRANSIENT_TTL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
enum SnapshotEvent {
    Snapshot(Box<Snapshot>),
    Error(String),
}

#[derive(Clone, Debug)]
enum OperationEvent {
    Line(String),
    Done(OperationResult),
}

#[derive(Clone, Debug)]
enum OperationKind {
    Merge(String),
    MergeAll,
    RebaseAll,
    Deploy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunningOperation {
    name: String,
    label: String,
    lines: Vec<String>,
    result: String,
    ok: Option<bool>,
    done: bool,
}

struct AppState {
    config: Config,
    snapshot: Snapshot,
    selected: usize,
    loading: bool,
    notice: String,
    notice_at: Option<Instant>,
    operation: Option<RunningOperation>,
    last_operation: Option<RunningOperation>,
    operation_rx: Option<Receiver<OperationEvent>>,
    snapshot_rx: Receiver<SnapshotEvent>,
    snapshot_tx: Sender<()>,
    pulse_on: bool,
    last_pulse: Instant,
}

pub fn run(config: Config) -> Result<i32> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal, config);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, config: Config) -> Result<i32> {
    let snapshot = loading_snapshot(&config);
    let (snapshot_tx, snapshot_rx) = start_snapshot_worker(config.clone());
    let _ = snapshot_tx.send(());
    let mut app = AppState {
        config,
        snapshot,
        selected: 0,
        loading: true,
        notice: String::new(),
        notice_at: None,
        operation: None,
        last_operation: None,
        operation_rx: None,
        snapshot_rx,
        snapshot_tx,
        pulse_on: false,
        last_pulse: Instant::now(),
    };

    let mut dirty = true;
    loop {
        let now = Instant::now();
        dirty |= app.poll_snapshot_events();
        dirty |= app.poll_operation_events();
        dirty |= app.clear_transient_notice(now);
        if app.needs_animation() && now.duration_since(app.last_pulse) >= PULSE_INTERVAL {
            app.pulse_on = !app.pulse_on;
            app.last_pulse = now;
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| render(frame, &mut app))?;
            dirty = false;
        }
        let timeout = if app.needs_animation() {
            Duration::from_millis(75)
        } else {
            Duration::from_millis(150)
        };
        if event::poll(timeout)?
            && let CrosstermEvent::Key(key) = event::read()?
        {
            if handle_key(&mut app, key) {
                return Ok(0);
            }
            dirty = true;
        }
    }
}

impl AppState {
    fn poll_snapshot_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.snapshot_rx.try_recv() {
            match event {
                SnapshotEvent::Snapshot(snapshot) => {
                    let snapshot = *snapshot;
                    if snapshot != self.snapshot || self.loading {
                        self.snapshot = snapshot;
                        self.selected = self
                            .selected
                            .min(self.snapshot.worktrees.len().saturating_sub(1));
                        changed = true;
                    }
                    self.loading = false;
                }
                SnapshotEvent::Error(error) => {
                    self.loading = false;
                    self.show_notice(format!("Refresh failed: {error}"));
                    changed = true;
                }
            }
        }
        changed
    }

    fn poll_operation_events(&mut self) -> bool {
        let mut changed = false;
        let mut done = None;
        if let Some(rx) = &self.operation_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    OperationEvent::Line(line) => {
                        if let Some(operation) = &mut self.operation {
                            operation.lines.push(line);
                            if operation.lines.len() > 80 {
                                operation.lines.drain(0..operation.lines.len() - 80);
                            }
                        }
                        changed = true;
                    }
                    OperationEvent::Done(result) => {
                        done = Some(result);
                        changed = true;
                    }
                }
            }
        }
        if let Some(result) = done {
            if let Some(mut operation) = self.operation.take() {
                operation.ok = Some(result.ok);
                operation.result = result.message.clone();
                operation.done = true;
                self.notice = result.message;
                self.notice_at = Some(Instant::now());
                self.last_operation = Some(operation);
            }
            self.operation_rx = None;
            self.refresh();
        }
        changed
    }

    fn clear_transient_notice(&mut self, now: Instant) -> bool {
        if self.operation.is_some() {
            return false;
        }
        let Some(notice_at) = self.notice_at else {
            return false;
        };
        if now.duration_since(notice_at) < TRANSIENT_TTL {
            return false;
        }
        self.notice.clear();
        self.last_operation = None;
        self.notice_at = None;
        true
    }

    fn selected_worktree(&self) -> Option<&WorktreeRow> {
        self.snapshot.worktrees.get(self.selected)
    }

    fn visible_operation(&self) -> Option<&RunningOperation> {
        self.operation.as_ref().or(self.last_operation.as_ref())
    }

    fn refresh(&self) {
        let _ = self.snapshot_tx.send(());
    }

    fn show_notice(&mut self, notice: impl Into<String>) {
        self.notice = notice.into();
        self.notice_at = Some(Instant::now());
        self.last_operation = None;
    }

    fn needs_animation(&self) -> bool {
        self.loading
            || self
                .operation
                .as_ref()
                .is_some_and(|operation| !operation.done)
    }

    fn begin_operation(&mut self, kind: OperationKind) {
        if self.operation.is_some() {
            self.show_notice("Busy: operation running");
            return;
        }
        let name = match &kind {
            OperationKind::Merge(label) => format!("merge {label}"),
            OperationKind::MergeAll => "merge all".to_string(),
            OperationKind::RebaseAll => "rebase all".to_string(),
            OperationKind::Deploy => "deploy".to_string(),
        };
        let label = match &kind {
            OperationKind::Merge(label) => label.clone(),
            _ => String::new(),
        };
        let config = self.config.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut log = |line: String| {
                let _ = tx.send(OperationEvent::Line(line));
            };
            let result = match kind {
                OperationKind::Merge(label) => merge_worktree(&config, &label, &mut log),
                OperationKind::MergeAll => merge_all(&config, &mut log),
                OperationKind::RebaseAll => rebase_all(&config, &mut log),
                OperationKind::Deploy => deploy(&config, &mut log),
            };
            let _ = tx.send(OperationEvent::Done(result));
        });
        self.operation = Some(RunningOperation {
            name,
            label,
            ..Default::default()
        });
        self.last_operation = None;
        self.operation_rx = Some(rx);
    }
}

fn start_snapshot_worker(config: Config) -> (Sender<()>, Receiver<SnapshotEvent>) {
    let (request_tx, request_rx) = mpsc::channel::<()>();
    let (event_tx, event_rx) = mpsc::channel::<SnapshotEvent>();
    thread::spawn(move || {
        let mut last_refresh = Instant::now() - SNAPSHOT_INTERVAL;
        loop {
            let timeout = SNAPSHOT_INTERVAL.saturating_sub(last_refresh.elapsed());
            match request_rx.recv_timeout(timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            while request_rx.try_recv().is_ok() {}
            let event = match std::panic::catch_unwind(|| build_snapshot(&config)) {
                Ok(snapshot) => SnapshotEvent::Snapshot(Box::new(snapshot)),
                Err(_) => SnapshotEvent::Error("snapshot worker panicked".to_string()),
            };
            last_refresh = Instant::now();
            if event_tx.send(event).is_err() {
                return;
            }
        }
    });
    (request_tx, event_rx)
}

fn loading_snapshot(config: &Config) -> Snapshot {
    Snapshot {
        base_branch: config.base_branch.clone(),
        repo: RepoStatus {
            path: config.repo_path.clone(),
            branch: String::new(),
            dirty: false,
            summary: "loading".to_string(),
        },
        worktrees: Vec::new(),
    }
}

fn handle_key(app: &mut AppState, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Down | KeyCode::Char('j') if !app.snapshot.worktrees.is_empty() => {
            app.selected = (app.selected + 1) % app.snapshot.worktrees.len();
        }
        KeyCode::Up | KeyCode::Char('k') if !app.snapshot.worktrees.is_empty() => {
            app.selected = if app.selected == 0 {
                app.snapshot.worktrees.len() - 1
            } else {
                app.selected - 1
            };
        }
        KeyCode::Char('m') => {
            if let Some(row) = app.selected_worktree() {
                app.begin_operation(OperationKind::Merge(row.label.clone()));
            } else {
                app.show_notice("Merge blocked: no worktree selected");
            }
        }
        KeyCode::Char('M') => app.begin_operation(OperationKind::MergeAll),
        KeyCode::Char('u') => app.begin_operation(OperationKind::RebaseAll),
        KeyCode::Char('d') => app.begin_operation(OperationKind::Deploy),
        _ => {}
    }
    false
}

fn render(frame: &mut Frame, app: &mut AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    render_title(frame, chunks[0], app);
    render_summary(frame, chunks[1], app);
    render_body(frame, chunks[2], app);
    render_keybar(frame, chunks[3]);
}

fn render_title(frame: &mut Frame, area: Rect, app: &AppState) {
    let title = Line::from(vec![
        Span::styled("sp", title_style()),
        Span::raw("  "),
        Span::styled("ship small, merge clean", muted_style()),
        Span::raw("  "),
        Span::styled(format!("base: {}", app.config.base_branch), model_style()),
    ]);
    frame.render_widget(Paragraph::new(title).block(panel("Project")), area);
}

fn render_summary(frame: &mut Frame, area: Rect, app: &AppState) {
    let (label, style, parts) = summary_line(app);
    let pulse = if label == "Working" && app.pulse_on {
        Style::default()
            .fg(Color::Rgb(125, 249, 255))
            .add_modifier(Modifier::BOLD)
    } else {
        style_for(style)
    };
    let line = Line::from(vec![
        Span::styled(indicator(&label), style_for(style)),
        Span::raw(" "),
        Span::styled(label, pulse),
        Span::styled(" - ", muted_style()),
        Span::styled(parts.join(", "), text_style()),
    ]);
    frame.render_widget(Paragraph::new(line).block(panel("Next")), area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut AppState) {
    if area.width < 120 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(7), Constraint::Length(8)])
            .split(area);
        render_worktrees(frame, chunks[0], app);
        render_recent(frame, chunks[1], app);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    render_worktrees(frame, chunks[0], app);
    render_recent(frame, chunks[1], app);
}

fn render_worktrees(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let header = Row::new(vec![
        "Agent",
        "Branch",
        "State",
        "Base",
        "Status",
        "Last commit",
    ])
    .style(header_style());
    let rows = app
        .snapshot
        .worktrees
        .iter()
        .map(|row| {
            Row::new(vec![
                Cell::from(row.label.clone()),
                Cell::from(row.branch.clone()),
                Cell::from(row.state.to_string()),
                Cell::from(format!("+{}/-{}", row.ahead, row.behind)),
                Cell::from(row.summary.clone()),
                Cell::from(compact(&row.subject, 96)),
            ])
            .style(worktree_style(row.state))
        })
        .collect::<Vec<_>>();
    let widths = if area.width < 100 {
        [
            Constraint::Length(16),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(10),
        ]
    } else {
        [
            Constraint::Length(18),
            Constraint::Length(24),
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Min(20),
        ]
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(panel("Worktrees"))
        .row_highlight_style(selected_style())
        .highlight_symbol(" ");
    let mut state = TableState::default().with_selected(if app.snapshot.worktrees.is_empty() {
        None
    } else {
        Some(app.selected)
    });
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_recent(frame: &mut Frame, area: Rect, app: &AppState) {
    let lines = recent_lines(app, area.height.saturating_sub(2) as usize)
        .into_iter()
        .map(|line| Line::styled(line.clone(), recent_style(&line)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Recent"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_keybar(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(
            "j/k select  m merge  M merge all  u rebase all  d deploy  r refresh  q quit",
        )
        .style(muted_style())
        .alignment(Alignment::Center),
        area,
    );
}

fn summary_line(app: &AppState) -> (String, &'static str, Vec<String>) {
    if app.loading {
        return (
            "Working".to_string(),
            "working",
            vec!["loading status".to_string()],
        );
    }
    if let Some(operation) = app.visible_operation() {
        if !operation.done {
            return (
                "Working".to_string(),
                "working",
                vec![format!("{} running", operation.name)],
            );
        }
        if operation.ok == Some(false) {
            return (
                "Blocked".to_string(),
                "blocked",
                vec![operation.result.clone()],
            );
        }
        return ("Done".to_string(), "done", vec![operation.result.clone()]);
    }
    if !app.notice.is_empty() {
        let failed = app.notice.to_ascii_lowercase().contains("blocked")
            || app.notice.to_ascii_lowercase().contains("failed");
        return (
            if failed { "Blocked" } else { "Done" }.to_string(),
            if failed { "blocked" } else { "done" },
            vec![app.notice.clone()],
        );
    }
    let summary = app.snapshot.summary();
    let (label, parts) = summary
        .split_once(" - ")
        .map(|(label, rest)| {
            (
                label.to_string(),
                rest.split(", ")
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_else(|| ("Idle".to_string(), vec![summary]));
    let style = match label.as_str() {
        "Ready" => "ready",
        "Blocked" => "blocked",
        _ => "muted",
    };
    (label, style, parts)
}

fn recent_lines(app: &AppState, limit: usize) -> Vec<String> {
    let mut rows = Vec::new();
    if let Some(operation) = app.visible_operation() {
        if !operation.result.is_empty() {
            rows.push(format!("{}: {}", operation.name, operation.result));
        }
        for line in operation.lines.iter().rev().take(limit).rev() {
            rows.push(format!("{}: {line}", operation.name));
            if rows.len() >= limit {
                break;
            }
        }
    }
    if rows.is_empty() {
        rows.push("No recent activity".to_string());
    }
    rows
}

fn indicator(label: &str) -> &'static str {
    match label {
        "Working" => ">",
        "Ready" => "+",
        "Blocked" => "!",
        "Done" => "*",
        _ => "-",
    }
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style())
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::Rgb(248, 250, 252))
        .add_modifier(Modifier::BOLD)
}

fn model_style() -> Style {
    Style::default()
        .fg(Color::Rgb(56, 189, 248))
        .add_modifier(Modifier::BOLD)
}

fn text_style() -> Style {
    Style::default().fg(Color::Rgb(226, 232, 240))
}

fn muted_style() -> Style {
    Style::default().fg(Color::Rgb(100, 116, 139))
}

fn header_style() -> Style {
    Style::default()
        .fg(Color::Rgb(148, 163, 184))
        .add_modifier(Modifier::BOLD)
}

fn border_style() -> Style {
    Style::default().fg(Color::Rgb(30, 64, 112))
}

fn selected_style() -> Style {
    Style::default()
        .bg(Color::Rgb(14, 116, 144))
        .fg(Color::Rgb(248, 250, 252))
        .add_modifier(Modifier::BOLD)
}

fn style_for(kind: &str) -> Style {
    match kind {
        "ready" => Style::default()
            .fg(Color::Rgb(34, 197, 94))
            .add_modifier(Modifier::BOLD),
        "working" => Style::default()
            .fg(Color::Rgb(34, 211, 238))
            .add_modifier(Modifier::BOLD),
        "blocked" => Style::default()
            .fg(Color::Rgb(251, 113, 133))
            .add_modifier(Modifier::BOLD),
        "done" => Style::default()
            .fg(Color::Rgb(74, 222, 128))
            .add_modifier(Modifier::BOLD),
        _ => muted_style(),
    }
}

fn worktree_style(state: WorktreeState) -> Style {
    match state {
        WorktreeState::Ready => style_for("ready"),
        WorktreeState::Dirty | WorktreeState::Missing | WorktreeState::WrongBranch => {
            Style::default().fg(Color::Rgb(250, 204, 21))
        }
        WorktreeState::Behind => Style::default().fg(Color::Rgb(232, 121, 249)),
        WorktreeState::Merged => muted_style(),
    }
}

fn recent_style(line: &str) -> Style {
    let lower = line.to_ascii_lowercase();
    if lower.contains("blocked") || lower.contains("failed") || lower.contains("conflict") {
        style_for("blocked")
    } else if lower.contains("merged") || lower.contains("rebased") || lower.contains("completed") {
        style_for("done")
    } else if line.starts_with('$') || line.contains(": $") {
        muted_style()
    } else {
        text_style()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(snapshot: Snapshot) -> AppState {
        let config = Config {
            repo_path: ".".into(),
            base_branch: "main".to_string(),
            deploy_command: None,
            worktrees: Vec::new(),
        };
        let (snapshot_tx, _snapshot_rx) = mpsc::channel::<()>();
        let (_event_tx, snapshot_rx) = mpsc::channel::<SnapshotEvent>();
        AppState {
            config,
            snapshot,
            selected: 0,
            loading: false,
            notice: String::new(),
            notice_at: None,
            operation: None,
            last_operation: None,
            operation_rx: None,
            snapshot_rx,
            snapshot_tx,
            pulse_on: false,
            last_pulse: Instant::now(),
        }
    }

    #[test]
    fn summary_uses_snapshot_state() {
        let snapshot = Snapshot {
            base_branch: "main".to_string(),
            repo: RepoStatus {
                path: ".".into(),
                branch: "main".to_string(),
                dirty: false,
                summary: "clean".to_string(),
            },
            worktrees: vec![WorktreeRow {
                label: "agent-A".to_string(),
                branch: "agent/A".to_string(),
                path: ".".into(),
                current_branch: "agent/A".to_string(),
                state: WorktreeState::Ready,
                dirty: false,
                summary: "clean".to_string(),
                ahead: 1,
                behind: 0,
                subject: "ship".to_string(),
            }],
        };

        let (label, style, parts) = summary_line(&app(snapshot));

        assert_eq!("Ready", label);
        assert_eq!("ready", style);
        assert_eq!(vec!["1 branch ready".to_string()], parts);
    }

    #[test]
    fn key_recent_keeps_operation_commands_visible() {
        let mut app = app(loading_snapshot(&Config {
            repo_path: ".".into(),
            base_branch: "main".to_string(),
            deploy_command: None,
            worktrees: Vec::new(),
        }));
        app.loading = false;
        app.operation = Some(RunningOperation {
            name: "merge agent-A".to_string(),
            lines: vec!["$ git rebase main".to_string()],
            ..Default::default()
        });

        assert!(
            recent_lines(&app, 4)
                .join("\n")
                .contains("$ git rebase main")
        );
    }
}
