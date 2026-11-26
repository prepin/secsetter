use std::io::{self, IsTerminal, stdout};
use std::{mem, ptr};

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};

pub(super) struct Screen {
    active: bool,
}

impl Screen {
    pub(super) fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !stdout().is_terminal() {
            return Err(io::Error::other("the TUI requires a terminal"));
        }
        let mut screen = Self { active: false };
        screen.resume()?;
        Ok(screen)
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
        execute!(stdout(), EnterAlternateScreen, Hide)
    }

    fn leave(&mut self) -> io::Result<()> {
        let raw = disable_raw_mode();
        let screen = execute!(stdout(), Show, LeaveAlternateScreen);
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
