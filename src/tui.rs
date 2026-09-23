use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::{
    github::{RunCounts, RunStatus, Workflow, WorkflowSource},
    query::{FilterExpression, SortSpec, select_workflows},
    repository::Repository,
    state::ViewState,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_FLASH_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    Command,
}

struct PendingLoad {
    repository: Repository,
    workflow_ids: Option<Vec<u64>>,
    state_path: Option<PathBuf>,
    kind: LoadKind,
    filter: Option<String>,
    sort: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadKind {
    Initial,
    Edit,
    Refresh,
}

struct App {
    repository: Repository,
    workflows: Vec<Workflow>,
    state_path: Option<PathBuf>,
    workflow_source: Arc<dyn WorkflowSource>,
    load_receiver: Option<Receiver<Result<Vec<Workflow>, crate::github::Error>>>,
    pending_load: Option<PendingLoad>,
    table_state: TableState,
    mode: Mode,
    command: String,
    command_cursor: usize,
    command_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    message: Option<String>,
    pending_g: bool,
    should_quit: bool,
    animation_started: Instant,
    refresh_interval: Duration,
    next_refresh: Instant,
    filter: Option<String>,
    filter_expression: Option<FilterExpression>,
    sort: Option<String>,
    sort_spec: Option<SortSpec>,
}

impl App {
    fn new(
        repository: Repository,
        workflows: Vec<Workflow>,
        state_path: Option<PathBuf>,
        workflow_source: Arc<dyn WorkflowSource>,
        filter: Option<String>,
        sort: Option<String>,
    ) -> Self {
        let selected = (!workflows.is_empty()).then_some(0);
        Self {
            repository,
            workflows,
            state_path,
            workflow_source,
            load_receiver: None,
            pending_load: None,
            table_state: TableState::default().with_selected(selected),
            mode: Mode::Normal,
            command: String::new(),
            command_cursor: 0,
            command_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            message: None,
            pending_g: false,
            should_quit: false,
            animation_started: Instant::now(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            next_refresh: Instant::now() + DEFAULT_REFRESH_INTERVAL,
            filter_expression: filter
                .as_deref()
                .and_then(|expression| FilterExpression::parse(expression).ok()),
            sort_spec: sort
                .as_deref()
                .and_then(|specification| SortSpec::parse(specification).ok()),
            filter,
            sort,
        }
    }

    fn new_loading(
        repository: Repository,
        workflow_ids: Option<Vec<u64>>,
        state_path: Option<PathBuf>,
        workflow_source: Arc<dyn WorkflowSource>,
        filter: Option<String>,
        sort: Option<String>,
    ) -> Self {
        let mut app = Self::new(
            repository.clone(),
            Vec::new(),
            None,
            workflow_source,
            filter.clone(),
            sort.clone(),
        );
        app.start_loading(PendingLoad {
            repository,
            workflow_ids,
            state_path,
            kind: LoadKind::Initial,
            filter,
            sort,
        });
        app
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
            self.poll_loading();
            self.refresh_if_due();
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(EVENT_POLL_INTERVAL)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key);
            }
        }

