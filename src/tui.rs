mod screen;
mod view;

use std::io;
use std::os::unix::process::ExitStatusExt;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::{Action, App, RestoreDestination, Status, restore_destination};
use crate::config::{Entry, Kind};
use crate::login;
use screen::Screen;
use view::{Panel, View};

pub(crate) fn run(app: &App) -> io::Result<()> {
    let mut screen = Screen::enter()?;
    let (mut entries, logins): (Vec<_>, Vec<_>) = app
        .entries()
        .partition(|entry| !matches!(entry.kind, Kind::Login { .. }));
    let login_start = (!logins.is_empty()).then_some(entries.len());
    entries.extend(logins);
    let mut selected = 0;
    let mut confirm: Option<(&str, Action<'_>)> = None;
    let mut notice = String::new();
    loop {
        let view = View {
            app,
            entries: &entries,
            login_start,
            selected,
        };
        let panel = match &confirm {
            Some((verb, _)) => Panel::Confirm(verb),
            None => Panel::Notice(&notice),
        };
        let usable = screen.draw(|frame| view.render(frame, panel))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if !usable {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('q') if confirm.is_none() => return Ok(()),
                _ => continue,
            }
        }
        if let Some((verb, action)) = confirm.take() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    notice = perform(app, &mut screen, entries[selected], action)?;
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n' | 'N') => {
                    notice = format!("{verb} canceled");
                }
                _ => confirm = Some((verb, action)),
            }
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Down => {
                let next = if key.code == KeyCode::Up {
                    selected.saturating_sub(1)
                } else {
                    (selected + 1).min(entries.len().saturating_sub(1))
                };
                if next != selected {
                    selected = next;
                    notice.clear();
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = entries.get(selected) {
                    match &entry.kind {
                        Kind::Template { .. } => match app.status(entry) {
                            Status::NotInstalled => confirm = Some(("Install", Action::Install)),
                            Status::Unknown(_) if entry.ownership().is_some() => {
                                confirm = Some(("Install", Action::Install));
                            }
                            Status::Installed => notice = "Already installed".into(),
                            status => notice = status.to_string(),
                        },
                        Kind::SecretFile { .. } | Kind::SecretField { .. } => {
                            notice = match prompt_secret(&mut screen, &view)? {
                                Some(secret) => perform(
                                    app,
                                    &mut screen,
                                    entry,
                                    Action::Set(&mut secret.as_bytes()),
                                )?,
                                None => "Set canceled".into(),
                            };
                        }
                        Kind::FnoxFile { destination, .. } => {
                            match restore_destination(entry, destination) {
                                Ok(RestoreDestination::Present) => {
                                    confirm = Some(("Replace", Action::Restore { replace: true }));
                                }
                                Ok(RestoreDestination::RequiresElevation) => {
                                    confirm = Some((
                                        "Restore or replace",
                                        Action::Restore { replace: true },
                                    ));
                                }
                                Ok(RestoreDestination::Missing) => {
                                    notice = perform(
                                        app,
                                        &mut screen,
                                        entry,
                                        Action::Restore { replace: false },
                                    )?;
                                }
                                Err(error) => notice = error.to_string(),
                            }
                        }
                        Kind::Login { command } => notice = run_login(&mut screen, command)?,
                    }
                }
            }
            _ => {}
        }
    }
}

fn perform(
    app: &App,
    screen: &mut Screen,
    entry: &Entry,
    action: Action<'_>,
) -> io::Result<String> {
    let result = if entry.ownership().is_some() || matches!(action, Action::Restore { .. }) {
        screen.with_terminal(|| app.perform(&entry.id, action))?
    } else {
        app.perform(&entry.id, action)
    };
    Ok(result.map_or_else(|error| error, str::to_owned))
}

fn prompt_secret(screen: &mut Screen, view: &View<'_>) -> io::Result<Option<String>> {
    let mut secret = String::new();
    loop {
        let usable = screen.draw(|frame| {
            let count = secret.chars().take(usize::from(frame.area().width)).count();
            view.render(frame, Panel::Secret(count))
        })?;
        match event::read()? {
            Event::Paste(text) if usable => secret.push_str(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    _ if !usable => {}
                    KeyCode::Enter => return Ok(Some(secret)),
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
