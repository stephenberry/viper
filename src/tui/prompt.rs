//! Single-line text input shown in place of the editor (used by `/login`).

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use super::editor::Editor;
use super::style::*;
use super::text::{char_width, truncate, visible_width};

pub enum PromptAction {
    None,
    Cancel,
    Submit(String),
}

pub struct TextPrompt {
    title: String,
    input: Editor,
    /// Show bullets instead of the value (for secrets).
    masked: bool,
    hint: String,
    /// Problem with the last submitted value, shown until the next edit.
    error: Option<String>,
    /// Index of the first grapheme shown when the value is wider than the line. Kept between
    /// renders so the view only scrolls when the cursor would leave it.
    scroll: Cell<usize>,
}

impl TextPrompt {
    pub fn new(title: impl Into<String>, initial: &str, masked: bool, hint: impl Into<String>) -> TextPrompt {
        let mut input = Editor::default();
        input.set_text(&single_line(initial));
        TextPrompt { title: title.into(), input, masked, hint: hint.into(), error: None, scroll: Cell::new(0) }
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    /// Insert pasted text at the cursor; line breaks and surrounding whitespace are dropped.
    pub fn paste(&mut self, text: &str) {
        self.input.insert(&single_line(text));
        self.error = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let input = &mut self.input;
        match key.code {
            KeyCode::Esc => return PromptAction::Cancel,
            KeyCode::Char('c') if ctrl => return PromptAction::Cancel,
            KeyCode::Enter => return PromptAction::Submit(input.text().trim().to_string()),
            KeyCode::Left if alt || ctrl => input.word_left(),
            KeyCode::Right if alt || ctrl => input.word_right(),
            KeyCode::Char('b') if alt => input.word_left(),
            KeyCode::Char('f') if alt => input.word_right(),
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Home => input.home(),
            KeyCode::End => input.end(),
            KeyCode::Char('a') if ctrl => input.home(),
            KeyCode::Char('e') if ctrl => input.end(),
            // Edits clear the error; moving the cursor leaves it shown.
            _ => {
                match key.code {
                    KeyCode::Char('w') if ctrl => input.delete_word_back(),
                    KeyCode::Backspace if alt || ctrl => input.delete_word_back(),
                    KeyCode::Char('u') if ctrl => input.delete_to_line_start(),
                    KeyCode::Char('k') if ctrl => input.delete_to_line_end(),
                    KeyCode::Backspace => input.backspace(),
                    KeyCode::Delete => input.delete(),
                    KeyCode::Char(c) if !ctrl && !c.is_control() => {
                        let mut buf = [0u8; 4];
                        input.insert(c.encode_utf8(&mut buf));
                    }
                    _ => return PromptAction::None,
                }
                self.error = None;
            }
        }
        PromptAction::None
    }

    /// Lines to draw and the cursor column on the input line (the second line).
    pub fn render(&self, width: usize) -> (Vec<String>, usize) {
        let text = self.input.text();
        let cursor = text[..self.input.cursor()].graphemes(true).count();
        let cells: Vec<(&str, usize)> = text
            .graphemes(true)
            .map(|g| if self.masked { ("•", 1) } else { (g, g.chars().map(char_width).sum()) })
            .collect();

        let prefix = "› ";
        // One column past the text is kept free for the cursor.
        let room = width.saturating_sub(visible_width(prefix) + 1).max(1);
        let (start, end) = viewport(&cells, cursor, room, self.scroll.get());
        self.scroll.set(start);

        let left = if start > 0 { "…" } else { "" };
        let right = if end < cells.len() { "…" } else { "" };
        let shown: String = cells[start..end].iter().map(|(g, _)| *g).collect();
        let column = visible_width(prefix)
            + visible_width(left)
            + cells[start..cursor.max(start)].iter().map(|(_, w)| w).sum::<usize>();

        let input = format!("{CYAN}{prefix}{RESET}{left}{shown}{right}");
        let mut lines = vec![format!("{BOLD}{}{RESET}", truncate(&self.title, width)), input];
        match &self.error {
            Some(error) => lines.push(paint(RED, &truncate(error, width))),
            None => lines.push(dim(&truncate(&self.hint, width))),
        }
        (lines, column)
    }
}

/// Collapse text to one line: trim each line and join them, dropping other control characters.
fn single_line(text: &str) -> String {
    text.trim().lines().map(str::trim).collect::<String>().chars().filter(|c| !c.is_control()).collect()
}

/// The range of cells to show in `room` columns so the cursor stays visible, moving as little as
/// possible from the previous first cell. An ellipsis takes one column on each clipped side.
fn viewport(cells: &[(&str, usize)], cursor: usize, room: usize, previous: usize) -> (usize, usize) {
    let len = cells.len();
    let fits = |start: usize, end: usize| {
        let ellipses = usize::from(start > 0) + usize::from(end < len);
        cells[start..end].iter().map(|(_, w)| w).sum::<usize>() + ellipses <= room
    };
    if fits(0, len) {
        return (0, len);
    }
    // The cell under the cursor must show, unless the cursor is past the end.
    let needed = (cursor + 1).min(len);
    let mut start = previous.min(cursor);
    while start < cursor && !fits(start, needed) {
        start += 1;
    }
    let mut end = needed;
    while end < len && fits(start, end + 1) {
        end += 1;
    }
    // Fill space freed at the end, e.g. after deleting.
    while start > 0 && fits(start - 1, end) {
        start -= 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn submit(prompt: &mut TextPrompt) -> String {
        match prompt.handle_key(key(KeyCode::Enter)) {
            PromptAction::Submit(value) => value,
            _ => panic!("enter did not submit"),
        }
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
        assert_eq!(submit(&mut prompt), "sk-1234");
    }

    #[test]
    fn moves_the_cursor_to_edit_mid_value() {
        let mut prompt = TextPrompt::new("Base URL", "https://exmple.com", false, "hint");
        for _ in 0.."mple.com".len() {
            prompt.handle_key(key(KeyCode::Left));
        }
        prompt.handle_key(key(KeyCode::Char('a')));
        let (_, cursor) = prompt.render(40);
        assert_eq!(cursor, 2 + "https://exa".len());
        prompt.handle_key(key(KeyCode::Home));
        prompt.handle_key(key(KeyCode::Delete));
        prompt.handle_key(key(KeyCode::End));
        prompt.handle_key(key(KeyCode::Backspace));
        assert_eq!(submit(&mut prompt), "ttps://example.co");
    }

    #[test]
    fn scrolls_a_long_value_to_keep_the_cursor_visible() {
        let mut prompt = TextPrompt::new("Base URL", "abcdefghijklmnopqrstuvwxyz", false, "hint");
        // 13 columns: "› " plus 10 for the text and 1 for the cursor.
        let (lines, cursor) = prompt.render(13);
        assert!(lines[1].ends_with("…rstuvwxyz"));
        assert_eq!(cursor, 12);
        prompt.handle_key(key(KeyCode::Home));
        let (lines, cursor) = prompt.render(13);
        assert!(lines[1].ends_with("abcdefghi…"));
        assert_eq!(cursor, 2);
        // Moving right within the view does not scroll it.
        prompt.handle_key(key(KeyCode::Right));
        let (lines, cursor) = prompt.render(13);
        assert!(lines[1].ends_with("abcdefghi…"));
        assert_eq!(cursor, 3);
    }
}
