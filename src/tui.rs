use std::io::{self, IsTerminal, Write, stdout};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use unicode_width::UnicodeWidthChar;

use crate::app::{Action, App, Status};
use crate::config::Entry;

struct Screen;

impl Screen {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !stdout().is_terminal() {
            return Err(io::Error::other("the TUI requires a terminal"));
        }
        enable_raw_mode()?;
        let screen = Self;
        execute!(stdout(), EnterAlternateScreen, Hide)?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
    }
}

pub(crate) fn run(app: &App) -> io::Result<()> {
    let _screen = Screen::enter()?;
    let entries: Vec<_> = app.entries().collect();
    let mut selected = 0;
    let mut confirm = false;
    let mut notice = String::new();
    loop {
        let prompt = if confirm {
            format!("Install {}? [y/N]", entries[selected].label)
        } else {
            notice.clone()
        };
        draw(app, &entries, selected, &prompt)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if confirm {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    notice = match app.perform(&entries[selected].id, Action::Install) {
                        Ok(message) => message.to_owned(),
                        Err(error) => error,
                    };
                    confirm = false;
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n' | 'N') => {
                    confirm = false;
                    notice = "Install canceled".into();
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(entries.len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(entry) = entries.get(selected) {
                    match app.status(entry) {
                        Status::NotInstalled => confirm = true,
                        Status::Installed => notice = "Already installed".into(),
                        status => notice = status.to_string(),
                    }
                }
            }
            _ => {}
        }
    }
}

fn draw(app: &App, entries: &[&Entry], selected: usize, prompt: &str) -> io::Result<()> {
    let (width, height) = terminal::size()?;
    let mut out = stdout().lock();
    queue!(out, Clear(ClearType::All))?;
    let header = if entries.is_empty() {
        "SecSetter — No entries for this platform   Esc quit"
    } else {
        "SecSetter — ↑/↓ select   Enter install   Esc quit"
    };
    line(&mut out, 0, width, header)?;
    let rows = height.saturating_sub(2);
    let first = selected.saturating_sub(usize::from(rows.saturating_sub(1)));
    for (row, entry) in (1..=rows).zip(entries.iter().skip(first)) {
        let mark = if first + usize::from(row - 1) == selected {
            ">"
        } else {
            " "
        };
        let status = app.status(entry);
        line(
            &mut out,
            row,
            width,
            &format!("{mark} {} — {status}", entry.label),
        )?;
    }
    if !prompt.is_empty() {
        line(&mut out, height.saturating_sub(1), width, prompt)?;
    }
    out.flush()
}

fn line(out: &mut impl Write, row: u16, width: u16, text: &str) -> io::Result<()> {
    let mut available = usize::from(width.saturating_sub(1));
    let visible: String = text
        .chars()
        .filter(|character| !character.is_control())
        .take_while(|character| {
            let columns = character.width().unwrap_or(0);
            if columns > available {
                return false;
            }
            available -= columns;
            true
        })
        .collect();
    queue!(out, MoveTo(0, row), Print(visible))
}
