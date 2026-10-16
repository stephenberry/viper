//! Multi-line prompt editor with soft wrapping, scrolling, history, and collapsed pastes.

use std::cell::Cell;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use super::style::{CYAN, RESET};
use super::text::char_width;

/// Pastes with more lines or characters than these are collapsed into a placeholder.
const COLLAPSE_LINES: usize = 10;
const COLLAPSE_CHARS: usize = 1000;

/// Whether pasted text is large enough to be collapsed into a placeholder.
pub fn should_collapse(text: &str) -> bool {
    text.lines().nth(COLLAPSE_LINES).is_some() || text.chars().nth(COLLAPSE_CHARS).is_some()
}

/// The placeholder shown for collapsed pasted text, in the editor and the transcript.
pub fn paste_label(content: &str) -> String {
    match content.lines().count() {
        0 | 1 => format!("[pasted {} chars]", content.chars().count()),
        lines => format!("[pasted {lines} lines]"),
    }
}

/// Pasted text shown as a placeholder label until it is submitted or expanded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paste {
    /// Byte range of the label in the draft's text.
    pub range: Range<usize>,
    pub content: String,
}

/// The editor's contents: text with a label for each collapsed paste, and the pastes in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pastes: Vec<Paste>,
}

impl Draft {
    pub fn plain(text: impl Into<String>) -> Draft {
        Draft { text: text.into(), pastes: Vec::new() }
    }

    /// The text with each label replaced by its paste, and the byte range each paste occupies.
    pub fn expand(&self) -> (String, Vec<Range<usize>>) {
        let size = self.text.len() + self.pastes.iter().map(|paste| paste.content.len()).sum::<usize>();
        let mut text = String::with_capacity(size);
        let mut spans = Vec::with_capacity(self.pastes.len());
        let mut last = 0;
        for paste in &self.pastes {
            text.push_str(&self.text[last..paste.range.start]);
            let start = text.len();
            text.push_str(&paste.content);
            spans.push(start..text.len());
            last = paste.range.end;
        }
        text.push_str(&self.text[last..]);
        (text, spans)
    }

    pub fn expanded(&self) -> String {
        self.expand().0
    }

    /// Drop trailing whitespace. Labels end in `]`, so the pastes are unaffected.
    pub fn trim_end(&mut self) {
        self.text.truncate(self.text.trim_end().len());
    }

    /// The expanded text without trailing whitespace, as it is sent, and where each paste is in it.
    pub fn expand_trimmed(&self) -> (String, Vec<Range<usize>>) {
        let (mut text, mut spans) = self.expand();
        text.truncate(text.trim_end().len());
        for span in &mut spans {
            span.end = span.end.min(text.len());
        }
        spans.retain(|span| !span.is_empty());
        (text, spans)
    }

    /// The inverse of [`Draft::expand`]: `text` with each span collapsed to a label. Spans that do
    /// not fit the text are left expanded.
    pub fn collapse(text: &str, spans: &[Range<usize>]) -> Draft {
        let mut draft = Draft::default();
        let mut last = 0;
        for span in spans {
            let Some(content) = text.get(span.clone()).filter(|_| span.start >= last && !span.is_empty()) else {
                continue;
            };
            draft.text.push_str(&text[last..span.start]);
            let start = draft.text.len();
            draft.text.push_str(&paste_label(content));
            draft.pastes.push(Paste { range: start..draft.text.len(), content: content.to_string() });
            last = span.end;
        }
        draft.text.push_str(&text[last..]);
        draft
    }

    /// Append `other`, keeping its pastes collapsed.
    pub fn append(&mut self, other: Draft) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        self.pastes.extend(other.pastes.into_iter().map(|paste| Paste {
            range: paste.range.start + offset..paste.range.end + offset,
            content: paste.content,
        }));
    }

    /// Whether the pastes are in order, without overlap, and on character boundaries of the text.
    /// Drafts read back from disk are checked before use.
    pub fn is_consistent(&self) -> bool {
        let mut last = 0;
        self.pastes.iter().all(|paste| {
            let fits =
                paste.range.start >= last && !paste.range.is_empty() && self.text.get(paste.range.clone()).is_some();
            last = paste.range.end;
            fits
        })
    }
}

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset of the cursor in `text`; never inside a paste's label.
    cursor: usize,
    /// Collapsed pastes, ordered by position. Each is edited as a single unit.
    pastes: Vec<Paste>,
    /// Preferred column for vertical movement.
    goal_col: Option<usize>,
    history: Vec<Draft>,
    /// Position while browsing history; `history.len()` means the draft.
    history_index: usize,
    draft: Draft,
    /// First visual row shown when the text is taller than the view. Kept between renders so the
    /// view only scrolls when the cursor would leave it.
    scroll: Cell<usize>,
}

