use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use ratatui::widgets::Gauge;

use crate::{
    models::{RateLimitProgress, Repository, SyncEvent, SyncProgress, SyncSummary, Team},
    state::{RepositoryProgressSummary, StateStore, now_epoch_ms},
};

#[derive(Debug, Clone)]
pub struct StartupWaitSnapshot {
    pub used: usize,
    pub usable: usize,
    pub current_time: String,
    pub resume_time: String,
    pub remaining: String,
    pub remaining_seconds: i64,
}

#[derive(Debug, Clone)]
enum Row {
    Team(usize),
    Repository(usize, usize),
}

pub fn show_startup_rate_limit_wait(
    host: &str,
    mut snapshot: impl FnMut() -> Result<Option<StartupWaitSnapshot>>,
) -> Result<bool> {
    let Some(initial) = snapshot()? else {
        return Ok(true);
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_startup_rate_limit_wait(&mut terminal, host, initial, snapshot);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

pub fn select_repositories(host: &str, teams: &mut [Team], state: &StateStore) -> Result<bool> {
    let previous: HashSet<_> = state.selected_repo_ids(host)?.into_iter().collect();
    for team in teams.iter_mut() {
        for repo in &mut team.repositories {
            repo.selected = previous.contains(&repo.id);
        }
    }
    let progress = state.repository_progress_by_host(host)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_selection(&mut terminal, host, teams, &progress);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

pub fn confirm_resume(host: &str, teams: &[Team], state: &StateStore) -> Result<bool> {
    let progress = state.repository_progress_by_host(host)?;
    let has_unfinished = teams
        .iter()
        .flat_map(|team| &team.repositories)
        .filter(|repo| repo.selected)
        .filter_map(|repo| progress.get(&repo.id))
        .any(|summary| summary.needs_resume());
    if !has_unfinished {
        return Ok(true);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_resume_confirmation(&mut terminal, host, teams, &progress);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run_startup_rate_limit_wait(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    host: &str,
    initial: StartupWaitSnapshot,
    mut snapshot: impl FnMut() -> Result<Option<StartupWaitSnapshot>>,
) -> Result<bool> {
    let initial_seconds = initial.remaining_seconds.max(1);
    let mut current = initial;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(7),
                    Constraint::Min(1),
                    Constraint::Length(3),
                ])
                .split(area);

            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "API 限流等待  ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(host),
                ]))
                .block(Block::default().borders(Borders::ALL).title("连接")),
                chunks[0],
            );

            let elapsed = initial_seconds.saturating_sub(current.remaining_seconds);
            let ratio = if initial_seconds == 0 {
                1.0
            } else {
                (elapsed as f64 / initial_seconds as f64).clamp(0.0, 1.0)
            };
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL).title("等待进度"))
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .ratio(ratio)
                    .label(format!("剩余 {}", current.remaining)),
                chunks[1],
            );

            let details = vec![
                Line::raw(format!(
                    "过去一小时 API 用量: {}/{}",
                    current.used, current.usable
                )),
                Line::raw(format!("当前时间: {}", current.current_time)),
                Line::raw(format!("预计恢复: {}", current.resume_time)),
                Line::raw(format!("剩余时间: {}", current.remaining)),
                Line::raw("窗口恢复后会自动继续进入 TUI。"),
            ];
            frame.render_widget(
                Paragraph::new(details).block(Block::default().borders(Borders::ALL).title("状态")),
                chunks[2],
            );

            frame.render_widget(
                Paragraph::new("q/Esc 退出")
                    .block(Block::default().borders(Borders::ALL).title("操作")),
                chunks[4],
            );
        })?;

        if event::poll(Duration::from_secs(1))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                return Ok(false);
            }
        }

        let Some(next) = snapshot()? else {
            return Ok(true);
        };
        current = next;
    }
}

