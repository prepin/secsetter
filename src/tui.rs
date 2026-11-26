mod screen;
mod view;

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::{Action, App, RestoreDestination, Status, restore_destination};
use crate::config::{Entry, Kind};
use crate::login;
use screen::Screen;
use view::{Panel, View};

struct Confirmation {
    verb: &'static str,
    action: Action<'static>,
    cancel_selected: bool,
}

impl Confirmation {
    fn new(verb: &'static str, action: Action<'static>) -> Self {
        Self {
            verb,
            cancel_selected: matches!(action, Action::Clear),
            action,
        }
    }
}

pub(crate) fn run(app: &App) -> io::Result<()> {
    let mut screen = Screen::enter()?;
    let (mut entries, logins): (Vec<_>, Vec<_>) = app
        .entries()
        .partition(|entry| !matches!(entry.kind, Kind::Login { .. }));
    let login_start = (!logins.is_empty()).then_some(entries.len());
    entries.extend(logins);
    let mut selected = 0;
    let mut confirm: Option<Confirmation> = None;
    let mut notice = String::new();
    loop {
        let view = View {
            app,
            entries: &entries,
            login_start,
            selected,
        };
        let panel = match &confirm {
            Some(confirm) => Panel::Confirm {
                verb: confirm.verb,
                cancel_selected: confirm.cancel_selected,
            },
            None => Panel::Notice(&notice),
        };
        let usable = screen.draw(|frame| view.render(frame, panel))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release
            || (key.code == KeyCode::Enter && key.kind == KeyEventKind::Repeat)
        {
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
        if let Some(mut pending) = confirm.take() {
            let approve = match key.code {
                KeyCode::Char('y' | 'Y') => true,
                KeyCode::Enter => !pending.cancel_selected,
                KeyCode::Esc | KeyCode::Char('n' | 'N') => false,
                _ => {
                    pending.cancel_selected = match key.code {
                        KeyCode::Left => false,
                        KeyCode::Right => true,
                        KeyCode::Tab | KeyCode::BackTab => !pending.cancel_selected,
                        _ => pending.cancel_selected,
                    };
                    confirm = Some(pending);
                    continue;
                }
            };
            notice = if approve {
                perform(app, &mut screen, entries[selected], pending.action)?
            } else {
                format!("{} canceled", pending.verb)
            };
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Delete => {
                if let Some(entry) = entries.get(selected) {
                    match app.status(entry) {
                        Status::Set => confirm = Some(Confirmation::new("Clear", Action::Clear)),
                        Status::NotSet => notice = "Nothing to clear: secret is not set".into(),
                        status @ (Status::Blocked(_) | Status::Unknown(_)) => {
                            notice = status.to_string();
                        }
                        _ => notice = "Only secrets can be cleared".into(),
                    }
                }
            }
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
                            Status::NotInstalled => {
                                confirm = Some(Confirmation::new("Install", Action::Install));
                            }
                            Status::Unknown(_) if entry.ownership().is_some() => {
                                confirm = Some(Confirmation::new("Install", Action::Install));
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
                                    confirm = Some(Confirmation::new(
                                        "Replace",
                                        Action::Restore { replace: true },
                                    ));
                                }
                                Ok(RestoreDestination::RequiresElevation) => {
                                    confirm = Some(Confirmation::new(
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
                        Kind::Login { commands, .. } => {
                            let result = screen.with_terminal(
                                |interrupt| login::run(commands, interrupt),
                                |result| {
                                    result
                                        .as_ref()
                                        .err()
                                        .filter(|failure| !failure.interrupted())
                                        .map(login::Failure::terminal_summary)
                                },
                            )?;
                            notice = result.map_or_else(
                                |failure| failure.terminal_summary(),
                                |_| "Login completed".into(),
                            );
                        }
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
        screen.with_terminal(|_| app.perform(&entry.id, action), |_| None)?
    } else {
        app.perform(&entry.id, action)
    };
    Ok(result.map_or_else(|error| error, str::to_owned))
}

fn prompt_secret(screen: &mut Screen, view: &View<'_>) -> io::Result<Option<String>> {
    let mut secret = String::new();
    loop {
        let usable = screen.draw(|frame| {
            let counts: Vec<_> = secret
                .rsplit('\n')
                .take(usize::from(frame.area().height))
                .enumerate()
                .map(|(index, line)| {
                    let line = if index == 0 {
                        line
                    } else {
                        line.strip_suffix('\r').unwrap_or(line)
                    };
                    line.chars().count()
                })
                .collect();
            view.render(frame, Panel::Secret(&counts))
        })?;
        match event::read()? {
            Event::Paste(text) if usable => secret.push_str(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                if usable
                    && key.code == KeyCode::Char('j')
                    && key.modifiers == KeyModifiers::CONTROL
                {
                    secret.push('\n');
                    continue;
                }
                if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    _ if !usable => {}
                    KeyCode::Enter => return Ok(Some(secret)),
                    KeyCode::Backspace => {
                        if secret.pop() == Some('\n') && secret.ends_with('\r') {
                            secret.pop();
                        }
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
