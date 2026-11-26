mod screen;

use std::io::{self, Write, stdout};
use std::os::unix::process::ExitStatusExt;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use unicode_width::UnicodeWidthChar;

use crate::app::{Action, App, Status};
use crate::config::{Entry, Kind};
use crate::login;
use screen::Screen;

pub(crate) fn run(app: &App) -> io::Result<()> {
    let mut screen = Screen::enter()?;
    let (mut entries, logins): (Vec<_>, Vec<_>) = app
        .entries()
        .partition(|entry| !matches!(entry.kind, Kind::Login { .. }));
    let login_start = (!logins.is_empty()).then_some(entries.len());
    entries.extend(logins);
    let mut selected = 0;
    let mut confirm = false;
    let mut notice = String::new();
    loop {
        let prompt = if confirm {
            format!("Install {}? [y/N]", entries[selected].label)
        } else {
            notice.clone()
        };
        draw(app, &entries, login_start, selected, &prompt)?;
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
                    match &entry.kind {
                        Kind::Template { .. } => match app.status(entry) {
                            Status::NotInstalled => confirm = true,
                            Status::Installed => notice = "Already installed".into(),
                            status => notice = status.to_string(),
                        },
                        Kind::SecretFile { .. } | Kind::SecretField { .. } => {
                            notice = match prompt_secret(|prompt| {
                                draw(app, &entries, login_start, selected, prompt)
                            })? {
                                Some(secret) => match app
                                    .perform(&entry.id, Action::Set(&mut secret.as_bytes()))
                                {
                                    Ok(message) => message.to_owned(),
                                    Err(error) => error,
                                },
                                None => "Set canceled".into(),
                            };
                        }
                        Kind::Login { command } => notice = run_login(&mut screen, command)?,
                    }
                }
            }
            _ => {}
        }
    }
}

fn prompt_secret(
    mut draw_prompt: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<Option<String>> {
    let mut secret = String::new();
    loop {
        let (width, _) = terminal::size()?;
        let count = secret.chars().take(usize::from(width)).count();
        draw_prompt(&format!(
            "Secret (Enter submit, Esc cancel): {}",
            "*".repeat(count)
        ))?;
        match event::read()? {
            Event::Paste(text) => secret.push_str(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => return Ok(Some(secret)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        secret.pop();
                    }
                    KeyCode::Char(character) => secret.push(character),
                    KeyCode::Tab => secret.push('\t'),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn run_login(screen: &mut Screen, command: &[String]) -> io::Result<String> {
    let result = screen.with_terminal(|| login::status(command))?;
    Ok(match result {
        Ok(status) if status.success() => "Login completed".into(),
        Ok(status)
            if status.signal() == Some(libc::SIGINT)
                || status.code() == Some(128 + libc::SIGINT) =>
        {
            "Login interrupted".into()
        }
        Ok(status) => format!("Login failed: {status}"),
        Err(error) => format!("Could not run login: {error}"),
    })
}

fn draw(
    app: &App,
    entries: &[&Entry],
    login_start: Option<usize>,
    selected: usize,
    prompt: &str,
) -> io::Result<()> {
    let (width, height) = terminal::size()?;
    let mut out = stdout().lock();
    queue!(out, Clear(ClearType::All))?;
    let header = if entries.is_empty() {
        "SecSetter — No entries for this platform   Esc quit"
    } else {
        "SecSetter — ↑/↓ select   Enter open   Esc quit"
    };
    line(&mut out, 0, width, header)?;
    let rows = height.saturating_sub(2);
    let selected_row = selected + usize::from(login_start.is_some_and(|start| selected >= start));
    let first = selected_row.saturating_sub(usize::from(rows.saturating_sub(1)));
    for (row, index) in (1..=rows).zip(first..) {
        if Some(index) == login_start {
            line(&mut out, row, width, "Login")?;
            continue;
        }
        let entry_index = index - usize::from(login_start.is_some_and(|start| index > start));
        let Some(entry) = entries.get(entry_index) else {
            break;
        };
        let mark = if entry_index == selected { ">" } else { " " };
        let label = &entry.label;
        let text = match app.status(entry) {
            Status::Untracked => format!("{mark} {label}"),
            status => format!("{mark} {label} — {status}"),
        };
        line(&mut out, row, width, &text)?;
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
