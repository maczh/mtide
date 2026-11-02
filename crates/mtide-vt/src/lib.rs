//! A small, deterministic VT screen state machine.
//!
//! The parser intentionally owns no terminal I/O. Bytes from a PTY can be fed
//! into [`Screen::feed`] and the resulting grid can be rendered by the TUI.

use std::fmt;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            underline: false,
            inverse: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    pub character: char,
    pub style: Style,
    pub continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            style: Style::default(),
            continuation: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cursor {
    row: usize,
    column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
}

pub struct Screen {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    cursor: Cursor,
    saved_cursor: Cursor,
    style: Style,
    state: ParserState,
    csi: String,
    alternate: Option<(Vec<Cell>, Cursor, Style)>,
    mouse_reporting: bool,
    bracketed_paste: bool,
}

impl fmt::Debug for Screen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Screen")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("cursor", &self.cursor)
            .field("alternate", &self.alternate.is_some())
            .finish()
    }
}

impl Screen {
    pub fn new(width: usize, height: usize) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            cursor: Cursor { row: 0, column: 0 },
            saved_cursor: Cursor { row: 0, column: 0 },
            style: Style::default(),
            state: ParserState::Ground,
            csi: String::new(),
            alternate: None,
            mouse_reporting: false,
            bracketed_paste: false,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.column)
    }
    pub fn cell(&self, row: usize, column: usize) -> Option<Cell> {
        (row < self.height && column < self.width).then(|| self.cells[row * self.width + column])
    }
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_reporting
    }
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }
    pub fn row_text(&self, row: usize) -> String {
        (0..self.width)
            .filter_map(|column| self.cell(row, column).map(|cell| cell.character))
            .collect()
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        let mut replacement = Screen::new(width, height);
        replacement.style = self.style;
        for row in 0..self.height.min(replacement.height) {
            for column in 0..self.width.min(replacement.width) {
                replacement.cells[row * replacement.width + column] =
                    self.cells[row * self.width + column];
            }
        }
        replacement.cursor.row = self.cursor.row.min(replacement.height - 1);
        replacement.cursor.column = self.cursor.column.min(replacement.width - 1);
        replacement.alternate = self.alternate.take();
        replacement.mouse_reporting = self.mouse_reporting;
        replacement.bracketed_paste = self.bracketed_paste;
        *self = replacement;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let input = String::from_utf8_lossy(bytes);
        for character in input.chars() {
            self.feed_character(character);
        }
    }

    fn feed_character(&mut self, character: char) {
        match self.state {
            ParserState::Ground => match character {
                '\x1b' => self.state = ParserState::Escape,
                '\n' => self.line_feed(),
                '\r' => self.cursor.column = 0,
                '\x08' => self.cursor.column = self.cursor.column.saturating_sub(1),
                '\x07' => {}
                character if !character.is_control() => self.put(character),
                _ => {}
            },
            ParserState::Escape => match character {
                '[' => {
                    self.csi.clear();
                    self.state = ParserState::Csi;
                }
                '7' => self.saved_cursor = self.cursor,
                '8' => self.cursor = self.saved_cursor,
                'c' => self.reset(),
                _ => self.state = ParserState::Ground,
            },
            ParserState::Csi => {
                if ('@'..='~').contains(&character) {
                    let params = std::mem::take(&mut self.csi);
                    self.execute_csi(&params, character);
                    self.state = ParserState::Ground;
                } else if self.csi.len() < 32 {
                    self.csi.push(character);
                } else {
                    self.state = ParserState::Ground;
                    self.csi.clear();
                }
            }
        }
    }

    fn put(&mut self, character: char) {
        let width = character.width().unwrap_or(1).max(1);
        if width == 2 && self.cursor.column + 1 >= self.width {
            self.line_feed();
            self.cursor.column = 0;
        }
        let index = self.cursor.row * self.width + self.cursor.column;
        self.cells[index] = Cell {
            character,
            style: self.style,
            continuation: false,
        };
        if width == 2 {
            self.cells[index + 1] = Cell {
                character: ' ',
                style: self.style,
                continuation: true,
            };
        }
        self.cursor.column += width;
        if self.cursor.column >= self.width {
            self.cursor.column = self.width - 1;
        }
    }

    fn line_feed(&mut self) {
        if self.cursor.row + 1 >= self.height {
            self.cells.copy_within(self.width.., 0);
            self.cells[(self.height - 1) * self.width..].fill(Cell::default());
        } else {
            self.cursor.row += 1;
        }
    }

    fn execute_csi(&mut self, sequence: &str, command: char) {
        let private = sequence.starts_with('?');
        let values = sequence
            .trim_start_matches('?')
            .split(';')
            .filter_map(|value| value.parse::<usize>().ok())
            .collect::<Vec<_>>();
        let first = |default| {
            values
                .first()
                .copied()
                .filter(|value| *value > 0)
                .unwrap_or(default)
        };
        match command {
            'A' => self.cursor.row = self.cursor.row.saturating_sub(first(1)),
            'B' => self.cursor.row = (self.cursor.row + first(1)).min(self.height - 1),
            'C' => self.cursor.column = (self.cursor.column + first(1)).min(self.width - 1),
            'D' => self.cursor.column = self.cursor.column.saturating_sub(first(1)),
            'H' | 'f' => {
                self.cursor.row = first(1).saturating_sub(1).min(self.height - 1);
                self.cursor.column = values
                    .get(1)
                    .copied()
                    .filter(|value| *value > 0)
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.width - 1);
            }
            'J' if values.first().copied().unwrap_or(0) == 2 => self.cells.fill(Cell::default()),
            'K' => {
                let start = self.cursor.row * self.width;
                self.cells[start..start + self.width].fill(Cell::default());
            }
            'm' => self.apply_sgr(&values),
            's' => self.saved_cursor = self.cursor,
            'u' => self.cursor = self.saved_cursor,
            'h' if private && values.contains(&1049) => self.enter_alternate(),
            'l' if private && values.contains(&1049) => self.leave_alternate(),
            'h' if private => self.set_private_modes(&values, true),
            'l' if private => self.set_private_modes(&values, false),
            _ => {}
        }
    }

    fn set_private_modes(&mut self, values: &[usize], enabled: bool) {
        if values
            .iter()
            .any(|value| matches!(*value, 1000 | 1002 | 1003 | 1006))
        {
            self.mouse_reporting = enabled;
        }
        if values.contains(&2004) {
            self.bracketed_paste = enabled;
        }
    }

    fn apply_sgr(&mut self, values: &[usize]) {
        if values.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut index = 0;
        while index < values.len() {
            let value = values[index];
            match value {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => self.style.bold = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.foreground = Color::Indexed((value - 30) as u8),
                40..=47 => self.style.background = Color::Indexed((value - 40) as u8),
                90..=97 => self.style.foreground = Color::Indexed((value - 90 + 8) as u8),
                100..=107 => self.style.background = Color::Indexed((value - 100 + 8) as u8),
                39 => self.style.foreground = Color::Default,
                49 => self.style.background = Color::Default,
                38 | 48 if index + 2 < values.len() && values[index + 1] == 5 => {
                    let color = Color::Indexed(values[index + 2].min(255) as u8);
                    if value == 38 {
                        self.style.foreground = color;
                    } else {
                        self.style.background = color;
                    }
                    index += 2;
                }
                38 | 48 if index + 4 < values.len() && values[index + 1] == 2 => {
                    let color = Color::Rgb(
                        values[index + 2].min(255) as u8,
                        values[index + 3].min(255) as u8,
                        values[index + 4].min(255) as u8,
                    );
                    if value == 38 {
                        self.style.foreground = color;
                    } else {
                        self.style.background = color;
                    }
                    index += 4;
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn enter_alternate(&mut self) {
        if self.alternate.is_none() {
            let primary = std::mem::replace(
                &mut self.cells,
                vec![Cell::default(); self.width * self.height],
            );
            self.alternate = Some((primary, self.cursor, self.style));
            self.cursor = Cursor { row: 0, column: 0 };
            self.style = Style::default();
        }
    }

    fn leave_alternate(&mut self) {
        if let Some((cells, cursor, style)) = self.alternate.take() {
            self.cells = cells;
            self.cursor = cursor;
            self.style = style;
        }
    }

    fn reset(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = Cursor { row: 0, column: 0 };
        self.saved_cursor = self.cursor;
        self.style = Style::default();
        self.state = ParserState::Ground;
        self.alternate = None;
        self.mouse_reporting = false;
        self.bracketed_paste = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{Color, Screen};

    #[test]
    fn fr_tr01_vt_renders_text_cursor_and_wide_cjk() {
        let mut screen = Screen::new(8, 2);
        screen.feed("hi 你好".as_bytes());
        assert_eq!(screen.row_text(0), "hi 你 好  ");
        assert_eq!(screen.cursor(), (0, 7));
        assert!(screen.cell(0, 4).is_some_and(|cell| cell.continuation));
    }

    #[test]
    fn fr_tr01_vt_handles_sgr_clear_and_cursor_position() {
        let mut screen = Screen::new(8, 2);
        screen.feed(b"\x1b[31mred\x1b[2;3HX");
        assert_eq!(
            screen.cell(0, 0).map(|cell| cell.style.foreground),
            Some(Color::Indexed(1))
        );
        assert_eq!(screen.cell(1, 2).map(|cell| cell.character), Some('X'));
        screen.feed(b"\x1b[2J");
        assert_eq!(screen.row_text(0), "        ");
    }

    #[test]
    fn fr_tr01_vt_supports_256_and_truecolor_sgr() {
        let mut screen = Screen::new(12, 1);
        screen.feed(b"\x1b[38;5;201mA\x1b[48;2;1;2;3mB\x1b[0m");
        assert_eq!(
            screen.cell(0, 0).map(|cell| cell.style.foreground),
            Some(Color::Indexed(201))
        );
        assert_eq!(
            screen.cell(0, 1).map(|cell| cell.style.foreground),
            Some(Color::Indexed(201))
        );
        assert_eq!(
            screen.cell(0, 1).map(|cell| cell.style.background),
            Some(Color::Rgb(1, 2, 3))
        );
    }

    #[test]
    fn fr_tr01_vt_restores_primary_screen_after_alternate_screen() {
        let mut screen = Screen::new(8, 2);
        screen.feed(b"primary\x1b[?1049halt\x1b[?1049l");
        assert!(screen.row_text(0).starts_with("primary"));
    }

    #[test]
    fn fr_tr01_vt_tracks_mouse_and_bracketed_paste_modes() {
        let mut screen = Screen::new(8, 2);
        screen.feed(b"\x1b[?1000;1006h\x1b[?2004h");
        assert!(screen.mouse_reporting());
        assert!(screen.bracketed_paste());
        screen.feed(b"\x1b[?1000;1006l\x1b[?2004l");
        assert!(!screen.mouse_reporting());
        assert!(!screen.bracketed_paste());
    }
}
