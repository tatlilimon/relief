//! The list a person ticks items off in before anything is deleted.

use crate::render;
use crate::style::{Style, fit};

/// One line the user can tick.
#[derive(Clone, Debug)]
pub struct Row {
    /// `project`, `cache`, `prune` or `leftover`.
    pub category: &'static str,
    pub bytes: Option<u64>,
    /// The line as shown, without marker and checkbox.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
    Toggle,
    All,
    Nothing,
    Projects,
    Caches,
    Docker,
    Confirm,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// The user is done; these row indexes are ticked.
    Confirm(Vec<usize>),
    Cancel,
}

pub struct Picker {
    rows: Vec<Row>,
    ticked: Vec<bool>,
    cursor: usize,
}

impl Picker {
    pub fn new(rows: Vec<Row>) -> Picker {
        Picker {
            ticked: vec![false; rows.len()],
            rows,
            cursor: 0,
        }
    }

    pub fn press(&mut self, key: Key, page: usize) -> Step {
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(page),
            Key::PageDown => self.cursor = (self.cursor + page).min(last),
            Key::First => self.cursor = 0,
            Key::Last => self.cursor = last,
            Key::Toggle => {
                if let Some(tick) = self.ticked.get_mut(self.cursor) {
                    *tick = !*tick;
                }
            }
            Key::All => self.ticked.fill(true),
            Key::Nothing => self.ticked.fill(false),
            Key::Projects => self.toggle_group(&["project"]),
            Key::Caches => self.toggle_group(&["cache", "prune"]),
            Key::Docker => self.toggle_group(&["docker"]),
            Key::Confirm => return Step::Confirm(self.ticked()),
            Key::Cancel => return Step::Cancel,
        }
        Step::Continue
    }

    /// Tick every row of the group, or untick them when all are ticked already.
    fn toggle_group(&mut self, categories: &[&str]) {
        let members: Vec<usize> = (0..self.rows.len())
            .filter(|i| categories.contains(&self.rows[*i].category))
            .collect();
        let all = members.iter().all(|i| self.ticked[*i]);
        for i in members {
            self.ticked[i] = !all;
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Indexes of the ticked rows.
    pub fn ticked(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|i| self.ticked[*i]).collect()
    }

    /// The screen as lines: a title, as many rows as fit around the cursor, and a footer
    /// with the running total and the keys.
    pub fn frame(&self, height: usize, style: &Style) -> Vec<String> {
        let cut = |text: String| match style.width {
            Some(width) => fit(&text, width),
            None => text,
        };
        // Title, two scroll hints, total and keys take five lines.
        let room = height.saturating_sub(5).max(1);
        let top = (self.cursor + 1).saturating_sub(room);
        let end = (top + room).min(self.rows.len());

        let mut lines = vec![style.bold(&cut("Select what to delete".to_string()))];
        if top > 0 {
            lines.push(style.dim(&cut(format!("      ↑ {top} more above"))));
        }
        for i in top..end {
            let row = &self.rows[i];
            let here = i == self.cursor;
            // Marker and checkbox are text, so they survive without colour and are read
            // out by a screen reader.
            let marker = if here { ">" } else { " " };
            let tick = if self.ticked[i] { "[x]" } else { "[ ]" };
            let size = row.bytes.map_or_else(|| "?".to_string(), render::bytes);
            let line = cut(format!("{marker} {tick} {size:>8}  {}", row.text));
            lines.push(if here {
                style.accent(&style.bold(&line))
            } else if self.ticked[i] {
                style.good(&line)
            } else {
                line
            });
        }
        if end < self.rows.len() {
            lines.push(style.dim(&cut(format!(
                "      ↓ {} more below",
                self.rows.len() - end
            ))));
        }
        let chosen = self.ticked();
        let bytes: u64 = chosen.iter().filter_map(|i| self.rows[*i].bytes).sum();
        lines.push(style.bold(&cut(format!(
            "{} of {} selected  ·  {}",
            chosen.len(),
            self.rows.len(),
            render::bytes(bytes)
        ))));
        lines.push(style.dim(&cut(
            "↑↓ move · space select · a all · n none · p projects · c caches · d docker · enter delete · q cancel"
                .to_string(),
        )));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::visible_width;

    fn picker(categories: &[&'static str]) -> Picker {
        Picker::new(
            categories
                .iter()
                .enumerate()
                .map(|(i, category)| Row {
                    category,
                    bytes: Some(1024 * 1024 * (i as u64 + 1)),
                    text: format!("row-{i}"),
                })
                .collect(),
        )
    }

    fn press(p: &mut Picker, keys: &[Key]) -> Step {
        let mut last = Step::Continue;
        for key in keys {
            last = p.press(*key, 10);
        }
        last
    }

    #[test]
    fn nothing_is_ticked_until_the_user_ticks_it() {
        let mut p = picker(&["project"; 3]);
        assert_eq!(press(&mut p, &[Key::Confirm]), Step::Confirm(vec![]));
    }

    #[test]
    fn the_cursor_moves_and_stops_at_the_ends() {
        let mut p = picker(&["project"; 3]);
        press(&mut p, &[Key::Up]);
        assert_eq!(p.cursor(), 0);
        press(&mut p, &[Key::Down, Key::Down, Key::Down, Key::Down]);
        assert_eq!(p.cursor(), 2);
        press(&mut p, &[Key::First]);
        assert_eq!(p.cursor(), 0);
        press(&mut p, &[Key::Last]);
        assert_eq!(p.cursor(), 2);
    }

    #[test]
    fn pages_move_by_a_screenful() {
        let mut p = picker(&["project"; 40]);
        press(&mut p, &[Key::PageDown, Key::PageDown]);
        assert_eq!(p.cursor(), 20);
        press(&mut p, &[Key::PageUp]);
        assert_eq!(p.cursor(), 10);
        press(&mut p, &[Key::PageDown; 9]);
        assert_eq!(p.cursor(), 39);
    }

    #[test]
    fn toggle_ticks_and_unticks_the_row_under_the_cursor() {
        let mut p = picker(&["project"; 3]);
        press(&mut p, &[Key::Down, Key::Toggle, Key::Down, Key::Toggle]);
        assert_eq!(p.ticked(), [1, 2]);
        press(&mut p, &[Key::Toggle]);
        assert_eq!(press(&mut p, &[Key::Confirm]), Step::Confirm(vec![1]));
    }

    #[test]
    fn group_keys_tick_whole_categories() {
        let mut p = picker(&["leftover", "project", "project", "cache", "prune"]);
        press(&mut p, &[Key::Projects]);
        assert_eq!(p.ticked(), [1, 2]);
        press(&mut p, &[Key::Caches]);
        assert_eq!(p.ticked(), [1, 2, 3, 4]);
        press(&mut p, &[Key::Nothing]);
        assert_eq!(p.ticked(), Vec::<usize>::new());
        let mut d = picker(&["project", "docker", "docker"]);
        press(&mut d, &[Key::Docker]);
        assert_eq!(d.ticked(), [1, 2]);
        press(&mut p, &[Key::All]);
        assert_eq!(p.ticked(), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_group_key_unticks_a_group_that_is_already_fully_ticked() {
        let mut p = picker(&["project", "project", "cache"]);
        press(&mut p, &[Key::Projects, Key::Projects]);
        assert_eq!(p.ticked(), Vec::<usize>::new());
    }

    #[test]
    fn cancel_gives_up_whatever_was_ticked() {
        let mut p = picker(&["project"; 3]);
        assert_eq!(press(&mut p, &[Key::All, Key::Cancel]), Step::Cancel);
    }

    #[test]
    fn the_frame_marks_the_cursor_and_the_ticked_rows_in_words_not_only_colour() {
        let mut p = picker(&["project"; 3]);
        press(&mut p, &[Key::Toggle, Key::Down]);

        let frame = p.frame(20, &Style::PLAIN);

        let line = |needle: &str| frame.iter().find(|l| l.contains(needle)).unwrap().clone();
        assert!(line("row-0").contains("[x]"), "{frame:?}");
        assert!(line("row-1").contains("[ ]"), "{frame:?}");
        assert!(line("row-1").starts_with('>'), "{frame:?}");
        assert!(!line("row-0").starts_with('>'), "{frame:?}");
    }

    #[test]
    fn the_footer_totals_what_is_ticked() {
        let mut p = picker(&["project"; 3]);
        press(&mut p, &[Key::Toggle, Key::Last, Key::Toggle]);

        let frame = p.frame(20, &Style::PLAIN).join("\n");

        // 1 MB + 3 MB
        assert!(frame.contains("2 of 3 selected"), "{frame}");
        assert!(frame.contains("4.0 MB"), "{frame}");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_on_screen() {
        let mut p = picker(&["project"; 50]);
        press(&mut p, &[Key::Last]);

        let frame = p.frame(12, &Style::PLAIN);

        assert!(frame.len() <= 12, "{} lines", frame.len());
        assert!(frame.iter().any(|l| l.contains("row-49")));
        assert!(
            !frame
                .iter()
                .any(|l| l.contains("row-0 ") || l.ends_with("row-0"))
        );
        assert!(
            frame
                .iter()
                .any(|l| l.contains("50 of 50") || l.contains("more above")),
            "{frame:?}"
        );
    }

    #[test]
    fn rows_are_cut_to_the_terminal_width() {
        let p = Picker::new(vec![Row {
            category: "project",
            bytes: Some(1),
            text: "x".repeat(300),
        }]);
        let style = Style {
            color: true,
            width: Some(60),
        };

        for line in p.frame(20, &style) {
            assert!(
                visible_width(&line) <= 60,
                "{} columns: {line}",
                visible_width(&line)
            );
        }
    }
}