        Ok(())
    }

    fn start_loading(&mut self, pending: PendingLoad) {
        let source = Arc::clone(&self.workflow_source);
        let repository = pending.repository.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(source.list_workflows(&repository));
        });
        self.load_receiver = Some(receiver);
        self.pending_load = Some(pending);
        self.message = None;
    }

    fn poll_loading(&mut self) {
        let Some(receiver) = self.load_receiver.as_ref() else {
            return;
        };

        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.finish_loading(Err("workflow loading stopped unexpectedly".to_owned()));
                return;
            }
        };
        self.finish_loading(result.map_err(|error| error.to_string()));
    }

    fn finish_loading(&mut self, result: Result<Vec<Workflow>, String>) {
        self.load_receiver = None;
        let Some(pending) = self.pending_load.take() else {
            return;
        };

        match result {
            Ok(workflows) => {
                let selected_id = self
                    .table_state
                    .selected()
                    .and_then(|index| self.visible_workflows().get(index).copied())
                    .map(|workflow| workflow.id);
                self.repository = pending.repository;
                self.workflows = match pending.workflow_ids {
                    Some(workflow_ids) => ViewState::resolve_workflows(&workflow_ids, workflows),
                    None => workflows,
                };
                self.state_path = pending.state_path;
                if pending.kind != LoadKind::Refresh {
                    self.filter_expression = pending
                        .filter
                        .as_deref()
                        .and_then(|expression| FilterExpression::parse(expression).ok());
                    self.sort_spec = pending
                        .sort
                        .as_deref()
                        .and_then(|specification| SortSpec::parse(specification).ok());
                    self.filter = pending.filter;
                    self.sort = pending.sort;
                }
                let visible = self.visible_workflows();
                let selected = selected_id
                    .and_then(|id| visible.iter().position(|workflow| workflow.id == id))
                    .or_else(|| (!visible.is_empty()).then_some(0));
                self.table_state = TableState::default().with_selected(selected);
                self.message = Some(match pending.kind {
                    LoadKind::Refresh => format!("{} workflows refreshed", self.workflows.len()),
                    LoadKind::Initial | LoadKind::Edit => {
                        format!("{} workflows loaded", self.workflows.len())
                    }
                });
            }
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
        self.next_refresh = Instant::now() + self.refresh_interval;
    }

    fn is_loading(&self) -> bool {
        self.pending_load.is_some()
    }

    fn visible_workflows(&self) -> Vec<&Workflow> {
        select_workflows(
            &self.workflows,
            self.filter_expression.as_ref(),
            self.sort_spec.as_ref(),
        )
    }

    fn refresh_if_due(&mut self) {
        if !self.is_loading() && Instant::now() >= self.next_refresh {
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        if self.is_loading() {
            self.message = Some("Refresh already in progress".to_owned());
            return;
        }

        self.start_loading(PendingLoad {
            repository: self.repository.clone(),
            workflow_ids: Some(self.workflows.iter().map(|workflow| workflow.id).collect()),
            state_path: self.state_path.clone(),
            kind: LoadKind::Refresh,
            filter: self.filter.clone(),
            sort: self.sort.clone(),
        });
    }

    fn refresh_rate_label(&self) -> String {
        format!("refresh: {}s", self.refresh_interval.as_secs())
    }

    fn normal_status(&self) -> (&'static str, String) {
        if let Some(pending) = self.pending_load.as_ref() {
            let action = match pending.kind {
                LoadKind::Refresh => "Refreshing",
                LoadKind::Initial | LoadKind::Edit => "Loading",
            };
            (
                if pending.kind == LoadKind::Refresh {
                    " REFRESHING "
                } else {
                    " LOADING "
                },
                format!(
                    "{action} workflows and run status for {}...  |  {}",
                    pending.repository,
                    self.refresh_rate_label()
                ),
            )
        } else {
            (
                " NORMAL ",
                self.message
                    .as_deref()
                    .unwrap_or("j/k: select  |  Ctrl-d/Ctrl-u: scroll  |  gg/G: jump  |  :q: quit")
                    .to_owned()
                    + "  |  "
                    + &self.refresh_rate_label(),
            )
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Command => self.handle_command_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.select_next(1),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(1),
            KeyCode::PageDown => self.select_next(10),
            KeyCode::PageUp => self.select_previous(10),
            KeyCode::Char('d') if control => self.select_next(10),
            KeyCode::Char('u') if control => self.select_previous(10),
            KeyCode::Home => self.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_last(),
            KeyCode::Char('g') if self.pending_g => self.select_first(),
            KeyCode::Char('g') => {
                self.pending_g = true;
                return;
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.command_cursor = 0;
                self.history_index = None;
                self.history_draft.clear();
                self.message = None;
            }
            _ => {}
        }

        self.pending_g = false;
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
                self.command_cursor = 0;
                self.history_index = None;
                self.history_draft.clear();
            }
            KeyCode::Enter => self.execute_command(),
            KeyCode::Left => {
                self.command_cursor = self.command_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.command_cursor = (self.command_cursor + 1).min(self.command.chars().count());
            }
            KeyCode::Home => self.command_cursor = 0,
            KeyCode::End => self.command_cursor = self.command.chars().count(),
            KeyCode::Up => self.previous_command(),
            KeyCode::Down => self.next_command(),
            KeyCode::Backspace => self.backspace_command(),
            KeyCode::Delete => self.delete_command_character(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let byte_index = char_to_byte_index(&self.command, self.command_cursor);
                self.command.insert(byte_index, character);
                self.command_cursor += 1;
            }
            _ => {}
        }
    }

    fn execute_command(&mut self) {
        let command = self.command.trim().to_owned();
        if !command.is_empty() {
            self.command_history.push(command.clone());
        }

        if let Some(count) = parse_delete_command(&command) {
            match count {
                Ok(count) => self.delete_rows(count),
                Err(message) => self.message = Some(message),
            }
            self.finish_command();
            return;
        }

        let (name, argument) = command
            .split_once(char::is_whitespace)
            .map_or((command.as_str(), None), |(name, argument)| {
                (name, Some(argument.trim()))
            });
        match name {
            "q" if argument.is_none() => {
                self.should_quit = true;
                return;
            }
            "w" => {
                self.write_state(argument);
            }
            "wq" => {
                if self.write_state(argument) {
                    self.should_quit = true;
                    return;
                }
            }
            "e" => self.edit_state(argument),
            "refresh" if argument.is_none() => self.refresh(),
            "refresh-rate" => self.set_refresh_rate(argument),
            "filter" => self.set_filter(argument),
            "sort" => self.set_sort(argument),
            "" => {}
            _ => self.message = Some(format!("E492: Not an editor command: {command}")),
        }

        self.finish_command();
    }

    fn finish_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
        self.mode = Mode::Normal;
    }

    fn delete_rows(&mut self, count: usize) {
        let Some(selected) = self.table_state.selected() else {
            self.message = Some("E749: Empty buffer".to_owned());
            return;
        };

        let deleted_ids = self
            .visible_workflows()
            .into_iter()
            .skip(selected)
            .take(count)
            .map(|workflow| workflow.id)
            .collect::<Vec<_>>();
        let deleted = deleted_ids.len();
        self.workflows
            .retain(|workflow| !deleted_ids.contains(&workflow.id));

        let visible_count = self.visible_workflows().len();
        let next_selection = if visible_count == 0 {
            None
        } else {
            Some(selected.min(visible_count - 1))
        };
        self.table_state.select(next_selection);
        self.message = Some(if deleted == 1 {
            "1 workflow deleted".to_owned()
        } else {
            format!("{deleted} workflows deleted")
        });
    }

    fn write_state(&mut self, argument: Option<&str>) -> bool {
        let path = match command_path(argument, self.state_path.as_ref()) {
            Ok(path) => path,
            Err(message) => {
                self.message = Some(message);
                return false;
            }
        };

        let state = ViewState::new(
            self.repository.clone(),
            &self.workflows,
            self.filter.clone(),
            self.sort.clone(),
        );
        match state.save(&path) {
            Ok(()) => {
                self.message = Some(format!(
                    "\"{}\" {} workflows written",
                    path.display(),
                    self.workflows.len()
                ));
                self.state_path = Some(path);
                true
            }
            Err(error) => {
                self.message = Some(format!("E212: {error}"));
                false
            }
        }
    }

    fn edit_state(&mut self, argument: Option<&str>) {
        let path = match command_path(argument, self.state_path.as_ref()) {
            Ok(path) => path,
            Err(message) => {
                self.message = Some(message);
                return;
            }
        };

        match ViewState::load(&path) {
            Ok(state) => self.start_loading(PendingLoad {
                repository: state.repository,
                workflow_ids: Some(state.workflow_ids),
                state_path: Some(path),
                kind: LoadKind::Edit,
                filter: state.filter,
                sort: state.sort,
            }),
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
    }

    fn set_refresh_rate(&mut self, argument: Option<&str>) {
        let Some(argument) = argument.filter(|argument| !argument.is_empty()) else {
            self.message = Some("E471: Argument required".to_owned());
            return;
        };
        let Ok(seconds) = argument.parse::<u64>() else {
            self.message = Some(format!("E474: Invalid argument: {argument}"));
            return;
        };
        if seconds == 0 {
            self.message = Some("E474: Refresh rate must be greater than zero".to_owned());
            return;
        }

        self.refresh_interval = Duration::from_secs(seconds);
        self.next_refresh = Instant::now() + self.refresh_interval;
        self.message = Some(format!("Refresh rate set to {seconds} seconds"));
    }

    fn set_filter(&mut self, argument: Option<&str>) {
        let Some(expression) = argument.filter(|argument| !argument.is_empty()) else {
            self.filter = None;
            self.filter_expression = None;
            self.reset_visible_selection();
            self.message = Some("Filter cleared".to_owned());
            return;
        };

        match FilterExpression::parse(expression) {
            Ok(filter) => {
                self.filter = Some(expression.to_owned());
                self.filter_expression = Some(filter);
                self.reset_visible_selection();
                self.message = Some(format!("Filter: {expression}"));
            }
            Err(error) => self.message = Some(format!("E474: {error}")),
        }
    }

    fn set_sort(&mut self, argument: Option<&str>) {
        let Some(specification) = argument.filter(|argument| !argument.is_empty()) else {
            self.sort = None;
            self.sort_spec = None;
            self.reset_visible_selection();
            self.message = Some("Sort cleared".to_owned());
            return;
        };

        match SortSpec::parse(specification) {
            Ok(sort) => {
                self.sort = Some(specification.to_owned());
                self.sort_spec = Some(sort);
                self.reset_visible_selection();
                self.message = Some(format!("Sort: {specification}"));
            }
            Err(error) => self.message = Some(format!("E474: {error}")),
        }
    }

    fn reset_visible_selection(&mut self) {
        self.table_state
            .select((!self.visible_workflows().is_empty()).then_some(0));
    }

    fn backspace_command(&mut self) {
        if self.command_cursor == 0 {
            return;
        }

        let start = char_to_byte_index(&self.command, self.command_cursor - 1);
        let end = char_to_byte_index(&self.command, self.command_cursor);
        self.command.replace_range(start..end, "");
        self.command_cursor -= 1;
    }

    fn delete_command_character(&mut self) {
        if self.command_cursor == self.command.chars().count() {
            return;
        }

        let start = char_to_byte_index(&self.command, self.command_cursor);
        let end = char_to_byte_index(&self.command, self.command_cursor + 1);
        self.command.replace_range(start..end, "");
    }

    fn previous_command(&mut self) {
        if self.command_history.is_empty() {
            return;
        }

        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft.clone_from(&self.command);
                self.command_history.len() - 1
            }
        };
        self.load_history(index);
    }

    fn next_command(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.command_history.len() {
            self.load_history(index + 1);
        } else {
            self.command.clone_from(&self.history_draft);
            self.command_cursor = self.command.chars().count();
            self.history_index = None;
        }
    }

    fn load_history(&mut self, index: usize) {
        self.command.clone_from(&self.command_history[index]);
        self.command_cursor = self.command.chars().count();
        self.history_index = Some(index);
    }

    fn select_next(&mut self, amount: usize) {
        let visible_count = self.visible_workflows().len();
        if visible_count == 0 {
            return;
        }

        let current = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some((current + amount).min(visible_count - 1)));
    }

    fn select_previous(&mut self, amount: usize) {
        if self.visible_workflows().is_empty() {
            return;
        }

        let current = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(current.saturating_sub(amount)));
    }

    fn select_first(&mut self) {
        if !self.visible_workflows().is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let visible_count = self.visible_workflows().len();
        if visible_count > 0 {
            self.table_state.select(Some(visible_count - 1));
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let [table_area, help_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());
        let visible_workflows = self
            .visible_workflows()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        if visible_workflows.is_empty() {
            let empty_message = if self.is_loading() {
                "Loading GitHub Actions workflows..."
            } else if self.filter.is_some() {
                "No workflows match the active filter."
            } else {
                "This repository has no active GitHub Actions workflows."
            };
            let empty = Paragraph::new(empty_message).centered().block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", self.repository)),
            );
            frame.render_widget(empty, table_area);
        } else {
            let header = Row::new(["Status", "Name", "24 Hours", "7 Days", "14 Days"])
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .bottom_margin(1);
            let flash_visible = flash_visible(self.animation_started.elapsed());
            let rows = visible_workflows.iter().map(|workflow| {
                Row::new([
                    Cell::from(status_indicator(workflow, flash_visible)),
                    Cell::from(workflow.name.as_str()),
                    metrics_cell(workflow.run_metrics.last_24_hours),
                    metrics_cell(workflow.run_metrics.last_7_days),
                    metrics_cell(workflow.run_metrics.last_14_days),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(8),
                    Constraint::Min(20),
                    Constraint::Length(19),
                    Constraint::Length(19),
                    Constraint::Length(19),
                ],
            )
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ")
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} - {} active workflows ",
                self.repository,
                visible_workflows.len()
            )));

            frame.render_stateful_widget(table, table_area, &mut self.table_state);
        }

        match self.mode {
            Mode::Normal => {
                let (title, text) = self.normal_status();
                let help = Paragraph::new(text)
                    .dark_gray()
                    .block(Block::default().borders(Borders::ALL).title(title));
                frame.render_widget(help, help_area);
            }
            Mode::Command => {
                let command = Paragraph::new(format!(":{}", self.command)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" COMMAND | {} ", self.refresh_rate_label())),
                );
                frame.render_widget(command, help_area);

                let cursor_x = help_area
                    .x
                    .saturating_add(2)
                    .saturating_add(self.command_cursor as u16)
                    .min(help_area.right().saturating_sub(2));
                frame.set_cursor_position((cursor_x, help_area.y.saturating_add(1)));
            }
        }
    }
}