fn run_selection(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    host: &str,
    teams: &mut [Team],
    progress: &HashMap<String, RepositoryProgressSummary>,
) -> Result<bool> {
    let rows = build_rows(teams);
    let mut list_state = ListState::default().with_selected(Some(0));
    let mut warning = String::new();
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(3),
                ])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "Yuque Backup  ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(host),
                ]))
                .block(Block::default().borders(Borders::ALL).title("连接")),
                chunks[0],
            );

            let items: Vec<ListItem> = rows
                .iter()
                .map(|row| match *row {
                    Row::Team(index) => {
                        let team = &teams[index];
                        let selected = team.repositories.iter().filter(|r| r.selected).count();
                        let marker = if selected == 0 {
                            "[ ]"
                        } else if selected == team.repositories.len() {
                            "[x]"
                        } else {
                            "[-]"
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!("{marker} {}", team.name),
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" [{}]", team.kind.as_str()),
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::raw(format!("  {selected}/{}", team.repositories.len())),
                        ]))
                    }
                    Row::Repository(team_index, repo_index) => {
                        let repo = &teams[team_index].repositories[repo_index];
                        let marker = if repo.selected { "[x]" } else { "[ ]" };
                        ListItem::new(Line::from(vec![
                            Span::raw(format!("    {marker} {}", repo.name)),
                            Span::styled(
                                format!("  {}", repository_progress_label(repo, progress)),
                                repository_progress_style(progress.get(&repo.id)),
                            ),
                        ]))
                    }
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("团队 / 知识库"),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            frame.render_stateful_widget(list, chunks[1], &mut list_state);
            let help = if warning.is_empty() {
                "↑/↓ 移动  Space 选择  a 全选/清空  Enter 保存并开始  q 退出".to_string()
            } else {
                format!("{warning}  |  Space 选择  Enter 保存并开始  q 退出")
            };
            frame.render_widget(
                Paragraph::new(help).block(Block::default().borders(Borders::ALL).title("操作")),
                chunks[2],
            );
        })?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let current = list_state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    list_state.select(Some(current.saturating_sub(1)))
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    list_state.select(Some((current + 1).min(rows.len().saturating_sub(1))))
                }
                KeyCode::Char(' ') => {
                    toggle_row(&rows[current], teams);
                    warning.clear();
                }
                KeyCode::Char('a') => {
                    let all_selected = teams
                        .iter()
                        .flat_map(|t| &t.repositories)
                        .all(|r| r.selected);
                    teams
                        .iter_mut()
                        .flat_map(|t| &mut t.repositories)
                        .for_each(|r| r.selected = !all_selected);
                    warning.clear();
                }
                KeyCode::Enter => {
                    if teams
                        .iter()
                        .flat_map(|team| &team.repositories)
                        .any(|repo| repo.selected)
                    {
                        return Ok(true);
                    }
                    warning = "没有选择任何知识库，请至少选择一个".into();
                }
                KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
                _ => {}
            }
        }
    }
}

fn run_resume_confirmation(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    host: &str,
    teams: &[Team],
    progress: &HashMap<String, RepositoryProgressSummary>,
) -> Result<bool> {
    let rows = build_resume_confirmation_rows(teams, progress);
    if rows.is_empty() {
        return Ok(true);
    }
    let mut list_state = ListState::default().with_selected(Some(0));
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(area);

            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "检测到上次同步记录  ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(host),
                ]))
                .block(Block::default().borders(Borders::ALL).title("继续同步")),
                chunks[0],
            );

            let items = rows
                .iter()
                .map(|row| match row {
                    ResumeConfirmRow::Section(title) => ListItem::new(Line::from(Span::styled(
                        title.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))),
                    ResumeConfirmRow::Repository {
                        name,
                        detail,
                        style,
                    } => ListItem::new(Line::from(vec![
                        Span::raw(format!("  {name}  ")),
                        Span::styled(detail.clone(), *style),
                    ])),
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("所选知识库进度"),
                )
                .highlight_style(Style::default().bg(Color::DarkGray))
                .highlight_symbol("▶ ");
            frame.render_stateful_widget(list, chunks[1], &mut list_state);

            frame.render_widget(
                Paragraph::new("Enter 继续断点同步    ↑/↓ 查看    q/Esc 退出")
                    .block(Block::default().borders(Borders::ALL).title("操作")),
                chunks[2],
            );
        })?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let current = list_state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    list_state.select(Some(current.saturating_sub(1)))
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    list_state.select(Some((current + 1).min(rows.len().saturating_sub(1))))
                }
                KeyCode::Enter => return Ok(true),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ResumeConfirmRow {
    Section(String),
    Repository {
        name: String,
        detail: String,
        style: Style,
    },
}

