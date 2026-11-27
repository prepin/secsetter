mod screen;
mod view;

use std::io;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::app::{Action, App, CopyDestination, Status, copy_destination};
use crate::config::{Entry, Kind};
use crate::login;
use screen::{MouseCapture, Screen};
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

enum Row<'a> {
    Heading(&'a str),
    Entry(&'a Entry),
}

impl<'a> Row<'a> {
    fn entry(&self) -> Option<&'a Entry> {
        match self {
            Self::Heading(_) => None,
            Self::Entry(entry) => Some(entry),
        }
    }
}

fn grouped_rows(app: &App) -> Vec<Row<'_>> {
    let mut sections: Vec<(&str, Vec<&Entry>)> = Vec::new();
    let mut templates = Vec::new();
    let mut secrets = Vec::new();
    let mut files = Vec::new();
    let mut logins = Vec::new();
    let mut fnox = Vec::new();
    for entry in app.entries() {
        if let Some(name) = entry.section.as_deref() {
            if let Some((_, entries)) = sections.iter_mut().find(|(section, _)| *section == name) {
                entries.push(entry);
            } else {
                sections.push((name, vec![entry]));
            }
        } else {
            match entry.kind {
                Kind::Template { .. } => templates.push(entry),
                Kind::SecretField { .. } => secrets.push(entry),
                Kind::SecretFile { .. } => files.push(entry),
                Kind::Login { .. } => logins.push(entry),
                Kind::FnoxFile { .. } => fnox.push(entry),
            }
        }
    }
    sections.sort_by_cached_key(|(name, _)| name.to_lowercase());
    sections.extend([
        ("Templates", templates),
        ("Secrets", secrets),
        ("Files", files),
        ("Logins", logins),
        ("Fnox", fnox),
    ]);
    let mut rows = Vec::new();
    for (name, entries) in sections
        .into_iter()
        .filter(|(_, entries)| !entries.is_empty())
    {
        rows.push(Row::Heading(name));
        rows.extend(entries.into_iter().map(Row::Entry));
    }
    rows
}

pub(crate) fn run(app: &App) -> io::Result<()> {
    let mut screen = Screen::enter()?;
    let rows = grouped_rows(app);
    let mut selected = rows
        .iter()
        .position(|row| row.entry().is_some())
        .unwrap_or(0);
    let mut confirm: Option<Confirmation> = None;
    let mut notice = String::new();
    let mut browser = Browser::default();
    loop {
        let view = View {
            app,
            rows: &rows,
            selected,
        };
        let panel = match &confirm {
            Some(confirm) => Panel::Confirm {
                verb: confirm.verb,
                cancel_selected: confirm.cancel_selected,
            },
            None => Panel::Notice(&notice),
        };
        let (usable, _) = screen.draw(|frame| view.render(frame, panel))?;
        let Some(Event::Key(key)) = browser.read_event(&mut notice)? else {
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
        let entry = view.selected_entry();
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
                let entry = entry.expect("confirmation requires a selected entry");
                perform(app, &mut screen, entry, pending.action)?
            } else {
                format!("{} canceled", pending.verb)
            };
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('o')
                if key.modifiers.is_empty()
                    && key.kind == KeyEventKind::Press
                    && let Some(url) = entry.and_then(|entry| entry.url.as_deref()) =>
            {
                notice = browser.open(url);
            }
            KeyCode::Delete => {
                if let Some(entry) = entry {
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
                    rows[..selected]
                        .iter()
                        .rposition(|row| row.entry().is_some())
                } else {
                    rows.iter()
                        .enumerate()
                        .skip(selected + 1)
                        .find_map(|(index, row)| row.entry().map(|_| index))
                };
                if let Some(next) = next {
                    selected = next;
                    notice.clear();
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = entry {
                    match &entry.kind {
                        kind @ (Kind::Template { destination, .. }
                        | Kind::FnoxFile { destination, .. }) => {
                            match copy_destination(entry, destination) {
                                Ok(state) => {
                                    let replace = !matches!(state, CopyDestination::Missing);
                                    let (verb, action) = if matches!(kind, Kind::Template { .. }) {
                                        let verb = match state {
                                            CopyDestination::Missing => "Install",
                                            CopyDestination::Present => "Reinstall",
                                            CopyDestination::RequiresElevation => {
                                                "Install or replace"
                                            }
                                        };
                                        (verb, Action::Install { replace })
                                    } else {
                                        let verb = match state {
                                            CopyDestination::Missing => "Restore",
                                            CopyDestination::Present => "Replace",
                                            CopyDestination::RequiresElevation => {
                                                "Restore or replace"
                                            }
                                        };
                                        (verb, Action::Restore { replace })
                                    };
                                    confirm = Some(Confirmation::new(verb, action));
                                }
                                Err(error) => notice = error.for_write(),
                            }
                        }
                        Kind::SecretFile { .. } | Kind::SecretField { .. } => {
                            notice = match prompt_secret(&mut screen, &view, &mut browser)? {
                                Some(secret) => perform(
                                    app,
                                    &mut screen,
                                    entry,
                                    Action::Set(&mut secret.as_bytes()),
                                )?,
                                None => "Set canceled".into(),
                            };
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

fn prompt_secret(
    screen: &mut Screen,
    view: &View<'_>,
    browser: &mut Browser,
) -> io::Result<Option<String>> {
    let url = view.selected_entry().and_then(|entry| entry.url.as_deref());
    let _mouse = url.map(|_| MouseCapture::enable()).transpose()?;
    let mut secret = String::new();
    let mut notice = String::new();
    loop {
        let (usable, link) = screen.draw(|frame| {
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
            view.render(
                frame,
                Panel::Secret {
                    counts: &counts,
                    notice: &notice,
                },
            )
        })?;
        match browser.read_event(&mut notice)? {
            Some(Event::Paste(text)) if usable => secret.push_str(&text),
            Some(Event::Mouse(mouse))
                if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && link.is_some_and(|link| link.contains((mouse.column, mouse.row).into()))
                    && let Some(url) = url =>
            {
                notice = browser.open(url);
            }
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
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
                if usable
                    && key.code == KeyCode::Char('o')
                    && key.modifiers == KeyModifiers::CONTROL
                    && key.kind == KeyEventKind::Press
                    && let Some(url) = url
                {
                    notice = browser.open(url);
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

#[derive(Default)]
struct Browser {
    openers: Vec<Child>,
}

impl Browser {
    fn open(&mut self, url: &str) -> String {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match Command::new(opener)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.openers.push(child);
                String::new()
            }
            Err(error) => format!("Could not open token page: {error}"),
        }
    }

    fn read_event(&mut self, notice: &mut String) -> io::Result<Option<Event>> {
        loop {
            let mut failed = false;
            self.openers.retain_mut(|child| {
                let result = match child.try_wait() {
                    Ok(None) => return true,
                    Ok(Some(status)) if status.success() => return false,
                    Ok(Some(status)) => status.to_string(),
                    Err(error) => error.to_string(),
                };
                *notice = format!("Could not open token page: {result}");
                failed = true;
                false
            });
            if failed {
                return Ok(None);
            }
            // Some Linux openers stay alive for the browser's lifetime.
            if self.openers.is_empty() || event::poll(Duration::from_millis(100))? {
                return event::read().map(Some);
            }
        }
    }
}
