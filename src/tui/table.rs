//! GitHub-flavored Markdown tables, laid out with box-drawing borders to fit the terminal width.
//!
//! Columns keep their natural width when the table fits. Otherwise the widest columns shrink and
//! their cells wrap, splitting words only when there is no room to keep them whole, and a table too
//! wide even for that is shown as its Markdown source.

use super::markdown::inline;
use super::style::*;
use super::text::{visible_width, wrap};

/// Narrowest a column with longer content is squeezed to, splitting its words, before the table
/// falls back to its source.
const MIN_COLUMN_WIDTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

pub struct Table {
    align: Vec<Align>,
    /// The header row, then the body rows, each with exactly one cell per column.
    rows: Vec<Vec<String>>,
    /// The source lines, shown when the table cannot fit.
    source: Vec<String>,
}

/// Whether `line` can be a table row. Rows must start with a pipe, so prose that merely contains
/// one is never held back as a possible header.
pub fn is_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

impl Table {
    /// Begin a table when `delimiter` is a delimiter row (`| --- | :-: |`) with one cell per cell
    /// of `header`. A delimiter without a pipe (`---`) is a heading underline or a rule instead.
    pub fn start(header: &str, delimiter: &str) -> Option<Table> {
        if !delimiter.contains('|') {
            return None;
        }
        let cells = split_row(header);
        let align: Vec<Align> = split_row(delimiter).iter().map(|cell| alignment(cell)).collect::<Option<_>>()?;
        (align.len() == cells.len()).then(|| Table {
            align,
            rows: vec![cells],
            source: vec![header.to_string(), delimiter.to_string()],
        })
    }

    /// Add `line` as a body row, or return false (leaving the table unchanged) if it is not one.
    /// Missing cells are left empty and extra cells are dropped.
    pub fn push_row(&mut self, line: &str) -> bool {
        if !is_row(line) {
            return false;
        }
        let mut cells = split_row(line);
        cells.resize(self.align.len(), String::new());
        self.rows.push(cells);
        self.source.push(line.to_string());
        true
    }

    /// Lines drawing the table in at most `width` columns.
    pub fn render(&self, width: usize) -> Vec<String> {
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                row.iter()
                    .map(|cell| if r == 0 { format!("{BOLD}{}{RESET}", inline(cell, BOLD)) } else { inline(cell, "") })
                    .collect()
            })
            .collect();
        let columns = self.align.len();
        let widest = |measure: fn(&str) -> usize| -> Vec<usize> {
            (0..columns).map(|c| cells.iter().map(|row| measure(&row[c])).max().unwrap_or(0)).collect()
        };
        let natural: Vec<usize> = widest(visible_width).into_iter().map(|w| w.max(1)).collect();
        let words = widest(|cell| cell.split(' ').map(visible_width).max().unwrap_or(0));
        // Each column adds a border and a space of padding on either side; one border closes the row.
        let Some(widths) = width.checked_sub(3 * columns + 1).and_then(|available| fit(&natural, &words, available))
        else {
            return self.source.iter().map(|line| inline(line.trim(), "")).collect();
        };

        let wrapped: Vec<Vec<Vec<String>>> =
            cells.iter().map(|row| row.iter().zip(&widths).map(|(cell, w)| wrap(cell, *w)).collect()).collect();
        // Once a body row spans several lines, rules between rows show where each one ends.
        let rule_between_rows = wrapped[1..].iter().any(|row| row.iter().any(|lines| lines.len() > 1));
        let rule = |left: &str, middle: &str, right: &str| {
            let segments: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            format!("{GRAY}{left}{}{right}{RESET}", segments.join(middle))
        };

        let mut out = vec![rule("┌", "┬", "┐")];
        for (r, row) in wrapped.iter().enumerate() {
            if r > 0 && (r == 1 || rule_between_rows) {
                out.push(rule("├", "┼", "┤"));
            }
            let height = row.iter().map(Vec::len).max().unwrap_or(1);
            for k in 0..height {
                let mut line = format!("{GRAY}│{RESET}");
                for (c, lines) in row.iter().enumerate() {
                    let text = lines.get(k).map(String::as_str).unwrap_or("");
                    line.push_str(&format!(" {} {GRAY}│{RESET}", pad(text, widths[c], self.align[c])));
                }
                out.push(line);
            }
        }
        out.push(rule("└", "┴", "┘"));
        out
    }
}

/// Split a row into trimmed cells on unescaped pipes, dropping the optional outer pipes and
/// unescaping `\|`.
fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    // The line is trimmed, so the cell after a closing pipe is empty rather than blank.
    if !cell.is_empty() || cells.is_empty() {
        cells.push(cell);
    }
    cells.into_iter().map(|cell| cell.trim().to_string()).collect()
}