fn build_resume_confirmation_rows(
    teams: &[Team],
    progress: &HashMap<String, RepositoryProgressSummary>,
) -> Vec<ResumeConfirmRow> {
    let mut completed = Vec::new();
    let mut pending = Vec::new();
    let mut not_started = Vec::new();

    for repo in teams
        .iter()
        .flat_map(|team| &team.repositories)
        .filter(|repo| repo.selected)
    {
        let summary = progress.get(&repo.id).copied();
        let row = ResumeConfirmRow::Repository {
            name: repo.name.clone(),
            detail: repository_progress_label(repo, progress),
            style: repository_progress_style(summary.as_ref()),
        };
        match summary {
            Some(item) if item.is_complete() => completed.push(row),
            Some(item) if item.is_started() => pending.push(row),
            _ => not_started.push(row),
        }
    }

    let mut rows = Vec::new();
    append_resume_section(&mut rows, "已完成", completed);
    append_resume_section(&mut rows, "待继续", pending);
    append_resume_section(&mut rows, "未开始", not_started);
    rows
}

fn append_resume_section(
    rows: &mut Vec<ResumeConfirmRow>,
    title: &str,
    mut items: Vec<ResumeConfirmRow>,
) {
    if items.is_empty() {
        return;
    }
    rows.push(ResumeConfirmRow::Section(title.to_string()));
    rows.append(&mut items);
}

fn repository_progress_label(
    repo: &Repository,
    progress: &HashMap<String, RepositoryProgressSummary>,
) -> String {
    match progress.get(&repo.id).copied() {
        Some(summary) if summary.is_complete() => format!(
            "{}/{} 已完成",
            summary.completed_documents, summary.total_documents
        ),
        Some(summary) if summary.is_started() => {
            let mut parts = vec![format!(
                "{}/{} 待继续",
                summary.completed_documents, summary.total_documents
            )];
            parts.push(format!("待处理 {}", summary.pending_documents()));
            if summary.failed_documents > 0 {
                parts.push(format!("失败 {}", summary.failed_documents));
            }
            if summary.in_progress_documents > 0 {
                parts.push(format!("中断 {}", summary.in_progress_documents));
            }
            parts.join("，")
        }
        _ => repo
            .items_count
            .map(|count| format!("未开始，约 {count} 项"))
            .unwrap_or_else(|| "未开始".to_string()),
    }
}

