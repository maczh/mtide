use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
};

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use unicode_width::UnicodeWidthChar;

use crate::error::{AppError, AppResult};

struct PtySession {
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Receiver<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct TerminalScreen {
    columns: u16,
    rows: u16,
    cells: Vec<Vec<char>>,
    wide: Vec<Vec<bool>>,
    combining: Vec<Vec<String>>,
    cursor_x: u16,
    cursor_y: u16,
    escape: EscapeState,
    utf8_pending: Vec<u8>,
    scrollback: Vec<Vec<char>>,
    scrollback_wide: Vec<Vec<bool>>,
    scrollback_combining: Vec<Vec<String>>,
    links: Vec<Vec<Option<String>>>,
    scrollback_limit: usize,
    main_snapshot: Option<MainSnapshot>,
    alternate: bool,
    current_link: Option<String>,
    saved_cursor: Option<(u16, u16)>,
    application_cursor_keys: bool,
    cursor_visible: bool,
    bracketed_paste: bool,
    mouse_tracking: MouseTracking,
    mouse_sgr: bool,
    style: TerminalStyle,
    styles: Vec<Vec<TerminalStyle>>,
    scroll_offset: usize,
    scrollback_styles: Vec<Vec<TerminalStyle>>,
    scroll_top: u16,
    scroll_bottom: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MouseTracking {
    #[default]
    None,
    Normal,
    Button,
    Any,
}

#[derive(Debug, Clone)]
struct MainSnapshot {
    cells: Vec<Vec<char>>,
    wide: Vec<Vec<bool>>,
    styles: Vec<Vec<TerminalStyle>>,
    cursor_x: u16,
    cursor_y: u16,
    scrollback: Vec<Vec<char>>,
    scrollback_wide: Vec<Vec<bool>>,
    scrollback_styles: Vec<Vec<TerminalStyle>>,
    combining: Vec<Vec<String>>,
    scrollback_combining: Vec<Vec<String>>,
    links: Vec<Vec<Option<String>>>,
    current_link: Option<String>,
    saved_cursor: Option<(u16, u16)>,
    scroll_top: u16,
    scroll_bottom: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalStyle {
    pub foreground: Option<AnsiColor>,
    pub background: Option<AnsiColor>,
    pub bold: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
    Basic(u8),
    Bright(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Default)]
enum EscapeState {
    #[default]
    Normal,
    Escape,
    Csi(String),
    Osc(String),
}

impl TerminalScreen {
    pub fn new(columns: u16, rows: u16) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        Self {
            columns,
            rows,
            cells: vec![vec![' '; columns as usize]; rows as usize],
            wide: vec![vec![false; columns as usize]; rows as usize],
            combining: vec![vec![String::new(); columns as usize]; rows as usize],
            cursor_x: 0,
            cursor_y: 0,
            escape: EscapeState::Normal,
            utf8_pending: Vec::new(),
            scrollback: Vec::new(),
            scrollback_wide: Vec::new(),
            scrollback_combining: Vec::new(),
            links: vec![vec![None; columns as usize]; rows as usize],
            scrollback_limit: 10_000,
            main_snapshot: None,
            alternate: false,
            current_link: None,
            saved_cursor: None,
            application_cursor_keys: false,
            cursor_visible: true,
            bracketed_paste: false,
            mouse_tracking: MouseTracking::None,
            mouse_sgr: false,
            style: TerminalStyle::default(),
            styles: vec![vec![TerminalStyle::default(); columns as usize]; rows as usize],
            scroll_offset: 0,
            scrollback_styles: Vec::new(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let (start, end) = self.visible_range();
        let history_len = self.scrollback.len();
        (start..end)
            .map(|index| {
                let (cells, combining) = if index < history_len {
                    (&self.scrollback[index], &self.scrollback_combining[index])
                } else {
                    let current = index - history_len;
                    (&self.cells[current], &self.combining[current])
                };
                cells
                    .iter()
                    .zip(combining)
                    .fold(String::new(), |mut line, (cell, marks)| {
                        line.push(*cell);
                        line.push_str(marks);
                        line
                    })
            })
            .collect()
    }

    pub fn styled_lines(&self) -> Vec<Vec<(char, TerminalStyle)>> {
        let (start, end) = self.visible_range();
        let history_len = self.scrollback.len();
        (start..end)
            .map(|index| {
                let (cells, styles, combining) = if index < history_len {
                    (
                        &self.scrollback[index],
                        &self.scrollback_styles[index],
                        &self.scrollback_combining[index],
                    )
                } else {
                    let current = index - history_len;
                    (
                        &self.cells[current],
                        &self.styles[current],
                        &self.combining[current],
                    )
                };
                let mut result = Vec::new();
                for (column, (cell, style)) in cells.iter().zip(styles).enumerate() {
                    let cursor_cell = self.cursor_visible
                        && self.scroll_offset == 0
                        && index >= history_len
                        && index - history_len == self.cursor_y as usize
                        && column == self.cursor_x as usize;
                    let mut style = *style;
                    if cursor_cell {
                        style.inverse = !style.inverse;
                    }
                    result.push((*cell, style));
                    result.extend(combining[column].chars().map(|mark| (mark, style)));
                }
                result
            })
            .collect()
    }

    pub fn link_at(&self, column: u16, row: u16) -> Option<&str> {
        self.links
            .get(row as usize)?
            .get(column as usize)?
            .as_deref()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.utf8_pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    let text = text.to_owned();
                    self.utf8_pending.clear();
                    for character in text.chars() {
                        self.feed_character(character);
                    }
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let text = std::str::from_utf8(&self.utf8_pending[..valid_up_to])
                            .expect("valid UTF-8 prefix")
                            .to_owned();
                        self.utf8_pending.drain(..valid_up_to);
                        for character in text.chars() {
                            self.feed_character(character);
                        }
                        continue;
                    }
                    let Some(error_length) = error.error_len() else {
                        break;
                    };
                    self.utf8_pending.drain(..error_length);
                    self.feed_character('\u{fffd}');
                }
            }
        }
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let mut cells = vec![vec![' '; columns as usize]; rows as usize];
        let mut wide = vec![vec![false; columns as usize]; rows as usize];
        let mut combining = vec![vec![String::new(); columns as usize]; rows as usize];
        let mut styles = vec![vec![TerminalStyle::default(); columns as usize]; rows as usize];
        let mut links = vec![vec![None; columns as usize]; rows as usize];
        for (new_row, old_row) in cells.iter_mut().zip(&self.cells) {
            for (new_cell, old_cell) in new_row.iter_mut().zip(old_row) {
                *new_cell = *old_cell;
            }
        }
        for (new_row, old_row) in wide.iter_mut().zip(&self.wide) {
            for (new_cell, old_cell) in new_row.iter_mut().zip(old_row) {
                *new_cell = *old_cell;
            }
        }
        for (new_row, old_row) in styles.iter_mut().zip(&self.styles) {
            for (new_cell, old_cell) in new_row.iter_mut().zip(old_row) {
                *new_cell = *old_cell;
            }
        }
        for (new_row, old_row) in combining.iter_mut().zip(&self.combining) {
            for (new_cell, old_cell) in new_row.iter_mut().zip(old_row) {
                *new_cell = old_cell.clone();
            }
        }
        for (new_row, old_row) in links.iter_mut().zip(&self.links) {
            for (new_cell, old_cell) in new_row.iter_mut().zip(old_row) {
                *new_cell = old_cell.clone();
            }
        }
        self.columns = columns;
        self.rows = rows;
        self.cells = cells;
        self.wide = wide;
        self.combining = combining;
        self.styles = styles;
        self.links = links;
        self.scroll_offset = 0;
        self.cursor_x = self.cursor_x.min(columns - 1);
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.scroll_top = self.scroll_top.min(rows - 1);
        self.scroll_bottom = self.scroll_bottom.min(rows - 1);
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
            self.scroll_bottom = rows - 1;
        }
    }

    pub fn set_scrollback(&mut self, limit: usize) {
        self.scrollback_limit = limit.max(1);
        if self.scrollback.len() > self.scrollback_limit {
            let drain = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(..drain);
            self.scrollback_wide.drain(..drain);
            self.scrollback_combining.drain(..drain);
            self.scrollback_styles.drain(..drain);
        }
    }

    pub fn scroll(&mut self, delta: isize) {
        let max_offset = self.scrollback.len();
        self.scroll_offset =
            (self.scroll_offset as isize + delta).clamp(0, max_offset as isize) as usize;
    }

    pub const fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    pub const fn mouse_tracking(&self) -> bool {
        !matches!(self.mouse_tracking, MouseTracking::None)
    }

    pub const fn mouse_sgr(&self) -> bool {
        self.mouse_sgr
    }

    pub const fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub const fn alternate_screen(&self) -> bool {
        self.alternate
    }

    pub fn clear(&mut self) {
        self.cells = vec![vec![' '; self.columns as usize]; self.rows as usize];
        self.wide = vec![vec![false; self.columns as usize]; self.rows as usize];
        self.combining = vec![vec![String::new(); self.columns as usize]; self.rows as usize];
        self.styles =
            vec![vec![TerminalStyle::default(); self.columns as usize]; self.rows as usize];
        self.links = vec![vec![None; self.columns as usize]; self.rows as usize];
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.utf8_pending.clear();
        self.scrollback.clear();
        self.scrollback_wide.clear();
        self.scrollback_combining.clear();
        self.scrollback_styles.clear();
        self.scroll_offset = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    fn feed_character(&mut self, character: char) {
        match &mut self.escape {
            EscapeState::Normal => match character {
                '\x1b' => self.escape = EscapeState::Escape,
                '\r' => self.cursor_x = 0,
                '\n' => self.line_feed(),
                '\x08' => {
                    self.cursor_x = self.cursor_x.saturating_sub(1);
                    if self.wide[self.cursor_y as usize][self.cursor_x as usize] {
                        self.cursor_x = self.cursor_x.saturating_sub(1);
                    }
                }
                '\t' => self.cursor_x = (((self.cursor_x / 8) + 1) * 8).min(self.columns - 1),
                character if !character.is_control() => self.put(character),
                _ => {}
            },
            EscapeState::Escape => {
                if character == '[' {
                    self.escape = EscapeState::Csi(String::new());
                } else if character == ']' {
                    self.escape = EscapeState::Osc(String::new());
                } else if matches!(character, '7' | '8') {
                    if character == '7' {
                        self.saved_cursor = Some((self.cursor_x, self.cursor_y));
                    } else if let Some((column, row)) = self.saved_cursor {
                        self.cursor_x = column.min(self.columns - 1);
                        self.cursor_y = row.min(self.rows - 1);
                    }
                    self.escape = EscapeState::Normal;
                } else {
                    self.escape = EscapeState::Normal;
                }
            }
            EscapeState::Csi(params) => {
                if matches!(character, '@'..='~') {
                    let params = std::mem::take(params);
                    self.escape = EscapeState::Normal;
                    self.apply_csi(&params, character);
                } else if character.is_ascii_digit() || matches!(character, ';' | '?') {
                    params.push(character);
                } else {
                    self.escape = EscapeState::Normal;
                }
            }
            EscapeState::Osc(payload) => {
                if character == '\x07' {
                    let payload = std::mem::take(payload);
                    self.apply_osc(&payload);
                    self.escape = EscapeState::Normal;
                } else if character == '\x1b' {
                    let payload = std::mem::take(payload);
                    self.apply_osc(&payload);
                    self.escape = EscapeState::Escape;
                } else {
                    payload.push(character);
                }
            }
        }
    }

    fn put(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            let (row, column) = if self.cursor_x > 0 {
                let row = self.cursor_y as usize;
                let mut column = self.cursor_x as usize - 1;
                if self.wide[row][column] {
                    column = column.saturating_sub(1);
                }
                (row, column)
            } else if self.cursor_y > 0 {
                (self.cursor_y as usize - 1, self.columns as usize - 1)
            } else {
                return;
            };
            self.combining[row][column].push(character);
            return;
        }
        if width > 1 && self.cursor_x as usize + width > self.columns as usize {
            self.cursor_x = 0;
            self.line_feed();
        }
        if self.cursor_x >= self.columns {
            self.cursor_x = 0;
            self.line_feed();
        }
        if self.wide[self.cursor_y as usize][self.cursor_x as usize]
            || (self.cursor_x as usize + 1 < self.columns as usize
                && self.wide[self.cursor_y as usize][self.cursor_x as usize + 1])
        {
            self.clear_cell(self.cursor_y as usize, self.cursor_x as usize);
        }
        self.cells[self.cursor_y as usize][self.cursor_x as usize] = character;
        self.wide[self.cursor_y as usize][self.cursor_x as usize] = false;
        self.combining[self.cursor_y as usize][self.cursor_x as usize].clear();
        self.styles[self.cursor_y as usize][self.cursor_x as usize] = self.style;
        self.links[self.cursor_y as usize][self.cursor_x as usize] = self.current_link.clone();
        for offset in 1..width {
            let column = self.cursor_x as usize + offset;
            self.cells[self.cursor_y as usize][column] = ' ';
            self.wide[self.cursor_y as usize][column] = true;
            self.combining[self.cursor_y as usize][column].clear();
            self.styles[self.cursor_y as usize][column] = self.style;
            self.links[self.cursor_y as usize][column] = self.current_link.clone();
        }
        self.cursor_x += width as u16;
        if self.cursor_x >= self.columns {
            self.cursor_x = 0;
            self.line_feed();
        }
    }

    fn line_feed(&mut self) {
        if self.cursor_y == self.scroll_bottom
            || (self.cursor_y + 1 >= self.rows && self.scroll_bottom == self.rows - 1)
        {
            self.scroll_region_up();
        } else if self.cursor_y + 1 < self.rows {
            self.cursor_y += 1;
        }
    }

    fn scroll_region_up(&mut self) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        let removed = self.cells.remove(top);
        let removed_wide = self.wide.remove(top);
        let removed_combining = self.combining.remove(top);
        let removed_styles = self.styles.remove(top);
        let removed_links = self.links.remove(top);
        if top == 0 && bottom + 1 == self.rows as usize && !self.alternate {
            self.scrollback.push(removed);
            self.scrollback_wide.push(removed_wide);
            self.scrollback_combining.push(removed_combining);
            self.scrollback_styles.push(removed_styles);
            if self.scrollback.len() > self.scrollback_limit {
                self.scrollback.remove(0);
                self.scrollback_wide.remove(0);
                self.scrollback_combining.remove(0);
                self.scrollback_styles.remove(0);
            }
            self.scroll_offset = 0;
        }
        let blank = vec![' '; self.columns as usize];
        self.cells.insert(bottom, blank);
        self.wide.insert(bottom, vec![false; self.columns as usize]);
        self.combining
            .insert(bottom, vec![String::new(); self.columns as usize]);
        self.styles.insert(
            bottom,
            vec![TerminalStyle::default(); self.columns as usize],
        );
        self.links.insert(bottom, vec![None; self.columns as usize]);
        let _ = removed_links;
    }

    fn scroll_region_down(&mut self) {
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        self.cells.remove(bottom);
        self.wide.remove(bottom);
        self.combining.remove(bottom);
        self.styles.remove(bottom);
        self.links.remove(bottom);
        self.cells.insert(top, vec![' '; self.columns as usize]);
        self.wide.insert(top, vec![false; self.columns as usize]);
        self.combining
            .insert(top, vec![String::new(); self.columns as usize]);
        self.styles
            .insert(top, vec![TerminalStyle::default(); self.columns as usize]);
        self.links.insert(top, vec![None; self.columns as usize]);
    }

    fn edit_lines(&mut self, count: usize, insert: bool) {
        let cursor = self.cursor_y as usize;
        let top = self.scroll_top as usize;
        let bottom = self.scroll_bottom as usize;
        if cursor < top || cursor > bottom {
            return;
        }
        let count = count.min(bottom - cursor + 1);
        for _ in 0..count {
            if insert {
                self.cells.remove(bottom);
                self.wide.remove(bottom);
                self.combining.remove(bottom);
                self.styles.remove(bottom);
                self.links.remove(bottom);
                self.cells.insert(cursor, vec![' '; self.columns as usize]);
                self.wide.insert(cursor, vec![false; self.columns as usize]);
                self.combining
                    .insert(cursor, vec![String::new(); self.columns as usize]);
                self.styles.insert(
                    cursor,
                    vec![TerminalStyle::default(); self.columns as usize],
                );
                self.links.insert(cursor, vec![None; self.columns as usize]);
            } else {
                self.cells.remove(cursor);
                self.wide.remove(cursor);
                self.combining.remove(cursor);
                self.styles.remove(cursor);
                self.links.remove(cursor);
                self.cells.insert(bottom, vec![' '; self.columns as usize]);
                self.wide.insert(bottom, vec![false; self.columns as usize]);
                self.combining
                    .insert(bottom, vec![String::new(); self.columns as usize]);
                self.styles.insert(
                    bottom,
                    vec![TerminalStyle::default(); self.columns as usize],
                );
                self.links.insert(bottom, vec![None; self.columns as usize]);
            }
        }
    }

    fn clear_cell(&mut self, row: usize, column: usize) {
        let start = if self.wide[row][column] {
            column.saturating_sub(1)
        } else {
            column
        };
        let end = if start + 1 < self.columns as usize && self.wide[row][start + 1] {
            start + 2
        } else {
            start + 1
        };
        for column in start..end.min(self.columns as usize) {
            self.cells[row][column] = ' ';
            self.wide[row][column] = false;
            self.combining[row][column].clear();
            self.styles[row][column] = TerminalStyle::default();
            self.links[row][column] = None;
        }
    }

    fn clear_row_range(&mut self, row: usize, start: usize, end: usize) {
        for column in start..end.min(self.columns as usize) {
            self.clear_cell(row, column);
        }
    }

    fn edit_row(&mut self, count: usize, insert: bool) {
        let row = self.cursor_y as usize;
        let start = self.cursor_x as usize;
        let width = self.columns as usize;
        let count = count.min(width.saturating_sub(start));
        if count == 0 {
            return;
        }
        if insert {
            let movable = width.saturating_sub(start + count);
            for offset in (0..movable).rev() {
                let source = start + offset;
                let destination = source + count;
                self.cells[row][destination] = self.cells[row][source];
                self.wide[row][destination] = self.wide[row][source];
                self.combining[row][destination] = self.combining[row][source].clone();
                self.styles[row][destination] = self.styles[row][source];
                self.links[row][destination] = self.links[row][source].clone();
            }
        } else {
            let movable = width.saturating_sub(start + count);
            for offset in 0..movable {
                let source = start + count + offset;
                let destination = start + offset;
                self.cells[row][destination] = self.cells[row][source];
                self.wide[row][destination] = self.wide[row][source];
                self.combining[row][destination] = self.combining[row][source].clone();
                self.styles[row][destination] = self.styles[row][source];
                self.links[row][destination] = self.links[row][source].clone();
            }
        }
        if insert {
            self.clear_row_range(row, start, start + count);
        } else {
            self.clear_row_range(row, width - count, width);
        }
    }

    fn apply_csi(&mut self, raw: &str, command: char) {
        let private = raw.starts_with('?');
        let clean = raw.strip_prefix('?').unwrap_or(raw);
        let values: Vec<u16> = if clean.is_empty() {
            Vec::new()
        } else {
            clean
                .split(';')
                .map(|value| value.parse::<u16>().unwrap_or(1))
                .collect()
        };
        let first = values.first().copied().unwrap_or(1).max(1);
        let erase = values.first().copied().unwrap_or(0);
        match command {
            'h' if private && values.first() == Some(&1049) => self.enter_alternate(),
            'l' if private && values.first() == Some(&1049) => self.leave_alternate(),
            'h' if private && values.first() == Some(&1) => self.application_cursor_keys = true,
            'l' if private && values.first() == Some(&1) => self.application_cursor_keys = false,
            'h' if private && values.first() == Some(&25) => self.cursor_visible = true,
            'l' if private && values.first() == Some(&25) => self.cursor_visible = false,
            'h' if private && values.first() == Some(&2004) => self.bracketed_paste = true,
            'l' if private && values.first() == Some(&2004) => self.bracketed_paste = false,
            'h' if private && values.first() == Some(&1000) => {
                self.mouse_tracking = MouseTracking::Normal
            }
            'l' if private && values.first() == Some(&1000) => {
                self.mouse_tracking = MouseTracking::None
            }
            'h' if private && values.first() == Some(&1002) => {
                self.mouse_tracking = MouseTracking::Button
            }
            'l' if private && values.first() == Some(&1002) => {
                self.mouse_tracking = MouseTracking::None
            }
            'h' if private && values.first() == Some(&1003) => {
                self.mouse_tracking = MouseTracking::Any
            }
            'l' if private && values.first() == Some(&1003) => {
                self.mouse_tracking = MouseTracking::None
            }
            'h' if private && values.first() == Some(&1006) => self.mouse_sgr = true,
            'l' if private && values.first() == Some(&1006) => self.mouse_sgr = false,
            'm' => self.apply_sgr(&values),
            'A' => self.cursor_y = self.cursor_y.saturating_sub(first),
            'B' => self.cursor_y = (self.cursor_y + first).min(self.rows - 1),
            'C' => self.cursor_x = (self.cursor_x + first).min(self.columns - 1),
            'D' => self.cursor_x = self.cursor_x.saturating_sub(first),
            'E' => {
                self.cursor_y = (self.cursor_y + first).min(self.rows - 1);
                self.cursor_x = 0;
            }
            'F' => {
                self.cursor_y = self.cursor_y.saturating_sub(first);
                self.cursor_x = 0;
            }
            'G' => {
                self.cursor_x = values
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.columns - 1);
            }
            'd' => {
                self.cursor_y = values
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.rows - 1);
            }
            's' => self.saved_cursor = Some((self.cursor_x, self.cursor_y)),
            'u' => {
                if let Some((column, row)) = self.saved_cursor {
                    self.cursor_x = column.min(self.columns - 1);
                    self.cursor_y = row.min(self.rows - 1);
                }
            }
            'H' | 'f' => {
                self.cursor_y = values
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.rows - 1);
                self.cursor_x = values
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.columns - 1);
            }
            'r' => {
                let top = values.first().copied().unwrap_or(1).saturating_sub(1);
                let bottom = values
                    .get(1)
                    .copied()
                    .unwrap_or(self.rows)
                    .saturating_sub(1);
                if top < bottom && bottom < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.cursor_x = 0;
                    self.cursor_y = 0;
                }
            }
            'J' => match erase {
                0 => {
                    self.clear_row_range(
                        self.cursor_y as usize,
                        self.cursor_x as usize,
                        self.columns as usize,
                    );
                    for row in self.cursor_y as usize + 1..self.rows as usize {
                        self.clear_row_range(row, 0, self.columns as usize);
                    }
                }
                1 => {
                    for row in 0..self.cursor_y as usize {
                        self.clear_row_range(row, 0, self.columns as usize);
                    }
                    self.clear_row_range(self.cursor_y as usize, 0, self.cursor_x as usize + 1);
                }
                2 | 3 => {
                    for row in 0..self.rows as usize {
                        self.clear_row_range(row, 0, self.columns as usize);
                    }
                    self.scrollback.clear();
                    self.scrollback_combining.clear();
                    self.scrollback_styles.clear();
                    self.scroll_offset = 0;
                }
                _ => {}
            },
            'K' => {
                let row = self.cursor_y as usize;
                match erase {
                    0 => self.clear_row_range(row, self.cursor_x as usize, self.columns as usize),
                    1 => self.clear_row_range(row, 0, self.cursor_x as usize + 1),
                    2 => self.clear_row_range(row, 0, self.columns as usize),
                    _ => {}
                }
            }
            '@' => self.edit_row(first as usize, true),
            'P' => self.edit_row(first as usize, false),
            'X' => self.clear_row_range(
                self.cursor_y as usize,
                self.cursor_x as usize,
                self.cursor_x as usize + first as usize,
            ),
            'L' => self.edit_lines(first as usize, true),
            'M' => self.edit_lines(first as usize, false),
            'S' => {
                for _ in 0..first {
                    self.scroll_region_up();
                }
            }
            'T' => {
                for _ in 0..first {
                    self.scroll_region_down();
                }
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, values: &[u16]) {
        let values = if values.is_empty() { &[0][..] } else { values };
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => self.style = TerminalStyle::default(),
                1 => self.style.bold = true,
                22 => self.style.bold = false,
                4 => self.style.underline = true,
                24 => self.style.underline = false,
                7 => self.style.inverse = true,
                27 => self.style.inverse = false,
                30..=37 => {
                    self.style.foreground = Some(AnsiColor::Basic((values[index] - 30) as u8))
                }
                39 => self.style.foreground = None,
                40..=47 => {
                    self.style.background = Some(AnsiColor::Basic((values[index] - 40) as u8))
                }
                49 => self.style.background = None,
                90..=97 => {
                    self.style.foreground = Some(AnsiColor::Bright((values[index] - 90) as u8))
                }
                100..=107 => {
                    self.style.background = Some(AnsiColor::Bright((values[index] - 100) as u8))
                }
                38 | 48 if values.get(index + 1) == Some(&5) => {
                    if let Some(color) = values.get(index + 2).copied() {
                        let color = AnsiColor::Indexed(color.min(255) as u8);
                        if values[index] == 38 {
                            self.style.foreground = Some(color);
                        } else {
                            self.style.background = Some(color);
                        }
                        index += 2;
                    }
                }
                38 | 48 if values.get(index + 1) == Some(&2) => {
                    if let (Some(r), Some(g), Some(b)) = (
                        values.get(index + 2),
                        values.get(index + 3),
                        values.get(index + 4),
                    ) {
                        let color = AnsiColor::Rgb(
                            (*r).min(255) as u8,
                            (*g).min(255) as u8,
                            (*b).min(255) as u8,
                        );
                        if values[index] == 38 {
                            self.style.foreground = Some(color);
                        } else {
                            self.style.background = Some(color);
                        }
                        index += 4;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn apply_osc(&mut self, payload: &str) {
        let Some(rest) = payload.strip_prefix("8;") else {
            return;
        };
        let Some((_, url)) = rest.split_once(';') else {
            return;
        };
        self.current_link = (!url.is_empty()).then(|| url.to_owned());
    }

    fn enter_alternate(&mut self) {
        if self.alternate {
            return;
        }
        self.main_snapshot = Some(MainSnapshot {
            cells: self.cells.clone(),
            wide: self.wide.clone(),
            styles: self.styles.clone(),
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            scrollback: self.scrollback.clone(),
            scrollback_wide: self.scrollback_wide.clone(),
            scrollback_styles: self.scrollback_styles.clone(),
            combining: self.combining.clone(),
            scrollback_combining: self.scrollback_combining.clone(),
            links: self.links.clone(),
            current_link: self.current_link.clone(),
            saved_cursor: self.saved_cursor,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
        });
        self.cells = vec![vec![' '; self.columns as usize]; self.rows as usize];
        self.wide = vec![vec![false; self.columns as usize]; self.rows as usize];
        self.combining = vec![vec![String::new(); self.columns as usize]; self.rows as usize];
        self.styles =
            vec![vec![TerminalStyle::default(); self.columns as usize]; self.rows as usize];
        self.links = vec![vec![None; self.columns as usize]; self.rows as usize];
        self.scrollback.clear();
        self.scrollback_wide.clear();
        self.scrollback_combining.clear();
        self.scrollback_styles.clear();
        self.scroll_offset = 0;
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.current_link = None;
        self.saved_cursor = None;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.alternate = true;
    }

    fn leave_alternate(&mut self) {
        let Some(snapshot) = self.main_snapshot.take() else {
            return;
        };
        self.cells = snapshot.cells;
        self.wide = snapshot.wide;
        self.combining = snapshot.combining;
        self.styles = snapshot.styles;
        self.cursor_x = snapshot.cursor_x;
        self.cursor_y = snapshot.cursor_y;
        self.scrollback = snapshot.scrollback;
        self.scrollback_wide = snapshot.scrollback_wide;
        self.scrollback_styles = snapshot.scrollback_styles;
        self.scrollback_combining = snapshot.scrollback_combining;
        self.links = snapshot.links;
        self.current_link = snapshot.current_link;
        self.saved_cursor = snapshot.saved_cursor;
        self.scroll_top = snapshot.scroll_top;
        self.scroll_bottom = snapshot.scroll_bottom;
        self.scroll_offset = 0;
        self.alternate = false;
    }

    fn visible_range(&self) -> (usize, usize) {
        let total = self.scrollback.len() + self.cells.len();
        let end = total.saturating_sub(self.scroll_offset);
        (end.saturating_sub(self.rows as usize), end)
    }
}

pub struct TerminalState {
    input: String,
    output: Vec<String>,
    screen: TerminalScreen,
    session: Option<PtySession>,
    command: Option<Vec<String>>,
    cwd: Option<PathBuf>,
    scrollback: usize,
}

impl Default for TerminalState {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: Vec::new(),
            screen: TerminalScreen::new(80, 24),
            session: None,
            command: None,
            cwd: None,
            scrollback: 10_000,
        }
    }
}

