use std::io::{self, IsTerminal, Stdout, Write, stdout};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

use crate::login::InterruptGuard;

pub(super) struct Screen {
    active: bool,
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    pub(super) fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !stdout().is_terminal() {
            return Err(io::Error::other("the TUI requires a terminal"));
        }
        let mut screen = Self {
            active: false,
            terminal: Terminal::new(CrosstermBackend::new(stdout()))?,
        };
        screen.resume()?;
        Ok(screen)
    }

    pub(super) fn draw<T>(&mut self, draw: impl FnOnce(&mut Frame) -> T) -> io::Result<T> {
        let mut rendered = None;
        self.terminal.draw(|frame| rendered = Some(draw(frame)))?;
        Ok(rendered.expect("successful draw rendered a frame"))
    }

    pub(super) fn with_terminal<T>(
        &mut self,
        operation: impl FnOnce(&InterruptGuard) -> T,
        pause_message: impl FnOnce(&T) -> Option<String>,
    ) -> io::Result<T> {
        let interrupt = InterruptGuard::new()?;
        self.leave()?;
        let original = tcgetattr(io::stdin())?;
        let result = operation(&interrupt);
        let pause = pause_message(&result);
        let restore = if pause.is_some() {
            OptionalActions::Flush
        } else {
            OptionalActions::Now
        };
        tcsetattr(io::stdin(), restore, &original)?;
        let waited = pause.as_deref().map_or(Ok(()), wait_for_enter);
        self.resume()?;
        waited?;
        Ok(result)
    }

    fn resume(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.active = true;
        execute!(stdout(), EnterAlternateScreen, Hide, EnableBracketedPaste)?;
        // The alternate screen needs a full repaint after a terminal handoff.
        self.terminal.resize(self.terminal.size()?.into())
    }

    fn leave(&mut self) -> io::Result<()> {
        let screen = execute!(stdout(), DisableBracketedPaste, Show, LeaveAlternateScreen);
        let raw = disable_raw_mode();
        raw.and(screen)?;
        self.active = false;
        Ok(())
    }
}

pub(super) struct MouseCapture;

impl MouseCapture {
    pub(super) fn enable() -> io::Result<Self> {
        let capture = Self;
        execute!(stdout(), EnableMouseCapture)?;
        Ok(capture)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
}

fn wait_for_enter(message: &str) -> io::Result<()> {
    enable_raw_mode()?;
    {
        let mut out = stdout().lock();
        write!(
            out,
            "\r\n{message}\r\nPress Enter to return to SecSetter.\r\n"
        )?;
        out.flush()?;
    }
    let mut byte = [0];
    while rustix::io::read(io::stdin(), &mut byte[..])? != 0 {
        if matches!(byte[0], b'\r' | b'\n' | b'\x03') {
            break;
        }
    }
    Ok(())
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let _ = self.leave();
        }
    }
}