fn repository_progress_style(progress: Option<&RepositoryProgressSummary>) -> Style {
    match progress.copied() {
        Some(summary) if summary.is_complete() => Style::default().fg(Color::Green),
        Some(summary) if summary.needs_resume() => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn build_rows(teams: &[Team]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (team_index, team) in teams.iter().enumerate() {
        rows.push(Row::Team(team_index));
        for repo_index in 0..team.repositories.len() {
            rows.push(Row::Repository(team_index, repo_index));
        }
    }
    rows
}

fn toggle_row(row: &Row, teams: &mut [Team]) {
    match *row {
        Row::Team(index) => {
            let selected = teams[index].repositories.iter().all(|r| r.selected);
            teams[index]
                .repositories
                .iter_mut()
                .for_each(|r| r.selected = !selected);
        }
        Row::Repository(team, repo) => {
            teams[team].repositories[repo].selected = !teams[team].repositories[repo].selected
        }
    }
}

pub fn show_sync_progress(receiver: Receiver<SyncEvent>) -> Result<bool> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_progress(&mut terminal, receiver);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run_progress(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    receiver: Receiver<SyncEvent>,
) -> Result<bool> {
    let mut ui = ProgressUiState::default();
    let mut messages = VecDeque::new();
    let mut repositories = BTreeMap::new();
    loop {
        loop {
            match receiver.try_recv() {
                Ok(event) => match event {
                    SyncEvent::Progress(update) => {
                        push_message(
                            &mut messages,
                            format!(
                                "{} / {} / {} — {}",
                                update.team,
                                update.repository,
                                display_document(&update),
                                update.message
                            ),
                        );
                        repositories.insert(
                            format!("{} / {}", update.team, update.repository),
                            RepositoryRow::from_progress(&update),
                        );
                        ui.current = Some(update);
                    }
                    SyncEvent::RateLimit(limit) => {
                        push_message(
                            &mut messages,
                            format!(
                                "{}：{}，等待约 {} 秒",
                                limit.bucket,
                                limit.reason.label(),
                                limit.wait_seconds
                            ),
                        );
                        ui.rate_limit = Some(limit);
                    }
                    SyncEvent::Warning { message } => {
                        push_message(&mut messages, format!("警告：{message}"));
                    }
                    SyncEvent::Finished { summary } => {
                        push_message(&mut messages, "同步完成".to_string());
                        ui.summary = Some(summary);
                    }
                    SyncEvent::Failed { message } => {
                        push_message(&mut messages, format!("同步失败：{message}"));
                        ui.failed = Some(message);
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if ui.summary.is_none() && ui.failed.is_none() && !ui.disconnected {
                        push_message(
                            &mut messages,
                            "同步任务已结束，但未收到完成事件".to_string(),
                        );
                        ui.disconnected = true;
                    }
                    break;
                }
            }
        }

        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(5),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(frame.area());

            frame.render_widget(
                Paragraph::new(rate_limit_line(ui.rate_limit.as_ref()))
                    .block(Block::default().borders(Borders::ALL).title("连接 / 限流")),
                chunks[0],
            );

            let (overall_ratio, overall_label) = overall_progress_label(&ui);
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL).title("整体进度"))
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .ratio(overall_ratio)
                    .label(overall_label),
                chunks[1],
            );

            let current_lines = current_task_lines(&ui);
            frame.render_widget(
                Paragraph::new(current_lines)
                    .block(Block::default().borders(Borders::ALL).title("当前任务")),
                chunks[2],
            );

            let repo_lines = repository_lines(&repositories, &messages);
            frame.render_widget(
                Paragraph::new(repo_lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("知识库 / 最近事件"),
                ),
                chunks[3],
            );

            let status = if let Some(summary) = ui.summary.as_ref() {
                format!(
                    "同步完成：知识库 {}/{}，文档 {}/{}，跳过 {}，下载 {}，失败 {}。Enter/q 返回",
                    summary.repository_completed,
                    summary.repository_total,
                    summary.document_completed,
                    summary.document_total,
                    summary.document_skipped,
                    summary.document_downloaded,
                    summary.document_failed
                )
            } else if ui.failed.is_some() {
                "同步失败，Enter/q 返回查看错误".into()
            } else if ui.disconnected {
                "同步任务已结束但未收到完成事件，Enter/q 返回".into()
            } else {
                "同步过程中可安全中断；下次将从 SQLite 状态继续。Esc/q 取消本次任务".into()
            };
            frame.render_widget(
                Paragraph::new(status).block(Block::default().borders(Borders::ALL).title("状态")),
                chunks[4],
            );
        })?;

        let terminal_state = ui.summary.is_some() || ui.failed.is_some() || ui.disconnected;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if terminal_state {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
                    return Ok(true);
                }
            } else if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                return Ok(false);
            }
        }
    }
}

#[derive(Debug, Default)]
struct ProgressUiState {
    current: Option<SyncProgress>,
    rate_limit: Option<RateLimitProgress>,
    summary: Option<SyncSummary>,
    failed: Option<String>,
    disconnected: bool,
}

#[derive(Debug, Clone)]
struct RepositoryRow {
    index: usize,
    total_repositories: usize,
    completed: usize,
    total: usize,
    failed: usize,
    skipped: usize,
    downloaded: usize,
    stage: String,
}

impl RepositoryRow {
    fn from_progress(progress: &SyncProgress) -> Self {
        Self {
            index: progress.repository_index,
            total_repositories: progress.repository_total,
            completed: progress.completed,
            total: progress.total,
            failed: progress.failed,
            skipped: progress.skipped,
            downloaded: progress.downloaded,
            stage: progress.stage.clone(),
        }
    }
}