impl TerminalState {
    pub fn with_command(command: Vec<String>, cwd: Option<PathBuf>) -> Self {
        Self {
            command: Some(command),
            cwd,
            ..Self::default()
        }
    }
    pub fn set_scrollback(&mut self, scrollback: usize) {
        self.scrollback = scrollback.max(1);
        self.screen.set_scrollback(self.scrollback);
        if self.output.len() > self.scrollback {
            let drain = self.output.len() - self.scrollback;
            self.output.drain(..drain);
        }
    }
    pub fn input(&self) -> &str {
        &self.input
    }
    pub fn screen_lines(&self) -> Vec<String> {
        self.screen.lines()
    }
    pub fn restore_screen(&mut self, lines: &[String]) {
        self.screen.clear();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                self.screen.feed(b"\r\n");
            }
            self.screen.feed(line.as_bytes());
        }
    }
    pub fn screen_styled_lines(&self) -> Vec<Vec<(char, TerminalStyle)>> {
        self.screen.styled_lines()
    }
    pub fn link_at(&self, column: u16, row: u16) -> Option<String> {
        self.screen.link_at(column, row).map(str::to_owned)
    }
    pub const fn application_cursor_keys(&self) -> bool {
        self.screen.application_cursor_keys()
    }
    pub const fn mouse_tracking(&self) -> bool {
        self.screen.mouse_tracking()
    }
    pub const fn mouse_sgr(&self) -> bool {
        self.screen.mouse_sgr()
    }
    pub const fn bracketed_paste(&self) -> bool {
        self.screen.bracketed_paste()
    }
    pub const fn alternate_screen(&self) -> bool {
        self.screen.alternate_screen()
    }
    pub fn push_char(&mut self, character: char) {
        self.input.push(character);
    }
    pub fn append_input(&mut self, text: &str) {
        self.input.push_str(text);
    }
    pub fn pop_char(&mut self) {
        self.input.pop();
    }
    pub fn clear_output(&mut self) {
        self.output.clear();
        self.screen.clear();
    }
    pub fn scroll(&mut self, delta: isize) {
        self.screen.scroll(delta);
    }

    pub fn send_mouse_event(
        &mut self,
        column: u16,
        row: u16,
        kind: MouseEventKind,
        modifiers: KeyModifiers,
    ) -> AppResult<bool> {
        if !self.mouse_tracking() {
            return Ok(false);
        }
        let Some(bytes) = encode_mouse_event(column, row, kind, modifiers, self.mouse_sgr()) else {
            return Ok(false);
        };
        self.write_bytes(&bytes)?;
        Ok(true)
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> AppResult<()> {
        self.ensure_session()?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| AppError::Terminal("PTY session unavailable".into()))?;
        session.writer.write_all(bytes)?;
        session.writer.flush()?;
        Ok(())
    }

    pub fn submit(&mut self) -> AppResult<()> {
        let command = std::mem::take(&mut self.input);
        if command.trim().is_empty() {
            return Ok(());
        }
        let mut bytes = command.into_bytes();
        bytes.push(b'\r');
        self.write_bytes(&bytes)
    }

    pub fn poll(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        while let Ok(bytes) = session.output.try_recv() {
            self.screen.feed(&bytes);
            let text = String::from_utf8_lossy(&bytes);
            self.output
                .extend(text.replace('\r', "").lines().map(str::to_owned));
            if self.output.len() > self.scrollback {
                let drain = self.output.len() - self.scrollback;
                self.output.drain(..drain);
            }
        }
        if session.child.try_wait().ok().flatten().is_some() {
            self.output.push("[terminal exited]".to_owned());
        }
    }

    pub fn resize(&mut self, columns: u16, rows: u16) -> AppResult<()> {
        self.screen.resize(columns, rows);
        if let Some(session) = &self.session {
            session
                .master
                .resize(PtySize {
                    rows,
                    cols: columns,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| AppError::Terminal(error.to_string()))?;
        }
        Ok(())
    }

    fn ensure_session(&mut self) -> AppResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        let mut command = if let Some(parts) = &self.command {
            let executable = parts
                .first()
                .ok_or_else(|| AppError::Terminal("agent command is empty".into()))?;
            let mut command = CommandBuilder::new(executable);
            command.args(&parts[1..]);
            command
        } else {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
            let mut command = CommandBuilder::new(shell);
            command.arg("-i");
            command
        };
        if let Some(cwd) = &self.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("mtide-pty-reader".to_owned())
            .spawn(move || read_pty(&mut reader, sender))
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        self.session = Some(PtySession {
            master: pair.master,
            writer,
            child,
            output: receiver,
        });
        Ok(())
    }
}

