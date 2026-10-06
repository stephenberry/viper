//! Terminal styles. Uses the 16-color palette so output follows the user's terminal theme.

pub const RESET: &str = "\u{1b}[0m";
pub const BOLD: &str = "\u{1b}[1m";
pub const ITALIC: &str = "\u{1b}[3m";
pub const UNDERLINE: &str = "\u{1b}[4m";
pub const RED: &str = "\u{1b}[31m";
pub const GREEN: &str = "\u{1b}[32m";
pub const YELLOW: &str = "\u{1b}[33m";
pub const BLUE: &str = "\u{1b}[34m";
pub const MAGENTA: &str = "\u{1b}[35m";
pub const CYAN: &str = "\u{1b}[36m";
pub const GRAY: &str = "\u{1b}[90m";

pub fn paint(style: &str, text: &str) -> String {
    if text.is_empty() { String::new() } else { format!("{style}{text}{RESET}") }
}

pub fn dim(text: &str) -> String {
    paint(GRAY, text)
}

pub fn bold(text: &str) -> String {
    paint(BOLD, text)
}

/// Border color signalling the thinking level.
pub fn thinking_color(level: crate::config::ThinkingLevel) -> &'static str {
    use crate::config::ThinkingLevel::*;
    match level {
        Off => GRAY,
        Minimal | Low => BLUE,
        Medium => CYAN,
        High => GREEN,
        Xhigh => YELLOW,
        Max => MAGENTA,
    }
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