/// A visual row: the byte range of `text` it shows.
#[derive(Debug, Clone, Copy)]
struct Row {
    start: usize,
    end: usize,
}

/// The rows of the editor that fit the space given to it.
pub struct View {
    pub rows: Vec<String>,
    /// Cursor (row within `rows`, column).
    pub cursor: (usize, usize),
    /// Visual rows scrolled out of view above and below.
    pub above: usize,
    pub below: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor in the text.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// A copy of the contents, pastes included.
    pub fn snapshot(&self) -> Draft {
        Draft { text: self.text.clone(), pastes: self.pastes.clone() }
    }

    pub fn set_text(&mut self, text: &str) {
        self.set_draft(Draft::plain(text));
    }

    pub fn set_draft(&mut self, draft: Draft) {
        self.text = draft.text;
        self.pastes = draft.pastes;
        self.cursor = self.text.len();
        self.goal_col = None;
    }

    pub fn take(&mut self) -> Draft {
        self.cursor = 0;
        self.goal_col = None;
        self.history_index = self.history.len();
        Draft { text: std::mem::take(&mut self.text), pastes: std::mem::take(&mut self.pastes) }
    }

    pub fn set_history(&mut self, history: Vec<Draft>) {
        self.history = history;
        self.history_index = self.history.len();
    }

    pub fn push_history(&mut self, entry: &Draft) {
        if entry.text.trim().is_empty() || self.history.last() == Some(entry) {
            self.history_index = self.history.len();
            return;
        }
        self.history.push(entry.clone());
        self.history_index = self.history.len();
    }

    /// Replace `range` of the text with `with`. A paste the range overlaps is removed whole, and an
    /// insertion inside a label goes after it. Returns where `with` starts.
    fn splice(&mut self, range: Range<usize>, with: &str) -> usize {
        let (mut start, mut end) = (range.start, range.end);
        if start == end {
            start = self.snap(start, true);
            end = start;
        }
        for paste in &self.pastes {
            if paste.range.start < end && start < paste.range.end {
                start = start.min(paste.range.start);
                end = end.max(paste.range.end);
            }
        }
        self.pastes.retain(|paste| paste.range.end <= start || paste.range.start >= end);
        let (removed, added) = (end - start, with.len());
        for paste in self.pastes.iter_mut().filter(|paste| paste.range.start >= end) {
            paste.range = paste.range.start + added - removed..paste.range.end + added - removed;
        }
        self.text.replace_range(start..end, with);
        start
    }

    /// Move `pos` out of a label, to its end when `forward` and its start otherwise.
    fn snap(&self, pos: usize, forward: bool) -> usize {
        match self.pastes.iter().find(|paste| paste.range.start < pos && pos < paste.range.end) {
            Some(paste) if forward => paste.range.end,
            Some(paste) => paste.range.start,
            None => pos,
        }
    }

    pub fn insert(&mut self, s: &str) {
        let at = self.splice(self.cursor..self.cursor, s);
        self.cursor = at + s.len();
        self.goal_col = None;
    }

    /// Insert pasted text at the cursor as a placeholder; it is expanded when the draft is sent.
    pub fn insert_paste(&mut self, content: String) {
        let label = paste_label(&content);
        let at = self.splice(self.cursor..self.cursor, &label);
        let index = self.pastes.partition_point(|paste| paste.range.start < at);
        self.pastes.insert(index, Paste { range: at..at + label.len(), content });
        self.cursor = at + label.len();
        self.goal_col = None;
    }