/// The alignment a delimiter cell (`---`, `:--`, `:-:`, `--:`) sets, or None if it is not one.
fn alignment(cell: &str) -> Option<Align> {
    let left = cell.starts_with(':');
    let right = cell.len() > 1 && cell.ends_with(':');
    let dashes = &cell[usize::from(left)..cell.len() - usize::from(right)];
    if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

/// Column widths summing to at most `available`, or None when some column would be squeezed below
/// both its natural width and [`MIN_COLUMN_WIDTH`]. Columns grow evenly from a floor toward their
/// natural width. The floor is each column's longest word (`words`) when all of them fit, so words
/// stay whole, and otherwise the minimum width.
fn fit(natural: &[usize], words: &[usize], available: usize) -> Option<Vec<usize>> {
    let mut widths: Vec<usize> = if words.iter().sum::<usize>() <= available {
        words.to_vec()
    } else {
        natural.iter().map(|n| (*n).min(MIN_COLUMN_WIDTH)).collect()
    };
    let mut spare = available.checked_sub(widths.iter().sum())?;
    while spare > 0
        && let Some(c) = (0..widths.len()).filter(|c| widths[*c] < natural[*c]).min_by_key(|c| widths[*c])
    {
        widths[c] += 1;
        spare -= 1;
    }
    Some(widths)
}

/// Pad `text` with spaces to `width` columns according to `align`.
fn pad(text: &str, width: usize, align: Align) -> String {
    let gap = width.saturating_sub(visible_width(text));
    let left = match align {
        Align::Left => 0,
        Align::Center => gap / 2,
        Align::Right => gap,
    };
    format!("{}{text}{}", " ".repeat(left), " ".repeat(gap - left))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[String]) -> Vec<String> {
        lines.iter().map(|line| crate::util::strip_ansi(line)).collect()
    }

    fn table(lines: &[&str]) -> Table {
        let mut table = Table::start(lines[0], lines[1]).expect("a table");
        for line in &lines[2..] {
            assert!(table.push_row(line));
        }
        table
    }

    #[test]
    fn splits_cells() {
        assert_eq!(split_row("| a | b |"), ["a", "b"]);
        assert_eq!(split_row("|a|b"), ["a", "b"]);
        assert_eq!(split_row("| a | |"), ["a", ""]);
        assert_eq!(split_row(r"| a \| b | `c` |"), ["a | b", "`c`"]);
        assert_eq!(split_row("|"), [""]);
    }

    #[test]
    fn reads_alignment() {
        assert_eq!(alignment("---"), Some(Align::Left));
        assert_eq!(alignment(":--"), Some(Align::Left));
        assert_eq!(alignment(":-:"), Some(Align::Center));
        assert_eq!(alignment("--:"), Some(Align::Right));
        assert_eq!(alignment("-"), Some(Align::Left));
        assert_eq!(alignment(":"), None);
        assert_eq!(alignment("::"), None);
        assert_eq!(alignment("a-"), None);
        assert_eq!(alignment(""), None);
    }

    #[test]
    fn needs_a_matching_delimiter_row() {
        assert!(Table::start("| a | b |", "| --- | --- |").is_some());
        assert!(Table::start("| a | b |", "| --- |").is_none());
        assert!(Table::start("| a | b |", "| a | b |").is_none());
        assert!(Table::start("| a |", "| - |").is_some());
        assert!(Table::start("| a |", "---").is_none());
    }

    #[test]
    fn draws_aligned_columns() {
        let t = table(&["| Name | Qty |", "| --- | --: |", "| **apple** | 3 |", "| `kiwi` | 12 |", "| fig |"]);
        assert_eq!(
            plain(&t.render(80)),
            [
                "┌───────┬─────┐",
                "│ Name  │ Qty │",
                "├───────┼─────┤",
                "│ apple │   3 │",
                "│ kiwi  │  12 │",
                "│ fig   │     │",
                "└───────┴─────┘",
            ]
        );
    }

    #[test]
    fn wraps_cells_to_fit_the_width() {
        let t = table(&["| Key | Description |", "| :-: | --- |", "| a | one two three four five six |", "| b | x |"]);
        let lines = plain(&t.render(22));
        assert_eq!(
            lines,
            [
                "┌─────┬──────────────┐",
                "│ Key │ Description  │",
                "├─────┼──────────────┤",
                "│  a  │ one two      │",
                "│     │ three four   │",
                "│     │ five six     │",
                "├─────┼──────────────┤",
                "│  b  │ x            │",
                "└─────┴──────────────┘",
            ]
        );
        assert!(t.render(22).iter().all(|line| visible_width(line) <= 22));
    }

    #[test]
    fn falls_back_to_source_when_too_narrow() {
        let t = table(&["| aaaaaaaa | bbbbbbbb | cccccccc |", "|---|---|---|"]);
        assert_eq!(plain(&t.render(24)), ["| aaaaaaaa | bbbbbbbb | cccccccc |", "|---|---|---|"]);
    }

    #[test]
    fn shares_space_among_wide_columns() {
        assert_eq!(fit(&[3, 40, 40], &[3, 5, 5], 50), Some(vec![3, 24, 23]));
        assert_eq!(fit(&[3, 10, 40], &[3, 5, 5], 50), Some(vec![3, 10, 37]));
        assert_eq!(fit(&[3, 4], &[3, 4], 50), Some(vec![3, 4]));
    }

    #[test]
    fn keeps_words_whole_when_there_is_room() {
        assert_eq!(fit(&[30, 30], &[4, 20], 30), Some(vec![10, 20]));
        // Without room for every longest word, columns shrink evenly and words split.
        assert_eq!(fit(&[30, 30], &[4, 28], 30), Some(vec![15, 15]));
        assert_eq!(fit(&[1, 20, 20], &[1, 20, 20], 12), None);
        assert_eq!(fit(&[1, 20, 20], &[1, 20, 20], 13), Some(vec![1, 6, 6]));
    }
}
