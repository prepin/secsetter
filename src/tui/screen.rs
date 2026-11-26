use std::io::{self, IsTerminal, Stdout, stdout};
use std::{mem, ptr};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

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

    pub(super) fn draw(&mut self, draw: impl FnOnce(&mut Frame) -> bool) -> io::Result<bool> {
        let mut usable = false;
        self.terminal.draw(|frame| usable = draw(frame))?;
        Ok(usable)
    }

    pub(super) fn with_terminal<T>(&mut self, operation: impl FnOnce() -> T) -> io::Result<T> {
        let _interrupt = InterruptGuard::new()?;
        self.leave()?;
        let original = tcgetattr(io::stdin())?;
        let result = operation();
        tcsetattr(io::stdin(), OptionalActions::Now, &original)?;
        self.resume()?;
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

impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let _ = self.leave();
        }
    }
}

struct InterruptGuard(libc::sigaction);

impl InterruptGuard {
    fn new() -> io::Result<Self> {
        extern "C" fn keep_tui_alive(_: libc::c_int) {}

        // The parent and child share the foreground process group. A caught
        // handler keeps the TUI alive and resets to the default on exec.
        // SAFETY: The structures are initialized and the handler is signal-safe.
        unsafe {
            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = keep_tui_alive as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous = mem::MaybeUninit::uninit();
            if libc::sigaction(libc::SIGINT, &action, previous.as_mut_ptr()) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(previous.assume_init()))
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // SAFETY: The saved action came from a successful call to sigaction.
        unsafe { libc::sigaction(libc::SIGINT, &self.0, ptr::null_mut()) };
    }
}
