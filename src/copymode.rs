//! Copy mode for terminal and AI panes (Ctrl-b [), like tmux: the pane's scrollback
//! and screen freeze, a cursor moves over them, `v` / `V` select and `y` copies to
//! the system clipboard. Lines the terminal soft-wrapped are copied as one line.
use crate::terminal::Row;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub struct CopyMode {
    /// The pane being copied from.
    pub pane: usize,
    pub rows: Vec<Row>,
    pub row: usize,
    /// Cell column; wide characters take two.
    pub col: usize,
    /// First row on screen.
    pub top: usize,
    /// Rows on screen, recorded at draw time for paging.
    pub height: usize,
    /// Selection start; the cursor is the other end.
    pub anchor: Option<(usize, usize)>,
    /// `V`: whole lines.
    pub lines: bool,
    /// A search being typed after `/` or `?`.
    pub typing: Option<String>,
    search: Option<(String, bool)>,
}

pub enum Outcome {
    Stay,
    Exit,
    /// Copy this text, then leave copy mode.
    Copy(String),
    /// Nothing matched this search.
    NotFound(String),
}

impl CopyMode {
    /// Start on the terminal's cursor row, showing what the pane showed.
    pub fn new(pane: usize, rows: Vec<Row>, cursor: usize, top: usize) -> Self {
        // Unused screen below the last text and the cursor isn't worth visiting.
        let last = rows
            .iter()
            .rposition(|(cells, _)| cells.iter().any(|c| c.has_contents()))
            .map_or(cursor, |row| row.max(cursor));
        let mut rows = rows;
        rows.truncate(last + 1);
        let row = cursor.min(rows.len().saturating_sub(1));
        Self {
            pane,
            rows,
            row,
            col: 0,
            top: top.min(row),
            height: 1,
            anchor: None,
            lines: false,
            typing: None,
            search: None,
        }
    }

    fn width(&self) -> usize {
        self.rows.first().map_or(0, |(cells, _)| cells.len())
    }

    /// A row's text by cell column (blank cells as spaces, wide characters once).
    pub fn text(&self, row: usize) -> String {
        let Some((cells, _)) = self.rows.get(row) else {
            return String::new();
        };
        cells
            .iter()
            .filter(|c| !c.is_wide_continuation())
            .map(|c| match c.contents().as_str() {
                "" => " ".to_string(),
                text => text.to_string(),
            })
            .collect()
    }