    /// Replace the placeholder beside the cursor, or the only one, with its text. Returns false
    /// when there is none to expand.
    pub fn expand_paste(&mut self) -> bool {
        let index = self
            .pastes
            .iter()
            .position(|paste| paste.range.end == self.cursor)
            .or_else(|| self.pastes.iter().position(|paste| paste.range.start == self.cursor))
            .or_else(|| (self.pastes.len() == 1).then_some(0));
        let Some(index) = index else { return false };
        let paste = self.pastes.remove(index);
        let at = self.splice(paste.range, &paste.content);
        self.cursor = at + paste.content.len();
        self.goal_col = None;
        true
    }

    pub fn has_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }

    /// Insert `draft` at the start of the text, leaving the cursor at the end.
    pub fn prepend(&mut self, draft: Draft) {
        self.splice(0..0, &draft.text);
        self.pastes.splice(0..0, draft.pastes);
        self.cursor = self.text.len();
        self.goal_col = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert("\n");
    }

    // A label is longer than one grapheme, so a step from its edge lands inside and snaps across.
    fn prev_boundary(&self, from: usize) -> usize {
        self.snap(self.text[..from].grapheme_indices(true).next_back().map(|(i, _)| i).unwrap_or(0), false)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.snap(self.text[from..].graphemes(true).next().map(|g| from + g.len()).unwrap_or(self.text.len()), true)
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.prev_boundary(self.cursor);
            self.cursor = self.splice(start..self.cursor, "");
        }
        self.goal_col = None;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let end = self.next_boundary(self.cursor);
            self.cursor = self.splice(self.cursor..end, "");
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

    // Labels never contain a line break, so line starts and ends are never inside one.
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
        self.snap(trimmed.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len(), false)
    }

    fn word_right_pos(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_').len();
        let rest = &after[skipped..];
        let word = rest.len() - rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').len();
        self.snap(self.cursor + skipped + word, true)
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
        self.cursor = self.splice(start..self.cursor, "");
        self.goal_col = None;
    }

    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start();
        let start = if start == self.cursor && start > 0 { start - 1 } else { start };
        self.cursor = self.splice(start..self.cursor, "");
        self.goal_col = None;
    }

    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end();
        let end = if end == self.cursor && end < self.text.len() { end + 1 } else { end };
        self.cursor = self.splice(self.cursor..end, "");
        self.goal_col = None;
    }

    /// The character immediately before the cursor.
    pub fn char_before_cursor(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    /// The whitespace-delimited token ending at the cursor, and its start offset. A label is not
    /// part of a token, so completing one never touches a paste.
    pub fn token_before_cursor(&self) -> (usize, &str) {
        let before = &self.text[..self.cursor];
        let start =
            before.rfind(char::is_whitespace).map(|i| i + before[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
        // Every label contains a space, so a start inside one means the token begins after it.
        let start = self.snap(start, true);
        (start, &before[start..])
    }

    /// Replace `start..cursor` with `replacement`.
    pub fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        let at = self.splice(start..self.cursor, replacement);
        self.cursor = at + replacement.len();
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

    /// Cursor (row, column) within `rows`.
    fn locate(&self, rows: &[Row]) -> (usize, usize) {
        for (index, row) in rows.iter().enumerate() {
            let next_starts_here =
                rows.get(index + 1).is_some_and(|next| next.start == self.cursor && next.start == row.end);
            if self.cursor >= row.start && self.cursor <= row.end && !next_starts_here {
                return (index, self.width_of(row.start, self.cursor));
            }
        }
        (rows.len().saturating_sub(1), 0)
    }

    /// A visual row as displayed, with labels highlighted.
    fn render_row(&self, row: Row) -> String {
        let mut out = String::new();
        let mut pos = row.start;
        for paste in self.pastes.iter().filter(|paste| paste.range.start < row.end && row.start < paste.range.end) {
            let (start, end) = (paste.range.start.max(row.start), paste.range.end.min(row.end));
            out.push_str(&self.text[pos..start].replace('\t', "    "));
            out.push_str(CYAN);
            out.push_str(&self.text[start..end]);
            out.push_str(RESET);
            pos = end;
        }
        out.push_str(&self.text[pos..row.end].replace('\t', "    "));
        out
    }

    /// The visual rows for a given content width, at most `height` of them, scrolled to keep the
    /// cursor in view.
    pub fn view(&self, width: usize, height: usize) -> View {
        let rows = self.layout(width);
        let (row, col) = self.locate(&rows);
        let height = height.max(1);
        let mut scroll = self.scroll.get().min(rows.len().saturating_sub(height));
        if row < scroll {
            scroll = row;
        } else if row >= scroll + height {
            scroll = row + 1 - height;
        }
        self.scroll.set(scroll);
        let end = (scroll + height).min(rows.len());
        View {
            rows: rows[scroll..end].iter().map(|&row| self.render_row(row)).collect(),
            cursor: (row - scroll, col),
            above: scroll,
            below: rows.len() - end,
        }
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
        let (row, col) = self.locate(&rows);
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
        // Step past a label in the direction of travel, so the cursor always leaves its row.
        self.cursor = self.snap(pos, delta > 0);
        true
    }

    pub fn history_prev(&mut self) {
        if self.history_index == 0 {
            return;
        }
        if self.history_index == self.history.len() {
            self.draft = self.snapshot();
        }
        self.history_index -= 1;
        let entry = self.history[self.history_index].clone();
        self.set_draft(entry);
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
        self.set_draft(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(count: usize) -> String {
        (1..=count).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

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
        let view = e.view(4, 10);
        assert_eq!(view.rows, vec!["abcd", "efgh", "xy"]);
        assert_eq!(view.cursor, (2, 2));
        assert!(e.up(4));
        assert_eq!(e.view(4, 10).cursor, (1, 2));
        assert!(e.up(4));
        assert!(!e.up(4));
        assert_eq!(e.view(4, 10).cursor, (0, 2));
    }

    #[test]
    fn view_scrolls_to_the_cursor() {
        let mut e = Editor::default();
        e.insert(&lines(10));
        let view = e.view(20, 3);
        assert_eq!(view.rows, vec!["line 8", "line 9", "line 10"]);
        assert_eq!((view.above, view.below, view.cursor), (7, 0, (2, 7)));
        for _ in 0..4 {
            e.up(20);
        }
        let view = e.view(20, 3);
        assert_eq!(view.rows, vec!["line 6", "line 7", "line 8"]);
        assert_eq!((view.above, view.below, view.cursor.0), (5, 2, 0));
        // Moving within the view does not scroll it.
        e.down(20);
        assert_eq!(e.view(20, 3).above, 5);
        // A shorter text pulls the view back.
        e.take();
        e.insert("one");
        let view = e.view(20, 3);
        assert_eq!((view.rows.len(), view.above, view.below), (1, 0, 0));
    }

    #[test]
    fn large_pastes_collapse() {
        assert!(!should_collapse(&lines(10)));
        assert!(should_collapse(&lines(11)));
        assert!(should_collapse(&"x".repeat(1001)));
        assert_eq!(paste_label(&lines(11)), "[pasted 11 lines]");
        assert_eq!(paste_label(&"x".repeat(1001)), "[pasted 1001 chars]");
    }

    #[test]
    fn pastes_are_edited_as_one_unit() {
        let pasted = lines(20);
        let mut e = Editor::default();
        e.insert("see ");
        e.insert_paste(pasted.clone());
        e.insert(" ok");
        assert_eq!(e.text(), "see [pasted 20 lines] ok");
        let (expanded, spans) = e.snapshot().expand();
        assert_eq!(expanded, format!("see {pasted} ok"));
        assert_eq!(spans, vec![4..4 + pasted.len()]);

        // Left steps over the whole label; typing there lands before it.
        e.end();
        e.left();
        e.left();
        e.left();
        e.left();
        assert_eq!(e.cursor(), 4);
        e.insert("<");
        assert_eq!(e.text(), "see <[pasted 20 lines] ok");
        e.right();
        e.backspace();
        assert_eq!(e.text(), "see < ok");
        assert!(e.snapshot().pastes.is_empty());

        e.set_text("a ");
        e.insert_paste(pasted.clone());
        e.delete_word_back();
        assert_eq!(e.text(), "a ");
        e.insert_paste(pasted.clone());
        e.delete_to_line_start();
        assert!(e.is_empty() && e.snapshot().pastes.is_empty());
    }

    #[test]
    fn vertical_moves_skip_labels() {
        let mut e = Editor::default();
        e.insert_paste(lines(20));
        e.insert("\n0123456789012");
        assert!(e.up(40));
        assert_eq!(e.cursor(), 0, "moving up lands before the label");
        assert!(e.down(40));
        e.home();
        assert!(e.up(40));
        assert!(e.down(40));
        assert_eq!(e.view(40, 5).cursor.0, 1);

        // A label split across rows does not trap the cursor.
        let mut e = Editor::default();
        e.insert(&format!("top\n{}", "x".repeat(66)));
        e.insert_paste(lines(20));
        e.insert(&"y".repeat(72));
        let mut moves = 0;
        while e.up(80) {
            moves += 1;
        }
        assert_eq!((moves, e.view(80, 5).cursor.0), (2, 0));
    }

    #[test]
    fn expanding_a_paste_makes_it_text() {
        let mut e = Editor::default();
        e.insert("> ");
        e.insert_paste("x".repeat(1200));
        e.insert(" <");
        assert!(e.expand_paste(), "the only paste expands from anywhere");
        assert_eq!(e.text(), format!("> {} <", "x".repeat(1200)));
        assert_eq!(e.cursor(), 2 + 1200);
        assert!(!e.expand_paste());
    }

    #[test]
    fn pastes_shift_with_edits_before_them() {
        let mut e = Editor::default();
        e.insert_paste(lines(12));
        e.insert(" and ");
        e.insert_paste(lines(15));
        let first = format!("{}\n\n", lines(11));
        let span = 0..first.len() - 2;
        e.prepend(Draft::collapse(&first, &[span]));
        e.home();
        e.insert("> ");
        let draft = e.snapshot();
        assert!(draft.is_consistent());
        assert_eq!(draft.text, "[pasted 11 lines]\n\n> [pasted 12 lines] and [pasted 15 lines]");
        assert_eq!(draft.expanded(), format!("{first}> {} and {}", lines(12), lines(15)));
    }

    #[test]
    fn completion_tokens_stop_at_labels() {
        let mut e = Editor::default();
        e.insert_paste(lines(12));
        assert_eq!(e.token_before_cursor(), (e.cursor(), ""));
        e.insert("@src");
        assert_eq!(e.token_before_cursor().1, "@src");
    }

    #[test]
    fn cursor_stays_out_of_labels_beside_combining_marks() {
        let mut e = Editor::default();
        e.insert_paste(lines(20));
        e.insert("\u{301}x");
        e.left();
        e.left();
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn drafts_collapse_and_join() {
        let pasted = lines(12);
        let text = format!("a {pasted} b");
        let span = 2..2 + pasted.len();
        let draft = Draft::collapse(&text, &[span]);
        assert_eq!(draft.text, "a [pasted 12 lines] b");
        assert_eq!(draft.expanded(), text);
        let mut joined = Draft::plain("x\n\n");
        joined.append(draft);
        assert!(joined.is_consistent());
        assert_eq!(joined.expanded(), format!("x\n\n{text}"));
        // Spans that do not fit stay expanded.
        let out_of_bounds = 2..99;
        assert_eq!(Draft::collapse("short", &[out_of_bounds]), Draft::plain("short"));
    }

    #[test]
    fn sent_text_is_trimmed() {
        let mut e = Editor::default();
        e.insert("see ");
        e.insert_paste("error\n".repeat(20));
        let (text, spans) = e.snapshot().expand_trimmed();
        assert!(text.ends_with("error"));
        assert_eq!(spans, vec![4..text.len()]);
    }

    #[test]
    fn rows_highlight_labels() {
        let mut e = Editor::default();
        e.insert("a ");
        e.insert_paste(lines(12));
        assert_eq!(e.view(40, 5).rows, vec![format!("a {CYAN}[pasted 12 lines]{RESET}")]);
    }

    #[test]
    fn history_round_trip() {
        let mut e = Editor::default();
        e.set_history(vec![Draft::plain("one"), Draft::plain("two")]);
        e.insert("draft");
        e.insert_paste(lines(12));
        e.history_prev();
        assert_eq!(e.text(), "two");
        e.history_prev();
        assert_eq!(e.text(), "one");
        e.history_next();
        e.history_next();
        assert_eq!(e.snapshot().expanded(), format!("draft{}", lines(12)));
    }
}
