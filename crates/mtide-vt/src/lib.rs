//! A small, deterministic VT screen state machine.
//!
//! The parser intentionally owns no terminal I/O. Bytes from a PTY can be fed
//! into [`Screen::feed`] and the resulting grid can be rendered by the TUI.
//!
//! Correctness priorities follow PRD TR-01: text must not be lost at the right
//! margin, partial UTF-8 sequences split across PTY reads must not turn into
//! replacement characters, and erase-line / erase-display modes must honour
//! their parameters. Control strings (OSC, DCS) are consumed instead of being
//! printed as text, which is what keeps shell titles and hyperlinks from
//! corrupting the grid.

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
    /// Consumes a charset designation such as `ESC ( B`.
    Charset,
    /// Consumes an OSC / DCS / APC string until BEL or ST.
    Control,
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
    /// Bytes of an incomplete UTF-8 sequence held over for the next feed call.
    pending_utf8: Vec<u8>,
    alternate: Option<(Vec<Cell>, Cursor, Style)>,
    mouse_reporting: bool,
    bracketed_paste: bool,
    /// Set when the cursor reached the right margin; the next printable
    /// character wraps instead of overwriting the last column.
    wrap_pending: bool,
    scroll_top: usize,
    scroll_bottom: usize,
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
            pending_utf8: Vec::new(),
            alternate: None,
            mouse_reporting: false,
            bracketed_paste: false,
            wrap_pending: false,
            scroll_top: 0,
            scroll_bottom: height,
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
        replacement.saved_cursor = self.saved_cursor;
        replacement.saved_cursor.row = replacement.saved_cursor.row.min(replacement.height - 1);
        replacement.saved_cursor.column =
            replacement.saved_cursor.column.min(replacement.width - 1);
        replacement.alternate = self.alternate.take();
        replacement.mouse_reporting = self.mouse_reporting;
        replacement.bracketed_paste = self.bracketed_paste;
        *self = replacement;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buffer = std::mem::take(&mut self.pending_utf8);
        buffer.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = buffer.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).expect("valid up to error"));
                    match error.error_len() {
                        Some(length) => rest = &after[length..],
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending_utf8 = rest.to_vec();
        for character in text.chars() {
            self.feed_character(character);
        }
    }

    fn feed_character(&mut self, character: char) {
        match self.state {
            ParserState::Ground => match character {
                '\x1b' => self.state = ParserState::Escape,
                '\n' => {
                    self.wrap_pending = false;
                    self.line_feed();
                }
                '\r' => {
                    self.wrap_pending = false;
                    self.cursor.column = 0;
                }
                '\t' => self.tab_forward(1),
                '\x08' => {
                    self.wrap_pending = false;
                    self.cursor.column = self.cursor.column.saturating_sub(1);
                }
                '\x07' => {}
                character if !character.is_control() => self.put(character),
                _ => {}
            },
            ParserState::Escape => match character {
                '[' => {
                    self.csi.clear();
                    self.state = ParserState::Csi;
                }
                ']' | 'P' | 'X' | '^' | '_' => self.state = ParserState::Control,
                '(' | ')' | '*' | '+' | '-' | '.' | '/' => self.state = ParserState::Charset,
                '7' => self.saved_cursor = self.cursor,
                '8' => {
                    self.cursor = self.saved_cursor;
                    self.wrap_pending = false;
                }
                'c' => self.reset(),
                'D' => self.line_feed(),
                'E' => {
                    self.line_feed();
                    self.cursor.column = 0;
                }
                'M' => self.reverse_index(),
                '\\' => self.state = ParserState::Ground,
                _ => self.state = ParserState::Ground,
            },
            ParserState::Charset => self.state = ParserState::Ground,
            ParserState::Control => match character {
                '\x07' => self.state = ParserState::Ground,
                '\x1b' => self.state = ParserState::Escape,
                _ => {}
            },
            ParserState::Csi => {
                if ('@'..='~').contains(&character) {
                    let params = std::mem::take(&mut self.csi);
                    self.execute_csi(&params, character);
                    self.state = ParserState::Ground;
                } else if self.csi.len() < 64 {
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
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor.column = 0;
            self.line_feed();
        }
        if width == 2 && self.cursor.column + 1 >= self.width {
            self.cursor.column = 0;
            self.line_feed();
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
        let next = self.cursor.column + width;
        if next >= self.width {
            self.cursor.column = self.width - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.column = next;
        }
    }

    fn tab_forward(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let next = (self.cursor.column / 8 + 1) * 8;
            self.wrap_pending = false;
            if next >= self.width {
                self.cursor.column = self.width - 1;
            } else {
                self.cursor.column = next;
            }
        }
    }

    fn line_feed(&mut self) {
        if self.cursor.row + 1 >= self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn scroll_up(&mut self, count: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        if bottom <= top {
            return;
        }
        let count = count.min(bottom - top);
        let start = top * self.width;
        let end = bottom * self.width;
        self.cells
            .copy_within(start + count * self.width..end, start);
        self.cells[end - count * self.width..end].fill(Cell::default());
    }

    fn scroll_down(&mut self, count: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        if bottom <= top {
            return;
        }
        let count = count.min(bottom - top);
        let start = top * self.width;
        let end = bottom * self.width;
        self.cells
            .copy_within(start..end - count * self.width, start + count * self.width);
        self.cells[start..start + count * self.width].fill(Cell::default());
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
            'A' => {
                self.wrap_pending = false;
                self.cursor.row = self.cursor.row.saturating_sub(first(1));
            }
            'B' => {
                self.wrap_pending = false;
                self.cursor.row = (self.cursor.row + first(1)).min(self.height - 1);
            }
            'C' => {
                self.wrap_pending = false;
                self.cursor.column = (self.cursor.column + first(1)).min(self.width - 1);
            }
            'D' => {
                self.wrap_pending = false;
                self.cursor.column = self.cursor.column.saturating_sub(first(1));
            }
            'E' => {
                self.wrap_pending = false;
                self.cursor.column = 0;
                self.cursor.row = (self.cursor.row + first(1)).min(self.height - 1);
            }
            'F' => {
                self.wrap_pending = false;
                self.cursor.column = 0;
                self.cursor.row = self.cursor.row.saturating_sub(first(1));
            }
            'G' | '`' => {
                self.wrap_pending = false;
                self.cursor.column = first(1).saturating_sub(1).min(self.width - 1);
            }
            'd' => {
                self.wrap_pending = false;
                self.cursor.row = first(1).saturating_sub(1).min(self.height - 1);
            }
            'H' | 'f' => {
                self.wrap_pending = false;
                self.cursor.row = first(1).saturating_sub(1).min(self.height - 1);
                self.cursor.column = values
                    .get(1)
                    .copied()
                    .filter(|value| *value > 0)
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.width - 1);
            }
            'J' => self.erase_display(values.first().copied().unwrap_or(0)),
            'K' => self.erase_line(values.first().copied().unwrap_or(0)),
            'X' => {
                let count = first(1);
                let start = self.cursor.row * self.width + self.cursor.column;
                let end = (start + count).min((self.cursor.row + 1) * self.width);
                self.cells[start..end].fill(Cell::default());
                self.wrap_pending = false;
            }
            'P' => self.delete_characters(first(1)),
            '@' => self.insert_characters(first(1)),
            'L' => self.insert_lines(first(1)),
            'M' => self.delete_lines(first(1)),
            'S' => self.scroll_up(first(1)),
            'T' => self.scroll_down(first(1)),
            'I' => self.tab_forward(first(1)),
            'Z' => {
                for _ in 0..first(1) {
                    let previous = self.cursor.column / 8;
                    let column = previous.saturating_sub(1) * 8;
                    self.cursor.column = column.min(self.width - 1);
                }
                self.wrap_pending = false;
            }
            'm' => self.apply_sgr(&values),
            'r' => self.set_scroll_region(&values),
            's' => self.saved_cursor = self.cursor,
            'u' => {
                self.cursor = self.saved_cursor;
                self.wrap_pending = false;
            }
            'h' if private && values.contains(&1049) => self.enter_alternate(),
            'l' if private && values.contains(&1049) => self.leave_alternate(),
            'h' if private => self.set_private_modes(&values, true),
            'l' if private => self.set_private_modes(&values, false),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row_start = self.cursor.row * self.width;
        let row_end = row_start + self.width;
        let column = self.cursor.column;
        match mode {
            1 => self.cells[row_start..row_start + column + 1].fill(Cell::default()),
            2 => self.cells[row_start..row_end].fill(Cell::default()),
            _ => self.cells[row_start + column..row_end].fill(Cell::default()),
        }
        self.wrap_pending = false;
    }

    fn erase_display(&mut self, mode: usize) {
        let index = self.cursor.row * self.width + self.cursor.column;
        match mode {
            0 => self.cells[index..].fill(Cell::default()),
            1 => self.cells[..=index].fill(Cell::default()),
            _ => self.cells.fill(Cell::default()),
        }
        self.wrap_pending = false;
    }

    fn delete_characters(&mut self, count: usize) {
        let row_start = self.cursor.row * self.width;
        let row_end = row_start + self.width;
        let start = row_start + self.cursor.column;
        let count = count.min(self.width - self.cursor.column);
        self.cells.copy_within(start + count..row_end, start);
        self.cells[row_end - count..row_end].fill(Cell::default());
    }

    fn insert_characters(&mut self, count: usize) {
        let row_start = self.cursor.row * self.width;
        let row_end = row_start + self.width;
        let start = row_start + self.cursor.column;
        let count = count.min(self.width - self.cursor.column);
        self.cells
            .copy_within(start..row_end - count, start + count);
        self.cells[start..start + count].fill(Cell::default());
    }

    fn insert_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row >= self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor.row;
        self.scroll_down(count);
        self.scroll_top = saved_top;
    }

    fn delete_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row >= self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor.row;
        self.scroll_up(count);
        self.scroll_top = saved_top;
    }

    fn set_scroll_region(&mut self, values: &[usize]) {
        let top = values
            .first()
            .copied()
            .filter(|value| *value > 0)
            .unwrap_or(1)
            .saturating_sub(1)
            .min(self.height - 1);
        let bottom = values
            .get(1)
            .copied()
            .filter(|value| *value > 0)
            .unwrap_or(self.height)
            .min(self.height);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            self.cursor = Cursor { row: 0, column: 0 };
            self.wrap_pending = false;
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
            self.wrap_pending = false;
            self.scroll_top = 0;
            self.scroll_bottom = self.height;
        }
    }

    fn leave_alternate(&mut self) {
        if let Some((cells, cursor, style)) = self.alternate.take() {
            self.cells = cells;
            self.cursor = cursor;
            self.style = style;
            self.wrap_pending = false;
            self.scroll_top = 0;
            self.scroll_bottom = self.height;
        }
    }

    fn reset(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = Cursor { row: 0, column: 0 };
        self.saved_cursor = self.cursor;
        self.style = Style::default();
        self.state = ParserState::Ground;
        self.csi.clear();
        self.pending_utf8.clear();
        self.alternate = None;
        self.mouse_reporting = false;
        self.bracketed_paste = false;
        self.wrap_pending = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.height;
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

    #[test]
    fn fr_tr01_vt_wraps_at_the_right_margin_instead_of_overwriting() {
        let mut screen = Screen::new(8, 3);
        screen.feed(b"abcdefghij");
        assert_eq!(screen.row_text(0), "abcdefgh");
        assert_eq!(screen.row_text(1), "ij      ");
        assert_eq!(screen.cursor(), (1, 2));
    }

    #[test]
    fn fr_tr01_vt_honours_erase_line_modes() {
        let mut screen = Screen::new(8, 1);
        screen.feed(b"hello");
        screen.feed(b"\x1b[1;3H\x1b[0K");
        assert_eq!(screen.row_text(0), "he      ");

        screen.feed(b"\x1b[1;1Habcdefg\x1b[1;4H\x1b[1K");
        assert_eq!(screen.row_text(0), "    efg ");

        screen.feed(b"\x1b[2K");
        assert_eq!(screen.row_text(0), "        ");
    }

    #[test]
    fn fr_tr01_vt_expands_tabs_to_eight_column_stops() {
        let mut screen = Screen::new(16, 1);
        screen.feed(b"a\tb");
        assert_eq!(&screen.row_text(0)[..9], "a       b");
    }

    #[test]
    fn fr_tr01_vt_consumes_control_strings_without_printing_them() {
        let mut screen = Screen::new(16, 1);
        screen.feed(b"\x1b]0;window title\x07ok");
        assert!(screen.row_text(0).starts_with("ok"));

        let mut screen = Screen::new(16, 1);
        screen.feed(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        assert!(screen.row_text(0).starts_with("link"));
    }

    #[test]
    fn fr_tr01_vt_ignores_charset_designation_sequences() {
        let mut screen = Screen::new(8, 1);
        screen.feed(b"\x1b(Bok");
        assert_eq!(screen.row_text(0), "ok      ");
    }

    #[test]
    fn fr_tr01_vt_reassembles_utf8_split_across_reads() {
        let mut screen = Screen::new(8, 1);
        let bytes = "你".as_bytes();
        screen.feed(&bytes[..2]);
        assert_eq!(screen.row_text(0), "        ");
        screen.feed(&bytes[2..]);
        assert!(screen.row_text(0).starts_with('你'));
    }

    #[test]
    fn fr_tr01_vt_scrolls_only_inside_the_declared_region() {
        let mut screen = Screen::new(4, 4);
        screen.feed(b"a\r\nb\r\nc\r\nd");
        screen.feed(b"\x1b[2;3r");
        screen.feed(b"\x1b[3;1H\r\nX");
        assert_eq!(screen.row_text(0), "a   ");
        assert_eq!(screen.row_text(1), "c   ");
        assert_eq!(screen.row_text(2), "X   ");
        assert_eq!(screen.row_text(3), "d   ");
    }

    #[test]
    fn fr_tr01_vt_inserts_and_deletes_characters() {
        let mut screen = Screen::new(8, 1);
        screen.feed(b"abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(screen.row_text(0), "abef    ");

        let mut screen = Screen::new(8, 1);
        screen.feed(b"abcdef\x1b[1;3H\x1b[2@");
        assert_eq!(screen.row_text(0), "ab  cdef");
    }

    #[test]
    fn fr_tr01_vt_inserts_and_deletes_lines_inside_the_region() {
        let mut screen = Screen::new(4, 4);
        screen.feed(b"a\r\nb\r\nc\r\nd");
        screen.feed(b"\x1b[2;1H\x1b[L");
        assert_eq!(screen.row_text(1), "    ");
        assert_eq!(screen.row_text(2), "b   ");

        let mut screen = Screen::new(4, 4);
        screen.feed(b"a\r\nb\r\nc\r\nd");
        screen.feed(b"\x1b[2;1H\x1b[M");
        assert_eq!(screen.row_text(1), "c   ");
        assert_eq!(screen.row_text(2), "d   ");
    }
}
