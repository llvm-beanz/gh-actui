use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct App {
    counter: i32,
    should_quit: bool,
}

impl App {
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
            KeyCode::Up | KeyCode::Char('+') => self.counter += 1,
            KeyCode::Down | KeyCode::Char('-') => self.counter -= 1,
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let title = Paragraph::new("GitHub CLI + Ratatui")
            .bold()
            .fg(Color::Cyan)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title(" gh-actui "));

        let counter = Paragraph::new(vec![
            Line::from("Your extension is ready."),
            Line::from(""),
            Line::from(vec![
                "Counter: ".into(),
                self.counter.to_string().yellow().bold(),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title(" Starter "));

        let help = Paragraph::new("Up/+ increment  |  Down/- decrement  |  q/Esc quit")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));

        frame.render_widget(title, areas[0]);
        frame.render_widget(counter, areas[1]);
        frame.render_widget(help, areas[2]);
    }
}

fn main() -> io::Result<()> {
    install_panic_hook();
    let mut terminal = ratatui::init();
    let result = App::default().run(&mut terminal);
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

    #[test]
    fn keyboard_input_updates_app_state() {
        let mut app = App::default();

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.counter, 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        assert_eq!(app.counter, 0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.should_quit);
    }
}