fn command_path(argument: Option<&str>, remembered: Option<&PathBuf>) -> Result<PathBuf, String> {
    match argument.filter(|argument| !argument.is_empty()) {
        Some(argument) => Ok(PathBuf::from(argument)),
        None => remembered
            .cloned()
            .ok_or_else(|| "E32: No file name".to_owned()),
    }
}

fn parse_delete_command(command: &str) -> Option<Result<usize, String>> {
    let count = command.strip_prefix('d')?;
    if count.is_empty() {
        return Some(Ok(1));
    }
    if !count.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }

    Some(
        count
            .parse::<usize>()
            .map_err(|_| format!("E488: Trailing characters: {count}"))
            .and_then(|count| {
                (count > 0)
                    .then_some(count)
                    .ok_or_else(|| "E16: Invalid range".to_owned())
            }),
    )
}

fn status_indicator(workflow: &Workflow, flash_visible: bool) -> &'static str {
    if workflow.is_in_progress && !flash_visible {
        return "";
    }

    match workflow.run_status {
        RunStatus::Success => "🟢",
        RunStatus::Failure => "🔴",
        RunStatus::Other => "⚪",
    }
}

fn metrics_cell(counts: RunCounts) -> Cell<'static> {
    let percentage = counts.pass_percentage();
    Cell::from(format!(
        "{}/{}/{} {:.1}%",
        counts.passed, counts.failed, counts.total, percentage
    ))
    .style(Style::default().fg(metrics_color(percentage)))
}

