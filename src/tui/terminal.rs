//! Terminal setup and the inline renderer.
//!
//! viper renders into the normal scrollback. Finished output is printed once and scrolls away
//! naturally; a small "live region" at the bottom (streaming text, running tools, the editor,
//! and the footer) is redrawn in place.

use std::io::{self, Write};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal;

use super::text::visible_width;

/// Raw mode, bracketed paste, and keyboard enhancements; restored on drop (including panics).
pub struct TerminalGuard {
    enhanced: bool,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<TerminalGuard> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnableBracketedPaste)?;
        let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
        if enhanced {
            crossterm::execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        install_panic_hook();
        Ok(TerminalGuard { enhanced })
    }

    /// Temporarily hand the terminal to another program (an external editor).
    pub fn suspend(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        if self.enhanced {
            crossterm::execute!(stdout, PopKeyboardEnhancementFlags)?;
        }
        crossterm::execute!(stdout, DisableBracketedPaste, crossterm::cursor::Show)?;
        terminal::disable_raw_mode()
    }

    pub fn resume(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnableBracketedPaste)?;
        if self.enhanced {
            crossterm::execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::execute!(
                io::stdout(),
                PopKeyboardEnhancementFlags,
                DisableBracketedPaste,
                crossterm::cursor::Show
            );
            let _ = terminal::disable_raw_mode();
            print!("\r\n");
            previous(info);
        }));
    });
}

pub fn size() -> (usize, usize) {
    let (w, h) = terminal::size().unwrap_or((80, 24));
    (w.max(20) as usize, h.max(5) as usize)
}

/// Draws the live region and commits finished lines above it.
pub struct Screen {
    /// Visible widths of the live lines currently on screen.
    drawn: Vec<usize>,
    /// Cursor position within the drawn live region as (line index, column).
    cursor: (usize, usize),
}

impl Screen {
    pub fn new() -> Screen {
        Screen { drawn: Vec::new(), cursor: (0, 0) }
    }

    /// Rows the drawn region occupies above the cursor at terminal width `width`, accounting for
    /// lines the terminal re-wrapped after a resize.
    fn rows_above_cursor(&self, width: usize) -> usize {
        let rows = |w: usize| w.max(1).div_ceil(width).max(1);
        let above: usize = self.drawn.iter().take(self.cursor.0).map(|w| rows(*w)).sum();
        above + self.cursor.1 / width
    }

    /// Redraw: print `committed` lines into scrollback, then draw `live` with the cursor at
    /// `cursor` (line, column) or hidden.
    pub fn draw(&mut self, committed: &[String], live: &[String], cursor: Option<(usize, usize)>) -> io::Result<()> {
        let (width, height) = size();
        let mut buf = String::new();
        buf.push_str("\u{1b}[?2026h");
        let up = self.rows_above_cursor(width);
        if up > 0 {
            buf.push_str(&format!("\u{1b}[{up}A"));
        }
        buf.push_str("\r\u{1b}[J");
        for line in committed {
            buf.push_str(line);
            buf.push_str("\u{1b}[0m\r\n");
        }

        let max = height.saturating_sub(1).max(1);
        let skip = live.len().saturating_sub(max);
        let live = &live[skip..];
        for (i, line) in live.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(line);
            buf.push_str("\u{1b}[0m");
        }

        let last = live.len().saturating_sub(1);
        let (row, col) = match cursor {
            Some((row, col)) if row >= skip => (row - skip, col),
            _ => (last, live.last().map(|l| visible_width(l)).unwrap_or(0)),
        };
        let row = row.min(last);
        if last > row {
            buf.push_str(&format!("\u{1b}[{}A", last - row));
        }
        buf.push('\r');
        let col = col.min(width.saturating_sub(1));
        if col > 0 {
            buf.push_str(&format!("\u{1b}[{col}C"));
        }
        buf.push_str(if cursor.is_some() { "\u{1b}[?25h" } else { "\u{1b}[?25l" });
        buf.push_str("\u{1b}[?2026l");

        let mut stdout = io::stdout().lock();
        stdout.write_all(buf.as_bytes())?;
        stdout.flush()?;
        self.drawn = live.iter().map(|l| visible_width(l)).collect();
        self.cursor = (row, col);
        Ok(())
    }

    /// Remove the live region, leaving the cursor at its start (before exit or a suspend).
    pub fn clear(&mut self) -> io::Result<()> {
        let up = self.rows_above_cursor(size().0);
        let mut stdout = io::stdout().lock();
        if up > 0 {
            write!(stdout, "\u{1b}[{up}A")?;
        }
        write!(stdout, "\r\u{1b}[J\u{1b}[?25h")?;
        stdout.flush()?;
        self.drawn.clear();
        self.cursor = (0, 0);
        Ok(())
    }
}
