//! Terminal entry and restoration boundaries.

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{MuxError, Result};

/// Minimal lifecycle required to prove terminal restoration behavior.
pub trait TerminalControl {
    /// Enters the application's terminal mode.
    fn enter(&mut self) -> io::Result<()>;

    /// Restores the terminal to its caller-owned mode.
    fn leave(&mut self) -> io::Result<()>;
}

/// Runs an operation while guaranteeing a best-effort restoration on return or panic.
pub fn with_restoration<C, T, E>(
    control: C,
    operation: impl FnOnce(&mut C) -> std::result::Result<T, E>,
) -> io::Result<std::result::Result<T, E>>
where
    C: TerminalControl,
{
    let mut guard = RestorationGuard::enter(control)?;
    let result = operation(guard.control_mut());
    guard.restore()?;
    Ok(result)
}

/// Runs a Ratatui operation in raw alternate-screen mode.
pub fn with_terminal<W, T>(
    writer: W,
    operation: impl FnOnce(&mut Terminal<CrosstermBackend<&mut W>>) -> Result<T>,
) -> Result<T>
where
    W: Write,
{
    let mut guard = RestorationGuard::enter(CrosstermControl::new(writer)).map_err(|source| {
        MuxError::Filesystem {
            path: "terminal".into(),
            source,
        }
    })?;
    let mut terminal = Terminal::new(CrosstermBackend::new(guard.control_mut().writer_mut()))
        .map_err(|source| MuxError::Filesystem {
            path: "terminal".into(),
            source,
        })?;
    let result = operation(&mut terminal);
    drop(terminal);
    let restore_result = guard.restore().map_err(|source| MuxError::Filesystem {
        path: "terminal".into(),
        source,
    });
    match (result, restore_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

struct RestorationGuard<C: TerminalControl> {
    control: Option<C>,
}

impl<C: TerminalControl> RestorationGuard<C> {
    fn enter(mut control: C) -> io::Result<Self> {
        control.enter()?;
        Ok(Self {
            control: Some(control),
        })
    }

    fn control_mut(&mut self) -> &mut C {
        self.control.as_mut().expect("active terminal guard")
    }

    fn restore(&mut self) -> io::Result<()> {
        match self.control.take() {
            Some(mut control) => control.leave(),
            None => Ok(()),
        }
    }
}

impl<C: TerminalControl> Drop for RestorationGuard<C> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Crossterm implementation of the terminal lifecycle.
pub struct CrosstermControl<W> {
    writer: W,
}

impl<W> CrosstermControl<W> {
    /// Wraps a terminal writer.
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }
}

impl<W: Write> TerminalControl for CrosstermControl<W> {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        if let Err(error) = execute!(self.writer, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        if let Err(error) = execute!(self.writer, Hide) {
            let _ = execute!(self.writer, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(())
    }

    fn leave(&mut self) -> io::Result<()> {
        let screen_result = execute!(self.writer, Show, LeaveAlternateScreen);
        let raw_result = disable_raw_mode();
        screen_result.and(raw_result)
    }
}