fn metrics_color(percentage: f64) -> Color {
    if percentage < 70.0 {
        Color::Red
    } else if percentage < 90.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn flash_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / STATUS_FLASH_INTERVAL.as_millis()).is_multiple_of(2)
}

fn char_to_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

pub fn run(
    repository: Repository,
    workflow_ids: Option<Vec<u64>>,
    filter: Option<String>,
    sort: Option<String>,
    state_path: Option<PathBuf>,
    workflow_source: Arc<dyn WorkflowSource>,
) -> io::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    let result = App::new_loading(
        repository,
        workflow_ids,
        state_path,
        workflow_source,
        filter,
        sort,
    )
    .run(&mut terminal);
    ratatui::restore();
    result
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        original_hook(panic_info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestWorkflowSource;

    impl WorkflowSource for TestWorkflowSource {
        fn list_workflows(
            &self,
            _repository: &Repository,
        ) -> Result<Vec<Workflow>, crate::github::Error> {
            Ok((0..100)
                .map(|id| {
                    let mut workflow = workflow(id);
                    workflow.name = format!("Refreshed Workflow {id}");
                    workflow.run_status = RunStatus::Success;
                    workflow
                })
                .collect())
        }
    }

    fn workflow(id: u64) -> Workflow {
        Workflow {
            id,
            name: format!("Workflow {id}"),
            path: format!(".github/workflows/{id}.yml"),
            state: "active".to_owned(),
            run_status: RunStatus::Other,
            is_in_progress: false,
            run_metrics: crate::github::RunMetrics::default(),
        }
    }

    fn app(workflow_count: u64) -> App {
        let repository = "owner/repository".parse().unwrap();
        let workflows = (0..workflow_count).map(workflow).collect();
        App::new(
            repository,
            workflows,
            None,
            Arc::new(TestWorkflowSource),
            None,
            None,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn enter_command(app: &mut App, command: &str) {
        app.handle_key(key(KeyCode::Char(':')));
        for character in command.chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
    }

    fn complete_loading(app: &mut App) {
        let result = app
            .load_receiver
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .map_err(|error| error.to_string());
        app.finish_loading(result);
    }

    #[test]
    fn new_selects_first_workflow() {
        assert_eq!(app(2).table_state.selected(), Some(0));
    }

    #[test]
    fn new_leaves_selection_empty_without_workflows() {
        assert_eq!(app(0).table_state.selected(), None);
    }

    #[test]
    fn new_loading_returns_before_workflows_are_available() {
        let mut app = App::new_loading(
            "owner/repository".parse().unwrap(),
            None,
            None,
            Arc::new(TestWorkflowSource),
            None,
            None,
        );

        assert!(app.is_loading());
        assert!(app.workflows.is_empty());
        assert_eq!(
            app.normal_status(),
            (
                " LOADING ",
                "Loading workflows and run status for owner/repository...  |  refresh: 15s"
                    .to_owned()
            )
        );

        complete_loading(&mut app);

        assert!(!app.is_loading());
        assert_eq!(app.workflows.len(), 100);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn loading_saved_view_filters_current_data_in_saved_order() {
        let mut app = App::new_loading(
            "owner/repository".parse().unwrap(),
            Some(vec![3, 1]),
            Some(PathBuf::from("saved-view.json")),
            Arc::new(TestWorkflowSource),
            None,
            None,
        );

        complete_loading(&mut app);

        let ids: Vec<_> = app.workflows.iter().map(|workflow| workflow.id).collect();
        assert_eq!(ids, vec![3, 1]);
        assert_eq!(app.state_path, Some(PathBuf::from("saved-view.json")));
    }

    #[test]
    fn refresh_command_updates_current_workflows_in_background() {
        let mut app = app(2);
        app.workflows[0].name = "Stale Workflow".to_owned();
        app.select_next(1);

        enter_command(&mut app, "refresh");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.is_loading());
        assert_eq!(app.pending_load.as_ref().unwrap().kind, LoadKind::Refresh);
        assert_eq!(app.normal_status().0, " REFRESHING ");
        complete_loading(&mut app);

        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
        assert_eq!(app.table_state.selected(), Some(1));
        assert_eq!(app.message.as_deref(), Some("2 workflows refreshed"));
    }

    #[test]
    fn refresh_starts_automatically_when_interval_elapses() {
        let mut app = app(2);
        app.next_refresh = Instant::now() - Duration::from_millis(1);

        app.refresh_if_due();

        assert!(app.is_loading());
        assert_eq!(app.pending_load.as_ref().unwrap().kind, LoadKind::Refresh);
        complete_loading(&mut app);
    }

    #[test]
    fn refresh_rate_command_changes_interval_and_footer() {
        let mut app = app(1);

        enter_command(&mut app, "refresh-rate 30");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.refresh_interval, Duration::from_secs(30));
        assert_eq!(
            app.message.as_deref(),
            Some("Refresh rate set to 30 seconds")
        );
        assert!(app.normal_status().1.ends_with("refresh: 30s"));
    }

    #[test]
    fn refresh_rate_command_rejects_missing_invalid_and_zero_values() {
        let mut app = app(1);

        enter_command(&mut app, "refresh-rate");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.message.as_deref(), Some("E471: Argument required"));

        enter_command(&mut app, "refresh-rate fast");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.message.as_deref(), Some("E474: Invalid argument: fast"));

        enter_command(&mut app, "refresh-rate 0");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.message.as_deref(),
            Some("E474: Refresh rate must be greater than zero")
        );
        assert_eq!(app.refresh_interval, DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn handle_key_moves_selection_and_clamps_to_bounds() {
        let mut app = app(20);

        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.table_state.selected(), Some(19));

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.table_state.selected(), Some(19));

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.table_state.selected(), Some(9));

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn handle_key_supports_gg_to_select_first_workflow() {
        let mut app = app(3);
        app.select_last();

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));

        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn handle_key_does_not_quit_from_normal_mode() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.should_quit);
    }

    #[test]
    fn handle_key_quits_after_q_command() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.should_quit);
    }

    #[test]
    fn escape_cancels_command_mode() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.command.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn unknown_command_displays_error_and_returns_to_normal_mode() {
        let mut app = app(1);

        enter_command(&mut app, "x");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.message.as_deref(),
            Some("E492: Not an editor command: x")
        );
    }

    #[test]
    fn command_cursor_supports_movement_and_mid_line_editing() {
        let mut app = app(1);
        enter_command(&mut app, "ac");

        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.command, "abc");
        assert_eq!(app.command_cursor, 2);

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.command, "ac");
        assert_eq!(app.command_cursor, 1);

        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.command, "a");
        assert_eq!(app.command_cursor, 1);
    }

    #[test]
    fn command_history_scrolls_and_restores_draft() {
        let mut app = app(1);
        for command in ["first", "second"] {
            enter_command(&mut app, command);
            app.handle_key(key(KeyCode::Enter));
        }
        enter_command(&mut app, "draft");

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "second");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "first");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.command, "first");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command, "second");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.command, "draft");
        assert_eq!(app.history_index, None);
        assert_eq!(app.command_cursor, 5);
    }

    #[test]
    fn command_history_ignores_empty_commands() {
        let mut app = app(1);
        enter_command(&mut app, "");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.command_history.is_empty());
    }

    #[test]
    fn filter_command_applies_and_clears_visible_view() {
        let mut app = app(3);

        enter_command(&mut app, "filter name:Workflow*");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 3);
        assert_eq!(app.filter.as_deref(), Some("name:Workflow*"));

        enter_command(&mut app, "filter name:Workflow\\ 1");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 1);
        assert_eq!(app.visible_workflows()[0].id, 1);

        enter_command(&mut app, "filter");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows().len(), 3);
        assert!(app.filter.is_none());
    }

    #[test]
    fn sort_command_applies_direction_and_clears() {
        let mut app = app(3);
        app.workflows[0].run_metrics.last_24_hours = RunCounts {
            passed: 1,
            failed: 9,
            total: 10,
        };
        app.workflows[1].run_metrics.last_24_hours = RunCounts {
            passed: 9,
            failed: 1,
            total: 10,
        };

        enter_command(&mut app, "sort 24h.rate:desc");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows()[0].id, 1);

        enter_command(&mut app, "sort");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_workflows()[0].id, 0);
        assert!(app.sort.is_none());
    }

    #[test]
    fn filter_and_sort_commands_report_parse_errors_without_changing_view() {
        let mut app = app(2);

        enter_command(&mut app, "filter name:\"unterminated");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.filter.is_none());
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("unterminated quote")
        );

        enter_command(&mut app, "sort name:sideways");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.sort.is_none());
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("invalid sort direction")
        );
    }

    #[test]
    fn write_and_edit_commands_remember_path_and_restore_view() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-command-state-{}.json",
            std::process::id()
        ));
        let mut app = app(2);

        enter_command(&mut app, "filter status:success");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "sort name:desc");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, &format!("w {}", path.display()));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.state_path.as_ref(), Some(&path));
        assert!(path.is_file());

        app.workflows.clear();
        enter_command(&mut app, "e");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_loading());
        complete_loading(&mut app);
        std::fs::remove_file(path).unwrap();

        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
        assert_eq!(app.workflows[0].run_status, RunStatus::Success);
        assert_eq!(app.filter.as_deref(), Some("status:success"));
        assert_eq!(app.sort.as_deref(), Some("name:desc"));
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn write_command_without_path_reports_error() {
        let mut app = app(1);

        enter_command(&mut app, "w");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.message.as_deref(), Some("E32: No file name"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn write_quit_command_saves_then_quits() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-write-quit-state-{}.json",
            std::process::id()
        ));
        let mut app = app(2);

        enter_command(&mut app, &format!("wq {}", path.display()));
        app.handle_key(key(KeyCode::Enter));
        let saved = ViewState::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        assert!(app.should_quit);
        assert_eq!(saved.workflow_ids, vec![0, 1]);
    }

    #[test]
    fn write_quit_command_does_not_quit_when_write_fails() {
        let mut app = app(1);

        enter_command(&mut app, "wq");
        app.handle_key(key(KeyCode::Enter));

        assert!(!app.should_quit);
        assert_eq!(app.message.as_deref(), Some("E32: No file name"));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn delete_command_removes_selected_workflow() {
        let mut app = app(3);
        app.select_next(1);

        enter_command(&mut app, "d");
        app.handle_key(key(KeyCode::Enter));

        let ids: Vec<_> = app.workflows.iter().map(|workflow| workflow.id).collect();
        assert_eq!(ids, vec![0, 2]);
        assert_eq!(app.table_state.selected(), Some(1));
        assert_eq!(app.message.as_deref(), Some("1 workflow deleted"));
    }

    #[test]
    fn delete_count_removes_consecutive_workflows_and_clamps_at_end() {
        let mut app = app(5);
        app.select_next(3);

        enter_command(&mut app, "d3");
        app.handle_key(key(KeyCode::Enter));

        let ids: Vec<_> = app.workflows.iter().map(|workflow| workflow.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(app.table_state.selected(), Some(2));
        assert_eq!(app.message.as_deref(), Some("2 workflows deleted"));
    }

    #[test]
    fn delete_count_follows_filtered_and_sorted_visible_order() {
        let mut app = app(4);
        enter_command(&mut app, "filter -name:Workflow\\ 0");
        app.handle_key(key(KeyCode::Enter));
        enter_command(&mut app, "sort name:desc");
        app.handle_key(key(KeyCode::Enter));

        enter_command(&mut app, "d2");
        app.handle_key(key(KeyCode::Enter));

        let ids = app
            .workflows
            .iter()
            .map(|workflow| workflow.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![0, 1]);
        assert_eq!(app.visible_workflows()[0].id, 1);
    }

    #[test]
    fn delete_count_can_empty_view_and_clear_selection() {
        let mut app = app(2);

        enter_command(&mut app, "d2");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.workflows.is_empty());
        assert_eq!(app.table_state.selected(), None);
    }

    #[test]
    fn delete_zero_reports_invalid_range() {
        let mut app = app(2);

        enter_command(&mut app, "d0");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.message.as_deref(), Some("E16: Invalid range"));
    }

    #[test]
    fn delete_from_empty_view_reports_error() {
        let mut app = app(0);

        enter_command(&mut app, "d");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.message.as_deref(), Some("E749: Empty buffer"));
    }

    #[test]
    fn status_indicator_reflects_last_completed_run() {
        let mut workflow = workflow(1);

        assert_eq!(status_indicator(&workflow, true), "⚪");
        workflow.run_status = RunStatus::Success;
        assert_eq!(status_indicator(&workflow, true), "🟢");
        workflow.run_status = RunStatus::Failure;
        assert_eq!(status_indicator(&workflow, true), "🔴");
    }

    #[test]
    fn status_indicator_flashes_while_run_is_in_progress() {
        let mut workflow = workflow(1);
        workflow.run_status = RunStatus::Success;
        workflow.is_in_progress = true;

        assert_eq!(status_indicator(&workflow, true), "🟢");
        assert_eq!(status_indicator(&workflow, false), "");
        assert!(flash_visible(Duration::from_millis(499)));
        assert!(!flash_visible(Duration::from_millis(500)));
        assert!(flash_visible(Duration::from_millis(1000)));
    }

    #[test]
    fn metrics_color_uses_requested_thresholds() {
        assert_eq!(metrics_color(69.9), Color::Red);
        assert_eq!(metrics_color(70.0), Color::Yellow);
        assert_eq!(metrics_color(89.9), Color::Yellow);
        assert_eq!(metrics_color(90.0), Color::Green);
    }
}