fn push_message(messages: &mut VecDeque<String>, message: String) {
    messages.push_back(message);
    while messages.len() > 8 {
        messages.pop_front();
    }
}

fn display_document(progress: &SyncProgress) -> &str {
    if progress.document.is_empty() {
        "-"
    } else {
        &progress.document
    }
}

fn rate_limit_line(rate_limit: Option<&RateLimitProgress>) -> Line<'static> {
    let Some(limit) = rate_limit else {
        return Line::from(vec![
            Span::styled("状态: 正常  ", Style::default().fg(Color::Green)),
            Span::raw("未检测到限流等待"),
        ]);
    };
    let remaining = ((limit.wait_until_ms - now_epoch_ms()).max(0) + 999) / 1000;
    if remaining == 0 {
        return Line::from(vec![
            Span::styled("状态: 正常  ", Style::default().fg(Color::Green)),
            Span::raw("上一次限流等待已结束"),
        ]);
    }
    let quota = match (limit.used, limit.usable) {
        (Some(used), Some(usable)) => format!(" 额度 {used}/{usable}"),
        _ => String::new(),
    };
    Line::from(vec![
        Span::styled("等待中  ", Style::default().fg(Color::Yellow)),
        Span::raw(format!(
            "{} {}{}，剩余约 {} 秒",
            limit.bucket,
            limit.reason.label(),
            quota,
            remaining
        )),
    ])
}

fn overall_progress_label(ui: &ProgressUiState) -> (f64, String) {
    if let Some(summary) = ui.summary.as_ref() {
        let ratio = if summary.document_total == 0 {
            1.0
        } else {
            summary.document_completed as f64 / summary.document_total as f64
        };
        return (
            ratio.clamp(0.0, 1.0),
            format!(
                "知识库 {}/{}，文档 {}/{}，跳过 {}，下载 {}，失败 {}",
                summary.repository_completed,
                summary.repository_total,
                summary.document_completed,
                summary.document_total,
                summary.document_skipped,
                summary.document_downloaded,
                summary.document_failed
            ),
        );
    }
    if let Some(progress) = ui.current.as_ref() {
        let ratio = if progress.overall_total == 0 {
            0.0
        } else {
            progress.overall_completed as f64 / progress.overall_total as f64
        };
        return (
            ratio.clamp(0.0, 1.0),
            format!(
                "知识库 {}/{}，文档 {}/{}，跳过 {}，下载 {}，失败 {}",
                progress.repository_completed,
                progress.repository_total,
                progress.overall_completed,
                progress.overall_total,
                progress.overall_skipped,
                progress.overall_downloaded,
                progress.overall_failed
            ),
        );
    }
    (0.0, "准备同步".into())
}

fn current_task_lines(ui: &ProgressUiState) -> Vec<Line<'static>> {
    let Some(progress) = ui.current.as_ref() else {
        return vec![Line::raw("准备同步")];
    };
    vec![
        Line::raw(format!("团队: {}", progress.team)),
        Line::raw(format!("知识库: {}", progress.repository)),
        Line::raw(format!("文档: {}", display_document(progress))),
        Line::raw(format!("阶段: {} — {}", progress.stage, progress.message)),
    ]
}

fn repository_lines(
    repositories: &BTreeMap<String, RepositoryRow>,
    messages: &VecDeque<String>,
) -> Vec<Line<'static>> {
    if repositories.is_empty() {
        return messages
            .iter()
            .map(|line| Line::raw(line.clone()))
            .collect();
    }
    let mut lines: Vec<_> = repositories
        .iter()
        .map(|(name, row)| {
            let marker = if row.total > 0 && row.completed >= row.total && row.failed == 0 {
                "✓"
            } else if row.failed > 0 {
                "!"
            } else {
                "▶"
            };
            Line::raw(format!(
                "{} [{}/{}] {}：{}/{}，跳过 {}，下载 {}，失败 {}，{}",
                marker,
                row.index,
                row.total_repositories,
                name,
                row.completed,
                row.total,
                row.skipped,
                row.downloaded,
                row.failed,
                row.stage
            ))
        })
        .collect();
    lines.push(Line::raw(""));
    lines.extend(messages.iter().map(|line| Line::raw(line.clone())));
    lines
}
