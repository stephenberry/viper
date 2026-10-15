//! Filterable list selector used for models, thinking levels, and sessions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::style::*;
use super::text::truncate;

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub label: String,
    pub detail: String,
    /// Extra text the filter matches but the list does not show.
    pub keywords: String,
}

pub enum SelectAction {
    None,
    Cancel,
    /// Index of the chosen item.
    Choose(usize),
}

pub struct Selector {
    pub title: String,
    items: Vec<Item>,
    filter: String,
    /// Indices of items matching the filter.
    visible: Vec<usize>,
    selected: usize,
    scroll: usize,
    /// Footer hint shown below the list.
    hint: String,
}

const MAX_ROWS: usize = 10;

impl Selector {
    pub fn new(title: impl Into<String>, items: Vec<Item>, initial: usize, hint: impl Into<String>) -> Selector {
        let mut selector = Selector {
            title: title.into(),
            items,
            filter: String::new(),
            visible: Vec::new(),
            selected: 0,
            scroll: 0,
            hint: hint.into(),
        };
        selector.refilter();
        selector.selected = selector.visible.iter().position(|i| *i == initial).unwrap_or(0);
        selector.fix_scroll();
        selector
    }

    /// Replace the items, keeping the filter and, while it is still listed, the selected item
    /// (recognized by its keywords and trimmed label, since column padding may change).
    pub fn set_items(&mut self, items: Vec<Item>) {
        let identity = |item: &Item| (item.label.trim_end().to_string(), item.keywords.clone());
        let selected = self.visible.get(self.selected).map(|index| identity(&self.items[*index]));
        self.items = items;
        self.refilter();
        if let Some(selected) = selected
            && let Some(row) = self.visible.iter().position(|index| identity(&self.items[*index]) == selected)
        {
            self.selected = row;
        }
        self.fix_scroll();
    }

    fn refilter(&mut self) {
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(str::to_string).collect();
        self.visible = (0..self.items.len())
            .filter(|i| {
                let item = &self.items[*i];
                let hay = format!("{} {} {}", item.label, item.detail, item.keywords).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.fix_scroll();
    }

    fn fix_scroll(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_ROWS {
            self.scroll = self.selected + 1 - MAX_ROWS;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SelectAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return SelectAction::Cancel,
            KeyCode::Char('c') if ctrl => return SelectAction::Cancel,
            KeyCode::Enter => {
                if let Some(index) = self.visible.get(self.selected) {
                    return SelectAction::Choose(*index);
                }
            }
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(self.visible.len().saturating_sub(1)),
            KeyCode::Char('p') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = if self.selected + 1 >= self.visible.len() { 0 } else { self.selected + 1 }
            }
            KeyCode::Char('n') if ctrl => self.selected = (self.selected + 1).min(self.visible.len().saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(MAX_ROWS),
            KeyCode::PageDown => self.selected = (self.selected + MAX_ROWS).min(self.visible.len().saturating_sub(1)),
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.selected = 0;
                self.refilter();
            }
            _ => {}
        }
        self.fix_scroll();
        SelectAction::None
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        let mut lines =
            vec![format!("{BOLD}{}{RESET}  {GRAY}filter:{RESET} {}{CYAN}▏{RESET}", self.title, self.filter)];
        if self.visible.is_empty() {
            lines.push(dim("  (no matches)"));
        }
        for (row, index) in self.visible.iter().enumerate().skip(self.scroll).take(MAX_ROWS) {
            let item = &self.items[*index];
            let selected = row == self.selected;
            let marker = if selected { format!("{CYAN}❯{RESET}") } else { " ".to_string() };
            let label = if selected { format!("{CYAN}{BOLD}{}{RESET}", item.label) } else { item.label.clone() };
            let line = if item.detail.is_empty() {
                format!("{marker} {label}")
            } else {
                format!("{marker} {label}  {GRAY}{}{RESET}", item.detail)
            };
            lines.push(truncate(&line, width));
        }
        if self.visible.len() > MAX_ROWS {
            lines.push(dim(&format!("  {}/{}", self.selected + 1, self.visible.len())));
        }
        lines.push(dim(&truncate(&self.hint, width)));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, keywords: &str) -> Item {
        Item { label: label.into(), keywords: keywords.into(), ..Default::default() }
    }

    fn selected(selector: &Selector) -> &str {
        &selector.items[selector.visible[selector.selected]].keywords
    }

    #[test]
    fn replacing_items_keeps_the_filter_and_selection() {
        let items = vec![item("Opus  ", "a/opus"), item("Sonnet", "a/sonnet"), item("Haiku ", "a/haiku")];
        let mut selector = Selector::new("Model", items, 0, "");
        selector.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(selected(&selector), "a/haiku");

        // New items arrive, and padding changes; the filter and the selected item stay.
        let items = vec![
            item("Opus", "a/opus"),
            item("Haiku Next", "b/haiku-next"),
            item("Sonnet", "a/sonnet"),
            item("Haiku", "a/haiku"),
        ];
        selector.set_items(items);
        assert_eq!(selector.filter, "k");
        assert_eq!(selected(&selector), "a/haiku");

        // A selected item that is gone leaves a valid selection.
        selector.set_items(vec![item("Opus", "a/opus"), item("Haiku Next", "b/haiku-next")]);
        assert_eq!(selected(&selector), "b/haiku-next");
    }
}
