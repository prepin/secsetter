use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Constraint::*, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Status};
use crate::config::{Entry, Kind};
use crate::paths::PathError;

const ACCENT: Color = Color::Cyan;
const STATUS_WIDTH: usize = 15;
const WRAP: Wrap = Wrap { trim: true };

#[derive(Clone, Copy)]
pub(super) enum Panel<'a> {
    Notice(&'a str),
    Confirm(&'a str),
    Secret(usize),
}

pub(super) struct View<'a> {
    pub app: &'a App,
    pub entries: &'a [&'a Entry],
    pub login_start: Option<usize>,
    pub selected: usize,
}

impl View<'_> {
    pub fn render(&self, frame: &mut Frame, panel: Panel<'_>) -> bool {
        let area = frame.area();
        if area.width < 32 || area.height < 8 {
            let keys = match panel {
                Panel::Notice(_) => "Esc quit",
                _ => "Esc cancel",
            };
            let message =
                Paragraph::new(vec!["Resize to 32×8".fg(ACCENT).into(), keys.dim().into()])
                    .centered()
                    .wrap(WRAP);
            frame.render_widget(message, area);
            return false;
        }

        let wide = area.width >= 90 && area.height >= 22;
        let entry = self.entries.get(self.selected).copied();
        let status = entry.map(|entry| self.app.status(entry));
        let compact_reason = if wide {
            None
        } else {
            status.as_ref().and_then(compact_reason)
        };
        let message_rows = if matches!(panel, Panel::Notice("")) && compact_reason.is_none() {
            0
        } else {
            2
        };
        let [header, body, footer] = Layout::vertical([
            Length(1),
            Min(1),
            Length(message_rows + if wide { 3 } else { 2 }),
        ])
        .areas(area.inner(Margin::new(1, 0)));
        frame.render_widget(
            Line::from(vec![
                " SecSetter ".fg(Color::Black).bg(ACCENT).bold(),
                "  Private setup".dim(),
            ]),
            header,
        );

        if wide {
            let [list, details] = Layout::horizontal([Fill(6), Fill(5)])
                .spacing(1)
                .areas(body);
            let list = pane(frame, list, " Entries ");
            self.list(frame, list, status.as_ref());
            let details = pane(frame, details, " Selected entry ");
            if let (Some(entry), Some(status)) = (entry, &status) {
                render_details(frame, details, entry, status);
            }
        } else {
            self.list(frame, body, status.as_ref());
        }

        render_panel(frame, footer, panel, entry, compact_reason, wide);
        true
    }

    fn list(&self, frame: &mut Frame, area: Rect, selected_status: Option<&Status>) {
        if self.entries.is_empty() {
            frame.render_widget("No entries for this platform".dim(), area);
            return;
        }
        let selected_row = self.selected
            + usize::from(self.login_start.is_some_and(|start| start <= self.selected));
        let rows = usize::from(area.height);
        let first = selected_row.saturating_sub(rows.saturating_sub(1));
        let mut lines = Vec::with_capacity(rows);
        for row in first..first + rows {
            if self.login_start == Some(row) {
                lines.push(Line::from(" Login".fg(ACCENT).bold()));
                continue;
            }
            let index = row - usize::from(self.login_start.is_some_and(|start| start < row));
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            let selected = index == self.selected;
            let badge = match selected_status.filter(|_| selected) {
                Some(status) => badge(status),
                None => badge(&self.app.status(entry)),
            };
            lines.push(entry_line(&entry.label, badge, area.width, selected));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

fn pane(frame: &mut Frame, area: Rect, title: &'static str) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .title(title.fg(ACCENT).bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn badge(status: &Status) -> (&'static str, Color) {
    match status {
        Status::Installed => ("✓ Installed", Color::Green),
        Status::NotInstalled => ("○ Not installed", Color::Yellow),
        Status::Set => ("✓ Set", Color::Green),
        Status::NotSet => ("○ Not set", Color::Yellow),
        Status::Blocked(_) => ("! Blocked", Color::Red),
        Status::Unknown(_) => ("? Unknown", Color::Yellow),
        Status::Untracked => ("→", ACCENT),
    }
}

fn entry_line(
    label: &str,
    (badge, color): (&str, Color),
    width: u16,
    selected: bool,
) -> Line<'static> {
    let label_width = usize::from(width).saturating_sub(STATUS_WIDTH + 4);
    let label = fit(label, label_width);
    let padding = " ".repeat(label_width.saturating_sub(label.width()));
    let title = if selected {
        format!("› {label}{padding}")
            .bg(ACCENT)
            .fg(Color::Black)
            .bold()
    } else {
        Span::raw(format!("  {label}{padding}"))
    };
    Line::from(vec![title, "  ".into(), badge.to_owned().fg(color)])
}

fn render_details(frame: &mut Frame, area: Rect, entry: &Entry, status: &Status) {
    let (badge, color) = badge(status);
    let (destination, action) = match &entry.kind {
        Kind::Template { destination, .. } => (
            Some(destination),
            if matches!(status, Status::Installed) {
                "Already installed"
            } else {
                "Enter to install"
            },
        ),
        Kind::SecretField { destination, .. } | Kind::SecretFile { destination } => (
            Some(destination),
            if matches!(status, Status::Set) {
                "Enter to replace the secret"
            } else {
                "Enter to set the secret"
            },
        ),
        Kind::FnoxFile { destination, .. } => (Some(destination), "Enter to restore from fnox"),
        Kind::Login { .. } => (None, "Enter to start the login command"),
    };
    let error = match status {
        Status::Blocked(error) | Status::Unknown(error) => Some(error),
        _ => None,
    };
    let action = match status {
        Status::Blocked(_) => "See the reason below",
        Status::Unknown(_) if entry.ownership().is_none() => "See the reason below",
        _ => action,
    };
    let [summary, body] = Layout::vertical([Length(6), Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(vec![
            fit(&entry.label, usize::from(area.width)).bold().into(),
            if destination.is_some() {
                badge.fg(color).into()
            } else {
                "→ Login action".fg(ACCENT).into()
            },
            Line::default(),
            "Next action".fg(ACCENT).into(),
            action.into(),
        ])
        .wrap(WRAP),
        summary,
    );
    let destination_area = if let Some(error) = error {
        let [destination_area, reason] = Layout::vertical([Fill(1), Fill(1)]).areas(body);
        render_detail(frame, reason, "Reason", error.to_string());
        destination_area
    } else {
        body
    };
    if let Some(destination) = destination {
        render_detail(
            frame,
            destination_area,
            "Destination",
            destination.display().to_string(),
        );
    }
}

fn render_detail(frame: &mut Frame, area: Rect, heading: &'static str, value: String) {
    frame.render_widget(
        Paragraph::new(vec![
            heading.fg(ACCENT).into(),
            single_line(&value).into_owned().into(),
        ])
        .wrap(WRAP),
        area,
    );
}

fn compact_reason(status: &Status) -> Option<Cow<'_, str>> {
    let error = match status {
        Status::Blocked(error) | Status::Unknown(error) => error,
        _ => return None,
    };
    match error {
        PathError::Blocked(_, reason) => Some(Cow::Borrowed(reason)),
        PathError::Io(_, operation, cause) => Some(Cow::Owned(format!("{operation}: {cause}"))),
        _ => None,
    }
}

fn render_panel(
    frame: &mut Frame,
    area: Rect,
    panel: Panel<'_>,
    entry: Option<&Entry>,
    reason: Option<Cow<'_, str>>,
    wide: bool,
) {
    let block = if wide {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
    } else {
        Block::new().borders(Borders::TOP)
    };
    let area_inside = block.inner(area);
    let border = match panel {
        Panel::Notice(_) => Color::DarkGray,
        Panel::Confirm(_) => Color::Yellow,
        Panel::Secret(_) => ACCENT,
    };
    frame.render_widget(block.border_style(Style::new().fg(border)), area);
    let [message, controls] = Layout::vertical([Min(0), Length(1)]).areas(area_inside);
    let (content, keys) = match panel {
        Panel::Notice(notice) => {
            let text = if notice.is_empty() {
                reason.unwrap_or_default()
            } else {
                single_line(notice)
            };
            (Line::from(text), "↑/↓ move  Enter open  Esc quit")
        }
        Panel::Confirm(verb) => {
            let question = format!("{verb} {}?", entry.map_or("", |entry| entry.label.as_str()));
            (
                fit(&question, usize::from(message.width)).bold().into(),
                "[y/N]   Esc cancel",
            )
        }
        Panel::Secret(count) => {
            let width = usize::from(message.width).saturating_sub("Secret: ".len());
            let masked = if count > width {
                format!("{}…", "*".repeat(width.saturating_sub(1)))
            } else {
                "*".repeat(count)
            };
            (
                Line::from(vec!["Secret: ".bold(), masked.fg(ACCENT)]),
                "Enter submit   Esc cancel",
            )
        }
    };
    frame.render_widget(Paragraph::new(content).wrap(WRAP), message);
    frame.render_widget(Line::from(keys.fg(ACCENT)), controls);
}

fn single_line(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_control) {
        Cow::Owned(text.chars().filter(|c| !c.is_control()).collect())
    } else {
        Cow::Borrowed(text)
    }
}

fn fit(text: &str, width: usize) -> String {
    let text = single_line(text);
    if text.width() <= width {
        return text.into_owned();
    }
    let line = Line::from(text);
    let mut available = width.saturating_sub(1);
    let mut visible: String = line
        .styled_graphemes(Style::new())
        .map(|cell| cell.symbol)
        .take_while(|symbol| {
            let columns = symbol.width();
            if columns > available {
                return false;
            }
            available -= columns;
            true
        })
        .collect();
    if width > 0 {
        visible.push('…');
    }
    visible
}
