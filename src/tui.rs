use std::time::{Duration, Instant};
use std::{io, path::PathBuf};

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
    github::{RunStatus, Workflow, WorkflowSource},
    repository::Repository,
    state::ViewState,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_FLASH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    Command,
}

struct App<'a> {
    repository: Repository,
    workflows: Vec<Workflow>,
    state_path: Option<PathBuf>,
    workflow_source: &'a dyn WorkflowSource,
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
}

impl<'a> App<'a> {
    fn new(
        repository: Repository,
        workflows: Vec<Workflow>,
        state_path: Option<PathBuf>,
        workflow_source: &'a dyn WorkflowSource,
    ) -> Self {
        let selected = (!workflows.is_empty()).then_some(0);
        Self {
            repository,
            workflows,
            state_path,
            workflow_source,
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
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_quit {
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

        let end = selected.saturating_add(count).min(self.workflows.len());
        let deleted = end - selected;
        self.workflows.drain(selected..end);

        let next_selection = if self.workflows.is_empty() {
            None
        } else {
            Some(selected.min(self.workflows.len() - 1))
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

        let state = ViewState::new(self.repository.clone(), &self.workflows);
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

        let loaded = ViewState::load(&path).and_then(|state| {
            let workflows = self
                .workflow_source
                .list_workflows(&state.repository)
                .map_err(crate::state::Error::Refresh)?;
            let workflows = state.resolve_workflows(workflows);
            Ok((state.repository, workflows))
        });

        match loaded {
            Ok((repository, workflows)) => {
                self.repository = repository;
                self.workflows = workflows;
                self.table_state =
                    TableState::default().with_selected((!self.workflows.is_empty()).then_some(0));
                self.message = Some(format!(
                    "\"{}\" {} workflows loaded",
                    path.display(),
                    self.workflows.len()
                ));
                self.state_path = Some(path);
            }
            Err(error) => self.message = Some(format!("E484: {error}")),
        }
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
        if self.workflows.is_empty() {
            return;
        }

        let current = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some((current + amount).min(self.workflows.len() - 1)));
    }

    fn select_previous(&mut self, amount: usize) {
        if self.workflows.is_empty() {
            return;
        }

        let current = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(current.saturating_sub(amount)));
    }

    fn select_first(&mut self) {
        if !self.workflows.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if !self.workflows.is_empty() {
            self.table_state.select(Some(self.workflows.len() - 1));
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let [table_area, help_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());

        if self.workflows.is_empty() {
            let empty = Paragraph::new("This repository has no active GitHub Actions workflows.")
                .centered()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", self.repository)),
                );
            frame.render_widget(empty, table_area);
        } else {
            let header = Row::new(["Status", "Name"])
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .bottom_margin(1);
            let flash_visible = flash_visible(self.animation_started.elapsed());
            let rows = self.workflows.iter().map(|workflow| {
                Row::new([
                    Cell::from(status_indicator(workflow, flash_visible)),
                    Cell::from(workflow.name.as_str()),
                ])
            });
            let table = Table::new(rows, [Constraint::Length(8), Constraint::Min(20)])
                .header(header)
                .row_highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">> ")
                .block(Block::default().borders(Borders::ALL).title(format!(
                    " {} - {} active workflows ",
                    self.repository,
                    self.workflows.len()
                )));

            frame.render_stateful_widget(table, table_area, &mut self.table_state);
        }

        match self.mode {
            Mode::Normal => {
                let text = self
                    .message
                    .as_deref()
                    .unwrap_or("j/k: select  |  Ctrl-d/Ctrl-u: scroll  |  gg/G: jump  |  :q: quit");
                let help = Paragraph::new(text)
                    .dark_gray()
                    .block(Block::default().borders(Borders::ALL).title(" NORMAL "));
                frame.render_widget(help, help_area);
            }
            Mode::Command => {
                let command = Paragraph::new(format!(":{}", self.command))
                    .block(Block::default().borders(Borders::ALL).title(" COMMAND "));
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
    workflows: Vec<Workflow>,
    state_path: Option<PathBuf>,
    workflow_source: &dyn WorkflowSource,
) -> io::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    let result = App::new(repository, workflows, state_path, workflow_source).run(&mut terminal);
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

    static TEST_WORKFLOW_SOURCE: TestWorkflowSource = TestWorkflowSource;

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
        }
    }

    fn app(workflow_count: u64) -> App<'static> {
        let repository = "owner/repository".parse().unwrap();
        let workflows = (0..workflow_count).map(workflow).collect();
        App::new(repository, workflows, None, &TEST_WORKFLOW_SOURCE)
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

    #[test]
    fn new_selects_first_workflow() {
        assert_eq!(app(2).table_state.selected(), Some(0));
    }

    #[test]
    fn new_leaves_selection_empty_without_workflows() {
        assert_eq!(app(0).table_state.selected(), None);
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
    fn write_and_edit_commands_remember_path_and_restore_view() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-command-state-{}.json",
            std::process::id()
        ));
        let mut app = app(2);

        enter_command(&mut app, &format!("w {}", path.display()));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.state_path.as_ref(), Some(&path));
        assert!(path.is_file());

        app.workflows.clear();
        enter_command(&mut app, "e");
        app.handle_key(key(KeyCode::Enter));
        std::fs::remove_file(path).unwrap();

        assert_eq!(app.workflows.len(), 2);
        assert_eq!(app.workflows[0].name, "Refreshed Workflow 0");
        assert_eq!(app.workflows[0].run_status, RunStatus::Success);
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
}
