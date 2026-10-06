//! Single-line text input shown in place of the editor (used by `/login`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::style::*;
use super::text::{truncate, visible_width};

pub enum PromptAction {
    None,
    Cancel,
    Submit(String),
}

pub struct TextPrompt {
    title: String,
    value: String,
    /// Show bullets instead of the value (for secrets).
    masked: bool,
    hint: String,
    /// Problem with the last submitted value, shown until the next edit.
    error: Option<String>,
}

impl TextPrompt {
    pub fn new(title: impl Into<String>, initial: &str, masked: bool, hint: impl Into<String>) -> TextPrompt {
        TextPrompt { title: title.into(), value: initial.to_string(), masked, hint: hint.into(), error: None }
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    /// Insert pasted text; line breaks and surrounding whitespace are dropped.
    pub fn paste(&mut self, text: &str) {
        self.value.push_str(text.trim().lines().map(str::trim).collect::<String>().as_str());
        self.error = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PromptAction::Cancel,
            KeyCode::Char('c') if ctrl => return PromptAction::Cancel,
            KeyCode::Enter => return PromptAction::Submit(self.value.trim().to_string()),
            KeyCode::Backspace => {
                self.value.pop();
            }
            KeyCode::Char('u') if ctrl => self.value.clear(),
            KeyCode::Char(c) if !ctrl => self.value.push(c),
            _ => return PromptAction::None,
        }
        self.error = None;
        PromptAction::None
    }

    /// Lines to draw and the cursor column on the input line (the second line).
    pub fn render(&self, width: usize) -> (Vec<String>, usize) {
        let shown = if self.masked { "•".repeat(self.value.chars().count()) } else { self.value.clone() };
        let prefix = "› ";
        // Keep the end of a long value visible, where typing happens.
        let room = width.saturating_sub(visible_width(prefix) + 1).max(1);
        let chars: Vec<char> = shown.chars().collect();
        let visible: String = if visible_width(&shown) > room {
            let mut start = chars.len();
            let mut used = 0;
            while start > 0 && used + visible_width(&chars[start - 1].to_string()) <= room.saturating_sub(1) {
                start -= 1;
                used += visible_width(&chars[start].to_string());
            }
            format!("…{}", chars[start..].iter().collect::<String>())
        } else {
            shown
        };
        let input = format!("{CYAN}{prefix}{RESET}{visible}");
        let cursor = visible_width(prefix) + visible_width(&visible);
        let mut lines = vec![format!("{BOLD}{}{RESET}", truncate(&self.title, width)), input];
        match &self.error {
            Some(error) => lines.push(paint(RED, &truncate(error, width))),
            None => lines.push(dim(&truncate(&self.hint, width))),
        }
        (lines, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_masks_and_submits() {
        let mut prompt = TextPrompt::new("API key", "", true, "hint");
        prompt.paste("  sk-12\n34  ");
        prompt.handle_key(key(KeyCode::Char('5')));
        prompt.handle_key(key(KeyCode::Backspace));
        let (lines, cursor) = prompt.render(40);
        assert!(lines[1].ends_with("•••••••"));
        assert!(!lines.concat().contains("sk-"));
        assert_eq!(cursor, 9);
        assert!(matches!(prompt.handle_key(key(KeyCode::Enter)), PromptAction::Submit(value) if value == "sk-1234"));
    }
}