    /// The selection as ((row, col), (row, col)), inclusive and in order.
    pub fn range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor?;
        let cursor = (self.row, self.col);
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        if self.lines {
            Some(((start.0, 0), (end.0, self.width().saturating_sub(1))))
        } else {
            Some((start, end))
        }
    }

    pub fn selected(&self, row: usize, col: usize) -> bool {
        self.range()
            .is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end)
    }

    /// The selected text; soft-wrapped rows join, other line ends become newlines
    /// without the padding after them.
    pub fn selection(&self) -> String {
        let ((first, from), (last, to)) = self
            .range()
            .unwrap_or(((self.row, 0), (self.row, self.width().saturating_sub(1))));
        let mut out = String::new();
        for row in first..=last {
            let Some((cells, wrapped)) = self.rows.get(row) else {
                break;
            };
            let start = if row == first { from } else { 0 };
            let end = if row == last { to + 1 } else { cells.len() };
            let piece: String = cells
                .iter()
                .enumerate()
                .filter(|(col, c)| (start..end).contains(col) && !c.is_wide_continuation())
                .map(|(_, c)| match c.contents().as_str() {
                    "" => " ".to_string(),
                    text => text.to_string(),
                })
                .collect();
            let joined = *wrapped && row != last && end == cells.len();
            if joined {
                out.push_str(&piece);
            } else {
                out.push_str(piece.trim_end());
                if row != last {
                    out.push('\n');
                }
            }
        }
        out
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if let Some(query) = &mut self.typing {
            match key.code {
                KeyCode::Enter => {
                    let query = std::mem::take(query);
                    let back = self.search.as_ref().is_some_and(|(_, back)| *back);
                    self.typing = None;
                    if !query.is_empty() {
                        self.search = Some((query, back));
                    }
                    return self.find(false);
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) => query.push(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.height.max(2);
        match key.code {
            KeyCode::Char('c') if ctrl => return Outcome::Copy(self.selection()),
            KeyCode::Char('u') if ctrl => self.move_rows(-(page as isize / 2)),
            KeyCode::Char('d') if ctrl => self.move_rows(page as isize / 2),
            KeyCode::Char('b') if ctrl => self.move_rows(-(page as isize)),
            KeyCode::Char('f') if ctrl => self.move_rows(page as isize),
            KeyCode::Esc if self.anchor.is_some() => self.anchor = None,
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Exit,
            KeyCode::Char('y') | KeyCode::Enter => return Outcome::Copy(self.selection()),
            KeyCode::Char('v') | KeyCode::Char('V') => {
                let lines = key.code == KeyCode::Char('V');
                if self.anchor.is_some() && self.lines == lines {
                    self.anchor = None;
                } else {
                    self.anchor.get_or_insert((self.row, self.col));
                    self.lines = lines;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.move_cols(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_cols(1),
            KeyCode::Char('j') | KeyCode::Down => self.move_rows(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_rows(-1),
            KeyCode::PageUp => self.move_rows(-(page as isize)),
            KeyCode::PageDown => self.move_rows(page as isize),
            KeyCode::Char('0') | KeyCode::Home => self.col = 0,
            KeyCode::Char('$') | KeyCode::End => {
                let text = self.text(self.row);
                self.col = self.column_of(text.trim_end().chars().count().saturating_sub(1));
            }
            KeyCode::Char('^') => {
                let text = self.text(self.row);
                let first = text.chars().take_while(|c| c.is_whitespace()).count();
                self.col = self.column_of(first);
            }
            KeyCode::Char('w') => self.word(true),
            KeyCode::Char('b') => self.word(false),
            KeyCode::Char('g') => {
                self.row = 0;
                self.col = 0;
            }
            KeyCode::Char('G') => {
                self.row = self.rows.len().saturating_sub(1);
                self.col = 0;
            }
            KeyCode::Char('/') | KeyCode::Char('?') => {
                let back = key.code == KeyCode::Char('?');
                self.typing = Some(String::new());
                let query = self.search.take().map(|(q, _)| q).unwrap_or_default();
                self.search = Some((query, back));
            }
            KeyCode::Char('n') => return self.find(false),
            KeyCode::Char('N') => return self.find(true),
            _ => {}
        }
        self.follow();
        Outcome::Stay
    }

    fn move_rows(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.row = self.row.saturating_add_signed(delta).min(last);
    }

    fn move_cols(&mut self, delta: isize) {
        let width = self.width();
        let mut col = self
            .col
            .saturating_add_signed(delta)
            .min(width.saturating_sub(1));
        // Land on the first cell of a wide character.
        while col > 0
            && self.rows[self.row]
                .0
                .get(col)
                .is_some_and(|c| c.is_wide_continuation())
        {
            col = if delta < 0 { col - 1 } else { col + 1 };
            if col >= width {
                col = self.col;
                break;
            }
        }
        self.col = col;
    }

    /// The cell column of the `index`th character on the cursor row.
    fn column_of(&self, index: usize) -> usize {
        self.rows[self.row]
            .0
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_wide_continuation())
            .nth(index)
            .map_or(0, |(col, _)| col)
    }

    /// `w` / `b`: the start of the next / previous word on this row.
    fn word(&mut self, forward: bool) {
        let cells = &self.rows[self.row].0;
        let blank = |col: usize| {
            cells
                .get(col)
                .is_none_or(|c| c.contents().trim().is_empty())
        };
        let mut col = self.col;
        if forward {
            while col < cells.len() && !blank(col) {
                col += 1;
            }
            while col < cells.len() && blank(col) {
                col += 1;
            }
            if col >= cells.len() {
                if self.row + 1 < self.rows.len() {
                    self.row += 1;
                    self.col = 0;
                }
                return;
            }
        } else {
            col = col.saturating_sub(1);
            while col > 0 && blank(col) {
                col -= 1;
            }
            while col > 0 && !blank(col - 1) {
                col -= 1;
            }
        }
        self.col = col;
    }

    /// `n` (same direction as the search) or `N` (the other way), wrapping around.
    fn find(&mut self, reverse: bool) -> Outcome {
        let Some((query, back)) = self.search.clone().filter(|(q, _)| !q.is_empty()) else {
            return Outcome::Stay;
        };
        let query = query.to_lowercase();
        let back = back != reverse;
        let count = self.rows.len();
        for step in 1..=count {
            let row = if back {
                (self.row + count - step % count) % count
            } else {
                (self.row + step) % count
            };
            let text = self.text(row).to_lowercase();
            if let Some(at) = text.find(&query) {
                self.row = row;
                self.col = self.column_of(text[..at].chars().count());
                self.follow();
                return Outcome::Stay;
            }
        }
        Outcome::NotFound(query)
    }

    /// Keep the cursor row on screen.
    pub fn follow(&mut self) {
        if self.row < self.top {
            self.top = self.row;
        }
        if self.row >= self.top + self.height {
            self.top = self.row + 1 - self.height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &[u8], width: u16, height: u16) -> (Vec<Row>, usize, usize) {
        let mut parser = vt100::Parser::new(height, width, 100);
        parser.process(text);
        let cursor = usize::from(parser.screen().cursor_position().0);
        let (rows, history) = crate::terminal::rows(&mut parser);
        (rows, history + cursor, history)
    }

    fn press(mode: &mut CopyMode, keys: &str) -> Option<String> {
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\u{1b}' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            if let Outcome::Copy(text) = mode.key(KeyEvent::new(code, KeyModifiers::NONE)) {
                return Some(text);
            }
        }
        None
    }

    #[test]
    fn copies_lines_without_padding_and_rejoins_soft_wraps() {
        // "abcdefghij" wraps at 6 columns; the shell prompt is below.
        let (rows, cursor, top) = rows(b"$ ls\r\nabcdefghij\r\nsrc  target\r\n$ ", 6, 4);
        let mut mode = CopyMode::new(0, rows, cursor, top);
        mode.height = 4;
        // The prompt line, as a whole line with no selection.
        assert_eq!(press(&mut mode, "y").as_deref(), Some("$"));
        let mut mode = CopyMode::new(0, mode.rows.clone(), cursor, top);
        mode.height = 4;
        // V from "abcdef" down two rows: the wrapped line comes back whole.
        let found = press(&mut mode, "?abc\n");
        assert!(found.is_none());
        assert_eq!(mode.text(mode.row).trim_end(), "abcdef");
        let copied = press(&mut mode, "Vjjy").unwrap();
        assert_eq!(copied, "abcdefghij\nsrc  t");
    }

    #[test]
    fn character_selection_and_search() {
        let (rows, cursor, top) = rows(b"error: one\r\nok\r\nerror: two\r\n", 20, 6);
        let mut mode = CopyMode::new(0, rows, cursor, top);
        mode.height = 6;
        // Searching up finds the nearest match first, then the earlier one.
        press(&mut mode, "?error\n");
        assert_eq!(mode.row, 2);
        press(&mut mode, "n");
        assert_eq!(mode.row, 0);
        // From "one" to the end of the line.
        press(&mut mode, "w");
        assert_eq!(mode.col, 7);
        assert_eq!(press(&mut mode, "v$y").as_deref(), Some("one"));
        // Not found is reported, and Esc leaves.
        let mut mode = CopyMode::new(0, mode.rows.clone(), cursor, top);
        assert!(matches!(
            mode.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            Outcome::Stay
        ));
        press(&mut mode, "zzz");
        assert!(matches!(
            mode.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Outcome::NotFound(_)
        ));
        assert!(matches!(
            mode.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Outcome::Exit
        ));
    }
}
