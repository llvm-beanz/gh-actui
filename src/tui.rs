use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::{github::Workflow, repository::Repository};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);

struct App {
    repository: Repository,
    workflows: Vec<Workflow>,
    table_state: TableState,
    should_quit: bool,
}

impl App {
    fn new(repository: Repository, workflows: Vec<Workflow>) -> Self {
        let selected = (!workflows.is_empty()).then_some(0);
        Self {
            repository,
            workflows,
            table_state: TableState::default().with_selected(selected),
            should_quit: false,
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.select_next(1),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(1),
            KeyCode::PageDown => self.select_next(10),
            KeyCode::PageUp => self.select_previous(10),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            _ => {}
        }
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
            let empty = Paragraph::new("This repository has no GitHub Actions workflows.")
                .centered()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", self.repository)),
                );
            frame.render_widget(empty, table_area);
        } else {
            let header = Row::new(["Name", "State", "Path"])
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .bottom_margin(1);
            let rows = self.workflows.iter().map(|workflow| {
                Row::new([
                    Cell::from(workflow.name.as_str()),
                    Cell::from(workflow.state.as_str()),
                    Cell::from(workflow.path.as_str()),
                ])
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(30),
                    Constraint::Length(20),
                    Constraint::Min(20),
                ],
            )
            .header(header)
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ")
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} - {} workflows ",
                self.repository,
                self.workflows.len()
            )));

            frame.render_stateful_widget(table, table_area, &mut self.table_state);
        }

        let help = Paragraph::new(
            "Up/k Down/j: select  |  PgUp/PgDn: scroll  |  Home/End: jump  |  q/Esc: quit",
        )
        .dark_gray()
        .centered()
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(help, help_area);
    }
}

pub fn run(repository: Repository, workflows: Vec<Workflow>) -> io::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    let result = App::new(repository, workflows).run(&mut terminal);
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
    use crossterm::event::KeyModifiers;

    fn workflow(id: u64) -> Workflow {
        Workflow {
            id,
            name: format!("Workflow {id}"),
            path: format!(".github/workflows/{id}.yml"),
            state: "active".to_owned(),
        }
    }

    fn app(workflow_count: u64) -> App {
        App::new(
            "owner/repository".parse().unwrap(),
            (0..workflow_count).map(workflow).collect(),
        )
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
        let mut app = app(3);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.table_state.selected(), Some(2));

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.table_state.selected(), Some(2));

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn handle_key_quits_on_escape() {
        let mut app = app(1);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.should_quit);
    }
}
