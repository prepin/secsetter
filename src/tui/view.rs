use std::borrow::Cow;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Constraint::*, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
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
    Confirm {
        verb: &'a str,
        cancel_selected: bool,
    },
    Secret(&'a [usize]),
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
        let confirmation_destination = entry.and_then(|entry| match &entry.kind {
            Kind::Template { destination, .. } | Kind::FnoxFile { destination, .. } => {
                Some(destination.as_path())
            }
            _ => None,
        });
        let popup = (!matches!(panel, Panel::Notice(_))).then(|| {
            let scaled_width = (u32::from(area.width) * 62 / 100) as u16;
            let (width, height) = if matches!(panel, Panel::Confirm { .. }) {
                let width = scaled_width.clamp(48, 72).min(area.width - 2);
                let height = if width >= 48 && area.height >= 16 {
                    let wraps = confirmation_destination.is_some_and(|path| {
                        single_line(&path.to_string_lossy()).trim().width() > usize::from(width - 4)
                    });
                    9 + u16::from(wraps)
                } else {
                    5 + u16::from(area.height >= 10)
                };
                (width, height)
            } else {
                let width = scaled_width.clamp(56, 112).min(area.width - 2);
                let min_height = if width >= 56 && area.height >= 18 {
                    10
                } else {
                    5
                };
                (width, (area.height / 3).clamp(min_height, 12))
            };
            Rect::new(
                area.x + (area.width - width) / 2,
                area.y + (area.height - height).div_ceil(2),
                width,
                height,
            )
        });
        let status = entry.map(|entry| self.app.status(entry));
        let compact_reason = if wide {
            None
        } else {
            status.as_ref().and_then(compact_reason)
        };
        let message_rows = match panel {
            Panel::Secret(_) | Panel::Confirm { .. } => 0,
            Panel::Notice("") if compact_reason.is_none() => 0,
            _ => 2,
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
            self.list(frame, list, status.as_ref(), popup);
            let details = pane(frame, details, " Selected entry ");
            if let (Some(entry), Some(status)) = (entry, &status) {
                render_details(frame, details, entry, status);
            }
        } else {
            self.list(frame, body, status.as_ref(), popup);
        }

        if let Panel::Notice(notice) = panel {
            render_notice(frame, footer, notice, compact_reason, wide);
        } else if let (Some(popup), Some(entry), Some(status)) = (popup, entry, status) {
            for cell in &mut frame.buffer_mut().content {
                if cell.symbol() != " " && cell.bg != ACCENT {
                    cell.set_style(Style::new().dim());
                }
            }
            match panel {
                Panel::Confirm {
                    verb,
                    cancel_selected,
                } => {
                    render_confirmation(
                        frame,
                        popup,
                        &entry.label,
                        confirmation_destination,
                        verb,
                        cancel_selected,
                    );
                }
                Panel::Secret(counts) => render_secret(frame, popup, entry, &status, counts),
                Panel::Notice(_) => unreachable!(),
            }
        }
        true
    }

    fn list(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected_status: Option<&Status>,
        popup: Option<Rect>,
    ) {
        if self.entries.is_empty() {
            frame.render_widget("No entries for this platform".dim(), area);
            return;
        }
        let selected_row = self.selected
            + usize::from(self.login_start.is_some_and(|start| start <= self.selected));
        let rows = usize::from(area.height);
        let visible_rows = popup.map_or(rows, |popup| {
            rows.min(usize::from(popup.y.saturating_sub(area.y)))
        });
        let first = selected_row.saturating_sub(visible_rows.saturating_sub(1));
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

fn render_notice(
    frame: &mut Frame,
    area: Rect,
    notice: &str,
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
    let [message, controls] = Layout::vertical([Min(0), Length(1)]).areas(area_inside);
    let text = if notice.is_empty() {
        reason.unwrap_or_default()
    } else {
        single_line(notice)
    };
    frame.render_widget(block.border_style(Style::new().fg(Color::DarkGray)), area);
    frame.render_widget(Paragraph::new(Line::from(text)).wrap(WRAP), message);
    frame.render_widget(
        Line::from("↑/↓ move  Enter open  Esc quit".fg(ACCENT)),
        controls,
    );
}

fn render_confirmation(
    frame: &mut Frame,
    area: Rect,
    entry_label: &str,
    destination: Option<&Path>,
    verb: &str,
    cancel_selected: bool,
) {
    let title = match verb {
        "Install" => " Install config? ",
        "Replace" => " Replace from fnox? ",
        _ => " Restore or replace? ",
    };
    let inner = draw_popup(frame, area, title);
    let gap = u16::from(area.height >= 9);
    let [details, _, buttons, _, controls] =
        Layout::vertical([Fill(1), Length(gap), Length(1), Length(gap), Length(1)]).areas(inner);
    let width = usize::from(inner.width);
    let mut description = vec![fit(entry_label, width).bold().into()];
    if details.height > 1
        && let Some(destination) = destination
    {
        let path = destination.to_string_lossy();
        if gap > 0 {
            description.push("Destination".dim().into());
            description.push(single_line(&path).into_owned().dim().into());
        } else {
            let target = format!("To: {path}");
            let target = if target.width() > width {
                let filename = destination.file_name().unwrap_or(destination.as_os_str());
                format!("To: …/{}", filename.to_string_lossy())
            } else {
                target
            };
            description.push(fit(&target, width).into());
        }
    }
    frame.render_widget(Paragraph::new(description).wrap(WRAP), details);

    let compact = width < 36;
    let label = if compact && verb == "Restore or replace" {
        "Proceed"
    } else {
        verb
    };
    let button = |label: &str, selected: bool| {
        let text = if compact {
            format!("[{label}]")
        } else {
            format!("[ {label} ]")
        };
        if selected {
            text.bg(ACCENT).fg(Color::Black).bold()
        } else {
            text.dim()
        }
    };
    let marker = |selected| {
        if selected {
            "› ".fg(ACCENT)
        } else {
            "  ".into()
        }
    };
    frame.render_widget(
        Line::from(vec![
            marker(!cancel_selected),
            button(label, !cancel_selected),
            " ".into(),
            marker(cancel_selected),
            button("Cancel", cancel_selected),
        ]),
        buttons,
    );
    let keys = if width >= 42 {
        "←/→ Select · Enter Confirm · Esc Cancel"
    } else {
        "←/→ move  Enter  Esc"
    };
    frame.render_widget(Line::from(keys.dim()), controls);
}

fn draw_popup(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .padding(Padding::horizontal(1))
        .title(title.fg(ACCENT).bold());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

fn render_secret(frame: &mut Frame, area: Rect, entry: &Entry, status: &Status, counts: &[usize]) {
    let title = if matches!(status, Status::Set) {
        " Replace secret "
    } else {
        " Set secret "
    };
    let inner = draw_popup(frame, area, title);
    let expanded = area.width >= 56 && area.height >= 10;
    let width = usize::from(inner.width);

    let mut description = vec![fit(&entry.label, width).bold().into()];
    let mut detail_rows = 1;
    if expanded
        && let Kind::SecretFile { destination } | Kind::SecretField { destination, .. } =
            &entry.kind
    {
        let target = format!("Destination: {}", destination.display());
        let target = single_line(&target);
        detail_rows = 2 + u16::from(area.height > 10 && target.width() > width);
        description.push(target.into_owned().dim().into());
    }
    let [details, input, controls] =
        Layout::vertical([Length(detail_rows), Fill(1), Length(1)]).areas(inner);

    frame.render_widget(Paragraph::new(description).wrap(WRAP), details);

    let field = if expanded {
        let field = Block::bordered()
            .border_style(Style::new().fg(ACCENT))
            .padding(Padding::horizontal(1))
            .title(" Secret ".fg(ACCENT));
        let inner = field.inner(input);
        frame.render_widget(field, input);
        inner
    } else {
        input
    };
    let content = if counts == [0] {
        Paragraph::new("Paste or type a secret".dim())
    } else {
        Paragraph::new(masked_rows(
            counts,
            usize::from(field.width),
            usize::from(field.height),
        ))
    };
    frame.render_widget(content, field);
    let keys = if controls.width >= 42 {
        "Enter submit   Esc cancel   Ctrl+J newline"
    } else {
        "Enter OK  Ctrl+J line  Esc"
    };
    frame.render_widget(Line::from(keys.fg(ACCENT)), controls);
}

fn masked_rows(counts: &[usize], width: usize, height: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::with_capacity(height);
    // Work from the newest line back so long inputs keep their end in view.
    for &count in counts {
        let full_rows = count.saturating_sub(1) / width;
        rows.push(Line::from("*".repeat(count - full_rows * width).fg(ACCENT)));
        let visible = full_rows.min(height - rows.len());
        rows.extend((0..visible).map(|_| Line::from("*".repeat(width).fg(ACCENT))));
        if rows.len() == height {
            break;
        }
    }
    rows.reverse();
    rows
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

#[cfg(test)]
mod tests {
    use super::masked_rows;
    use ratatui::text::Line;

    #[test]
    fn masked_input_wraps_and_keeps_the_latest_rows_including_blank_lines() {
        let rows = masked_rows(&[2, 0, 12], 5, 4);
        assert_eq!(
            rows.iter().map(Line::width).collect::<Vec<_>>(),
            [5, 2, 0, 2],
        );
    }
}