fn encode_mouse_event(
    column: u16,
    row: u16,
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    sgr: bool,
) -> Option<Vec<u8>> {
    let modifier_bits = u16::from(modifiers.contains(KeyModifiers::SHIFT)) * 4
        + u16::from(modifiers.contains(KeyModifiers::ALT)) * 8
        + u16::from(modifiers.contains(KeyModifiers::CONTROL)) * 16;
    let (button, release) = match kind {
        MouseEventKind::Down(MouseButton::Left) => (0, false),
        MouseEventKind::Down(MouseButton::Middle) => (1, false),
        MouseEventKind::Down(MouseButton::Right) => (2, false),
        MouseEventKind::Up(_) => (3, true),
        MouseEventKind::Drag(MouseButton::Left) => (32, false),
        MouseEventKind::Drag(MouseButton::Middle) => (33, false),
        MouseEventKind::Drag(MouseButton::Right) => (34, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    let button = button + modifier_bits;
    let suffix = if release { 'm' } else { 'M' };
    if sgr {
        Some(format!("\x1b[<{};{};{}{}", button, column + 1, row + 1, suffix).into_bytes())
    } else if column < 222 && row < 222 && button < 224 {
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (button + 32) as u8,
            (column + 33) as u8,
            (row + 33) as u8,
        ])
    } else {
        None
    }
}

