//! Multi-line prompt editor with soft wrapping and history.

use unicode_segmentation::UnicodeSegmentation;

use super::text::char_width;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset of the cursor in `text`.
    cursor: usize,
    /// Preferred column for vertical movement.
    goal_col: Option<usize>,
    history: Vec<String>,
    /// Position while browsing history; `history.len()` means the draft.
    history_index: usize,
    draft: String,
}

/// A visual row: the byte range of `text` it shows and whether it starts a logical line.
#[derive(Debug, Clone, Copy)]
struct Row {
    start: usize,
    end: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.goal_col = None;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.goal_col = None;
        self.history_index = self.history.len();
        std::mem::take(&mut self.text)
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_index = self.history.len();
    }

    pub fn push_history(&mut self, entry: &str) {
        if entry.trim().is_empty() || self.history.last().is_some_and(|last| last == entry) {
            self.history_index = self.history.len();
            return;
        }
        self.history.push(entry.to_string());
        self.history_index = self.history.len();
    }

    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.goal_col = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert("\n");
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.text[..from].grapheme_indices(true).next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..].graphemes(true).next().map(|g| from + g.len()).unwrap_or(self.text.len())
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.prev_boundary(self.cursor);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.goal_col = None;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let end = self.next_boundary(self.cursor);
            self.text.replace_range(self.cursor..end, "");
        }
        self.goal_col = None;
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
        self.goal_col = None;
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
        self.goal_col = None;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
        self.goal_col = None;
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
        self.goal_col = None;
    }

    fn word_left_pos(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        trimmed.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len()
    }

    fn word_right_pos(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_').len();
        let rest = &after[skipped..];
        let word = rest.len() - rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').len();
        self.cursor + skipped + word
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
        self.goal_col = None;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_pos();
        self.goal_col = None;
    }

    pub fn delete_word_back(&mut self) {
        let start = self.word_left_pos();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.goal_col = None;
    }

    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start();
        let start = if start == self.cursor && start > 0 { start - 1 } else { start };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.goal_col = None;
    }

    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end();
        let end = if end == self.cursor && end < self.text.len() { end + 1 } else { end };
        self.text.replace_range(self.cursor..end, "");
        self.goal_col = None;
    }

    /// The character immediately before the cursor.
    pub fn char_before_cursor(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    /// The whitespace-delimited token ending at the cursor, and its start offset.
    pub fn token_before_cursor(&self) -> (usize, &str) {
        let before = &self.text[..self.cursor];
        let start =
            before.rfind(char::is_whitespace).map(|i| i + before[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
        (start, &before[start..])
    }

    /// Replace `start..cursor` with `replacement`.
    pub fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        self.text.replace_range(start..self.cursor, replacement);
        self.cursor = start + replacement.len();
        self.goal_col = None;
    }

    fn layout(&self, width: usize) -> Vec<Row> {
        let width = width.max(4);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut row_start = line_start;
            let mut col = 0;
            for (offset, grapheme) in line.grapheme_indices(true) {
                let w: usize = grapheme.chars().map(char_width).sum();
                if col + w > width && col > 0 {
                    rows.push(Row { start: row_start, end: line_start + offset });
                    row_start = line_start + offset;
                    col = 0;
                }
                col += w;
            }
            rows.push(Row { start: row_start, end: line_start + line.len() });
            line_start += line.len() + 1;
        }
        rows
    }

    fn width_of(&self, start: usize, end: usize) -> usize {
        self.text[start..end].chars().map(char_width).sum()
    }

    /// Cursor (row, column) for a given content width.
    pub fn cursor_position(&self, width: usize) -> (usize, usize) {
        let rows = self.layout(width);
        for (index, row) in rows.iter().enumerate() {
            let next_starts_here =
                rows.get(index + 1).is_some_and(|next| next.start == self.cursor && next.start == row.end);
            if self.cursor >= row.start && self.cursor <= row.end && !next_starts_here {
                return (index, self.width_of(row.start, self.cursor));
            }
        }
        (rows.len().saturating_sub(1), 0)
    }

    /// Visual rows of text for a given content width.
    pub fn rows(&self, width: usize) -> Vec<String> {
        self.layout(width).iter().map(|row| self.text[row.start..row.end].replace('\t', "    ")).collect()
    }

    /// Move up a visual row. Returns false when already on the first row.
    pub fn up(&mut self, width: usize) -> bool {
        self.vertical(width, -1)
    }

    pub fn down(&mut self, width: usize) -> bool {
        self.vertical(width, 1)
    }

    fn vertical(&mut self, width: usize, delta: isize) -> bool {
        let rows = self.layout(width);
        let (row, col) = self.cursor_position(width);
        let target = row as isize + delta;
        if target < 0 || target as usize >= rows.len() {
            return false;
        }
        let goal = *self.goal_col.get_or_insert(col);
        let target = rows[target as usize];
        let mut pos = target.start;
        let mut used = 0;
        for (offset, grapheme) in self.text[target.start..target.end].grapheme_indices(true) {
            let w: usize = grapheme.chars().map(char_width).sum();
            if used + w > goal {
                break;
            }
            used += w;
            pos = target.start + offset + grapheme.len();
        }
        self.cursor = pos;
        true
    }

    pub fn history_prev(&mut self) {
        if self.history_index == 0 {
            return;
        }
        if self.history_index == self.history.len() {
            self.draft = self.text.clone();
        }
        self.history_index -= 1;
        let entry = self.history[self.history_index].clone();
        self.set_text(&entry);
    }

    pub fn history_next(&mut self) {
        if self.history_index >= self.history.len() {
            return;
        }
        self.history_index += 1;
        let entry = if self.history_index == self.history.len() {
            self.draft.clone()
        } else {
            self.history[self.history_index].clone()
        };
        self.set_text(&entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_moves() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.word_left();
        e.insert("big ");
        assert_eq!(e.text(), "hello big world");
        e.delete_word_back();
        assert_eq!(e.text(), "hello world");
        e.end();
        e.backspace();
        assert_eq!(e.text(), "hello worl");
    }

    #[test]
    fn wraps_and_tracks_cursor() {
        let mut e = Editor::default();
        e.insert("abcdefgh\nxy");
        assert_eq!(e.rows(4), vec!["abcd", "efgh", "xy"]);
        assert_eq!(e.cursor_position(4), (2, 2));
        assert!(e.up(4));
        assert_eq!(e.cursor_position(4), (1, 2));
        assert!(e.up(4));
        assert!(!e.up(4));
        assert_eq!(e.cursor_position(4), (0, 2));
    }

    #[test]
    fn history_round_trip() {
        let mut e = Editor::default();
        e.set_history(vec!["one".into(), "two".into()]);
        e.insert("draft");
        e.history_prev();
        assert_eq!(e.text(), "two");
        e.history_prev();
        assert_eq!(e.text(), "one");
        e.history_next();
        e.history_next();
        assert_eq!(e.text(), "draft");
    }
}