fn read_pty(reader: &mut Box<dyn Read + Send>, sender: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                if sender.send(buffer[..size].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn pty_keeps_a_shell_session_and_returns_output() {
        let mut terminal = TerminalState::default();
        for character in "printf mtide-ok".chars() {
            terminal.push_char(character);
        }
        terminal.submit().expect("spawn pty");
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(5));
            terminal.poll();
            if terminal
                .screen_lines()
                .iter()
                .any(|line| line.contains("mtide-ok"))
            {
                break;
            }
        }
        assert!(terminal
            .screen_lines()
            .iter()
            .any(|line| line.contains("mtide-ok")));
    }

    #[test]
    fn terminal_screen_handles_cursor_and_clear_sequences() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.feed(b"hello\x1b[2;1Hworld\x1b[2K");
        assert_eq!(screen.lines(), vec!["hello   ", "        "]);
        screen.feed(b"ok");
        assert_eq!(screen.lines()[1], "     ok ");
    }

    #[test]
    fn terminal_screen_uses_default_erase_range_and_clears_cell_metadata() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.feed(b"ab\x1b]8;;https://example.test\x07c\x1b]8;;\x07");
        assert_eq!(screen.lines()[0], "abc     ");
        assert_eq!(screen.link_at(2, 0), Some("https://example.test"));
        screen.feed(b"\x1b[1;2H\x1b[K");
        assert_eq!(screen.lines()[0], "a       ");
        assert_eq!(screen.link_at(2, 0), None);
        screen.feed(b"\x1b[1;1H\x1b[J");
        assert_eq!(screen.lines(), vec!["        ", "        "]);
    }

    #[test]
    fn terminal_screen_supports_insert_delete_and_erase_characters() {
        let mut screen = TerminalScreen::new(8, 1);
        screen.feed(b"abcd\x1b[1;2H\x1b[2@xy");
        assert_eq!(screen.lines()[0], "axybcd  ");
        screen.feed(b"\x1b[1;3H\x1b[P");
        assert_eq!(screen.lines()[0], "axbcd   ");
        screen.feed(b"\x1b[1;3H\x1b[2X");
        assert_eq!(screen.lines()[0], "ax  d   ");
    }

    #[test]
    fn overwriting_a_wide_cell_clears_all_continuation_metadata() {
        let mut screen = TerminalScreen::new(8, 1);
        screen.feed(b"a\x1b]8;;https://example.test\x07\x1b[31m\xe7\x95\x8c\x1b]8;;\x07");
        assert_eq!(screen.link_at(1, 0), Some("https://example.test"));
        assert_eq!(screen.link_at(2, 0), Some("https://example.test"));
        screen.feed(b"\x1b[1;3H\x1b[X");
        assert_eq!(screen.link_at(1, 0), None);
        assert_eq!(screen.link_at(2, 0), None);
        assert_eq!(screen.styled_lines()[0][1].1.foreground, None);
        assert_eq!(screen.styled_lines()[0][2].1.foreground, None);
    }

    #[test]
    fn terminal_screen_restores_saved_cursor_across_escape_forms_and_alternate_screen() {
        let mut screen = TerminalScreen::new(8, 2);
        assert!(!screen.alternate_screen());
        screen.feed(b"abc\x1b[2;3H\x1b[s\x1b[1;1H\x1b[uX");
        assert_eq!(screen.lines()[1], "  X     ");
        screen.feed(b"\x1b[?1049h\x1b[1;1Halt");
        assert!(screen.alternate_screen());
        screen.feed(b"\x1b[?1049l");
        assert!(!screen.alternate_screen());
        screen.feed(b"\x1b7\x1b[1;1HQ\x1b8Y");
        assert_eq!(screen.lines()[1], "  XY    ");
        assert_eq!(screen.lines()[0].chars().next(), Some('Q'));
    }

    #[test]
    fn terminal_screen_scrolls_only_inside_configured_scroll_region() {
        let mut screen = TerminalScreen::new(4, 4);
        screen.feed(b"\x1b[2;3r\x1b[2;1Ha\r\nb\r\nc");
        assert_eq!(screen.lines(), vec!["    ", "b   ", "c   ", "    "]);
        assert!(screen.scrollback.is_empty());
    }

    #[test]
    fn terminal_screen_edits_and_scrolls_rows_inside_scroll_region() {
        let mut screen = TerminalScreen::new(4, 4);
        screen.feed(b"\x1b[2;3r\x1b[2;1Ha\r\nb");
        screen.feed(b"\x1b[2;1H\x1b[L");
        assert_eq!(screen.lines(), vec!["    ", "    ", "a   ", "    "]);
        screen.feed(b"\x1b[3;1H\x1b[M");
        assert_eq!(screen.lines(), vec!["    ", "    ", "    ", "    "]);
        screen.feed(b"\x1b[2;1Hx\r\ny\x1b[1S");
        assert_eq!(screen.lines(), vec!["    ", "y   ", "    ", "    "]);
        screen.feed(b"\x1b[1T");
        assert_eq!(screen.lines(), vec!["    ", "    ", "y   ", "    "]);
    }

    #[test]
    fn terminal_screen_advances_wide_characters_by_two_columns() {
        let mut screen = TerminalScreen::new(6, 2);
        screen.feed("a你好b".as_bytes());
        assert_eq!(screen.lines()[0], "a你 好 b");
        assert_eq!((screen.cursor_x, screen.cursor_y), (0, 1));
    }

    #[test]
    fn terminal_screen_backspace_and_overwrite_treat_wide_cells_as_one_character() {
        let mut screen = TerminalScreen::new(8, 1);
        screen.feed("你".as_bytes());
        screen.feed(b"\x08x");
        assert_eq!(screen.lines()[0], "x       ");
        let mut erased = TerminalScreen::new(8, 1);
        erased.feed("你".as_bytes());
        erased.feed(b"\x1b[1;2H\x1b[X");
        assert_eq!(erased.lines()[0], "        ");
    }

    #[test]
    fn terminal_screen_attaches_combining_marks_to_previous_cell() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.feed("e\u{301}x".as_bytes());
        assert_eq!(screen.lines()[0], "e\u{301}x      ");
        assert_eq!(screen.styled_lines()[0][0].0, 'e');
        assert_eq!(screen.styled_lines()[0][1].0, '\u{301}');
    }

    #[test]
    fn terminal_screen_keeps_utf8_codepoints_split_across_reads() {
        let mut screen = TerminalScreen::new(8, 1);
        let bytes = "你".as_bytes();
        screen.feed(&bytes[..1]);
        assert_eq!(screen.lines()[0], "        ");
        screen.feed(&bytes[1..2]);
        assert_eq!(screen.lines()[0], "        ");
        screen.feed(&bytes[2..]);
        assert_eq!(screen.lines()[0], "你       ");
    }

    #[test]
    fn terminal_screen_tracks_application_cursor_mode() {
        let mut screen = TerminalScreen::new(8, 2);
        assert!(!screen.application_cursor_keys());
        screen.feed(b"\x1b[?1h");
        assert!(screen.application_cursor_keys());
        screen.feed(b"\x1b[?1l");
        assert!(!screen.application_cursor_keys());
    }

    #[test]
    fn terminal_screen_tracks_bracketed_paste_mode() {
        let mut screen = TerminalScreen::new(8, 2);
        assert!(!screen.bracketed_paste());
        screen.feed(b"\x1b[?2004h");
        assert!(screen.bracketed_paste());
        screen.feed(b"\x1b[?2004l");
        assert!(!screen.bracketed_paste());
    }

    #[test]
    fn terminal_screen_renders_cursor_visibility_as_inverse_style() {
        let mut screen = TerminalScreen::new(8, 1);
        screen.feed(b"x");
        assert!(screen.styled_lines()[0][1].1.inverse);
        screen.feed(b"\x1b[?25l");
        assert!(!screen.styled_lines()[0][1].1.inverse);
        screen.feed(b"\x1b[?25h");
        assert!(screen.styled_lines()[0][1].1.inverse);
    }

    #[test]
    fn terminal_screen_tracks_mouse_modes_and_sgr_encoding() {
        let mut screen = TerminalScreen::new(80, 24);
        assert!(!screen.mouse_tracking());
        screen.feed(b"\x1b[?1002h\x1b[?1006h");
        assert!(screen.mouse_tracking());
        assert!(screen.mouse_sgr());
        assert_eq!(
            encode_mouse_event(
                4,
                6,
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::SHIFT,
                true,
            ),
            Some(b"\x1b[<4;5;7M".to_vec())
        );
        screen.feed(b"\x1b[?1002l\x1b[?1006l");
        assert!(!screen.mouse_tracking());
        assert!(!screen.mouse_sgr());
    }

    #[test]
    fn terminal_screen_preserves_indexed_and_truecolor_styles() {
        let mut screen = TerminalScreen::new(12, 1);
        screen.feed(b"\x1b[38;5;200;48;2;1;2;3mX");
        let style = screen.styled_lines()[0][0].1;
        assert_eq!(style.foreground, Some(AnsiColor::Indexed(200)));
        assert_eq!(style.background, Some(AnsiColor::Rgb(1, 2, 3)));
    }

    #[test]
    fn terminal_screen_clamps_out_of_range_truecolor_components() {
        let mut screen = TerminalScreen::new(4, 1);
        screen.feed(b"\x1b[38;2;300;256;999mX");
        assert_eq!(
            screen.styled_lines()[0][0].1.foreground,
            Some(AnsiColor::Rgb(255, 255, 255))
        );
    }

    #[test]
    fn terminal_screen_tracks_osc8_hyperlinks_by_cell() {
        let mut screen = TerminalScreen::new(16, 2);
        screen.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07plain");
        assert_eq!(screen.link_at(0, 0), Some("https://example.com"));
        assert_eq!(screen.link_at(3, 0), Some("https://example.com"));
        assert_eq!(screen.link_at(4, 0), None);
    }

    #[test]
    fn legacy_mouse_encoding_uses_x10_coordinates() {
        assert_eq!(
            encode_mouse_event(1, 2, MouseEventKind::ScrollDown, KeyModifiers::NONE, false,),
            Some(vec![0x1b, b'[', b'M', 97, 34, 35])
        );
    }

    #[test]
    fn terminal_screen_preserves_main_screen_across_alternate_buffer() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.feed(b"\x1b]8;;https://example.com\x07main\x1b]8;;\x07");
        screen.feed(b"\x1b[?1049halt");
        assert!(screen.lines()[0].starts_with("alt"));
        screen.feed(b"\x1b[?1049l");
        assert!(screen.lines()[0].starts_with("main"));
        assert_eq!(screen.link_at(0, 0), Some("https://example.com"));
    }

    #[test]
    fn terminal_screen_retains_scrollback_when_scrolling() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.set_scrollback(2);
        screen.feed(b"one\ntwo\nthree\nfour");
        assert!(screen.scrollback.len() <= 2);
        assert!(screen.scrollback.iter().any(|line| line.contains(&'t')));
    }

    #[test]
    fn terminal_screen_scrolls_history_and_returns_to_bottom() {
        let mut screen = TerminalScreen::new(8, 2);
        screen.feed(b"one\ntwo\nthree\nfour");
        let bottom = screen.lines();
        screen.scroll(1);
        assert_ne!(screen.lines(), bottom);
        screen.scroll(-1);
        assert_eq!(screen.lines(), bottom);
    }
}
