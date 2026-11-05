use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::error::AppResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndentInfo {
    pub use_tabs: bool,
    pub has_tabs: bool,
    pub size: Option<usize>,
}

pub fn detect_indent(lines: &[String]) -> IndentInfo {
    let mut tabs = 0usize;
    let mut spaces = 0usize;
    let mut diffs = [0usize; 9];
    let mut previous = 0usize;
    for line in lines.iter().take(200) {
        let prefix: String = line
            .chars()
            .take_while(|character| *character == ' ' || *character == '\t')
            .collect();
        if prefix.is_empty() {
            previous = 0;
            continue;
        }
        if prefix.contains('\t') {
            tabs += 1;
        } else {
            spaces += 1;
        }
        let diff = prefix.chars().count().abs_diff(previous);
        if (1..=8).contains(&diff) {
            diffs[diff] += 1;
        }
        previous = prefix.chars().count();
    }
    let size = (1..=8)
        .filter(|&candidate| diffs[candidate] > 0)
        .max_by_key(|&candidate| (diffs[candidate], std::cmp::Reverse(candidate)));
    IndentInfo {
        use_tabs: tabs > spaces,
        has_tabs: tabs > 0,
        size,
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Position,
    state_id: u64,
}

#[derive(Debug)]
pub struct Buffer {
    lines: Vec<String>,
    cursor: Position,
    path: Option<PathBuf>,
    language_override: Option<String>,
    saved_lines: Vec<String>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    disk_mtime: Option<SystemTime>,
    external_change: bool,
    readonly: bool,
    undo_group_depth: usize,
    undo_group_checkpointed: bool,
    indent_info: IndentInfo,
    state_id: u64,
    next_state_id: u64,
    saved_state_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalChange {
    Reloaded,
    Dirty,
    Deleted,
}

impl Buffer {
    pub fn replace_document_text(&mut self, text: &str) {
        if self.readonly {
            return;
        }
        self.checkpoint();
        self.lines = split_lines(text);
        self.cursor.line = self.cursor.line.min(self.lines.len().saturating_sub(1));
        self.cursor.column = self.cursor.column.min(
            self.lines
                .get(self.cursor.line)
                .map_or(0, |line| line.chars().count()),
        );
    }

    pub fn welcome() -> Self {
        Self::new(
            vec![
                "# mtide".into(),
                "".into(),
                "Rust Terminal IDE compatible with TTT.".into(),
                "".into(),
                "The UI shell is ready for editor, workspace, Git, LSP, terminal,".into(),
                "plugin, and AI Agent services.".into(),
                "".into(),
                "Ctrl+Q  Quit    Tab  Next focus    Ctrl+B  Sidebar    Ctrl+T  Terminal".into(),
            ],
            None,
        )
    }

    pub fn open(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref().to_path_buf();
        let text = fs::read_to_string(&path)?;
        Ok(Self::new(split_lines(&text), Some(path)))
    }

    pub(crate) fn from_text(text: &str) -> Self {
        Self::new(split_lines(text), None)
    }

    pub(crate) fn new(lines: Vec<String>, path: Option<PathBuf>) -> Self {
        let lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        let disk_mtime = path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok());
        let indent_info = detect_indent(&lines);
        Self {
            saved_lines: lines.clone(),
            lines,
            cursor: Position { line: 0, column: 0 },
            path,
            language_override: None,
            undo: Vec::new(),
            redo: Vec::new(),
            disk_mtime,
            external_change: false,
            readonly: false,
            undo_group_depth: 0,
            undo_group_checkpointed: false,
            indent_info,
            state_id: 0,
            next_state_id: 1,
            saved_state_id: Some(0),
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }
    pub const fn cursor(&self) -> Position {
        self.cursor
    }
    pub fn file_name(&self) -> &str {
        self.path
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or("welcome.md")
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub const fn has_external_change(&self) -> bool {
        self.external_change
    }

    pub(crate) fn relocate(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        self.disk_mtime = fs::metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        self.external_change = false;
        self.path = Some(path);
    }
    pub const fn indent_info(&self) -> IndentInfo {
        self.indent_info
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
    pub const fn is_readonly(&self) -> bool {
        self.readonly
    }
    pub fn language_override(&self) -> Option<&str> {
        self.language_override.as_deref()
    }
    pub fn set_language_override(&mut self, language: Option<String>) {
        self.language_override = language.filter(|language| !language.trim().is_empty());
    }
    pub(crate) fn poll_external_change(&mut self) -> AppResult<Option<ExternalChange>> {
        let Some(path) = self.path.as_deref() else {
            return Ok(None);
        };
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.disk_mtime.is_none() && self.external_change {
                    return Ok(None);
                }
                self.disk_mtime = None;
                self.external_change = true;
                return Ok(Some(if self.dirty() {
                    ExternalChange::Dirty
                } else {
                    ExternalChange::Deleted
                }));
            }
            Err(error) => return Err(error.into()),
        };
        let modified = metadata.modified().ok();
        if modified == self.disk_mtime {
            return Ok(None);
        }
        self.disk_mtime = modified;
        if self.dirty() {
            self.external_change = true;
            return Ok(Some(ExternalChange::Dirty));
        }
        let text = fs::read_to_string(path)?;
        self.lines = split_lines(&text);
        self.indent_info = detect_indent(&self.lines);
        self.saved_lines = self.lines.clone();
        self.undo.clear();
        self.redo.clear();
        self.state_id = 0;
        self.next_state_id = 1;
        self.saved_state_id = Some(self.state_id);
        self.cursor.line = self.cursor.line.min(self.lines.len().saturating_sub(1));
        self.cursor.column = self
            .cursor
            .column
            .min(self.lines[self.cursor.line].chars().count());
        self.external_change = false;
        Ok(Some(ExternalChange::Reloaded))
    }
    pub(crate) fn reload_from_disk(&mut self) -> AppResult<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let text = fs::read_to_string(&path)?;
        self.lines = split_lines(&text);
        self.indent_info = detect_indent(&self.lines);
        self.saved_lines = self.lines.clone();
        self.undo.clear();
        self.redo.clear();
        self.state_id = 0;
        self.next_state_id = 1;
        self.saved_state_id = Some(self.state_id);
        self.disk_mtime = fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        self.cursor.line = self.cursor.line.min(self.lines.len().saturating_sub(1));
        self.cursor.column = self
            .cursor
            .column
            .min(self.lines[self.cursor.line].chars().count());
        self.external_change = false;
        Ok(true)
    }
    pub fn char_at_cursor(&self) -> Option<char> {
        self.lines
            .get(self.cursor.line)
            .and_then(|line| line.chars().nth(self.cursor.column))
    }
    pub fn text_between(&self, first: Position, second: Position) -> String {
        let (start, end) = ordered_positions(first, second);
        if start.line == end.line {
            return self.lines[start.line]
                .chars()
                .skip(start.column)
                .take(end.column.saturating_sub(start.column))
                .collect();
        }
        let mut lines = Vec::new();
        lines.push(self.lines[start.line].chars().skip(start.column).collect());
        for line in (start.line + 1)..end.line {
            lines.push(self.lines[line].clone());
        }
        lines.push(self.lines[end.line].chars().take(end.column).collect());
        lines.join("\n")
    }
    pub fn delete_between(&mut self, first: Position, second: Position) {
        if self.readonly {
            return;
        }
        let (start, end) = ordered_positions(first, second);
        if start == end {
            return;
        }
        self.checkpoint();
        if start.line == end.line {
            let line = &mut self.lines[start.line];
            let start_byte = line
                .char_indices()
                .nth(start.column)
                .map_or(line.len(), |(i, _)| i);
            let end_byte = line
                .char_indices()
                .nth(end.column)
                .map_or(line.len(), |(i, _)| i);
            line.replace_range(start_byte..end_byte, "");
        } else {
            let prefix: String = self.lines[start.line].chars().take(start.column).collect();
            let suffix: String = self.lines[end.line].chars().skip(end.column).collect();
            self.lines
                .splice(start.line..=end.line, [format!("{prefix}{suffix}")]);
        }
        self.cursor = start;
    }
    pub fn replace_ranges(&mut self, ranges: &[(Position, Position)], replacement: &str) -> usize {
        if self.readonly {
            return 0;
        }
        let mut ranges: Vec<_> = ranges
            .iter()
            .map(|(first, second)| ordered_positions(*first, *second))
            .filter(|(start, end)| start != end || !replacement.is_empty())
            .collect();
        if ranges.is_empty() {
            return 0;
        }
        ranges.sort_by_key(|(start, _)| (start.line, start.column));
        let mut merged: Vec<(Position, Position)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some((_, previous_end)) = merged.last_mut() {
                if (start.line, start.column) <= (previous_end.line, previous_end.column) {
                    if (end.line, end.column) > (previous_end.line, previous_end.column) {
                        *previous_end = end;
                    }
                } else {
                    merged.push((start, end));
                }
            } else {
                merged.push((start, end));
            }
        }
        self.checkpoint();
        for (start, end) in merged.iter().rev() {
            self.replace_between_unchecked(*start, *end, replacement);
        }
        merged.len()
    }

    pub(crate) fn apply_text_edits(&mut self, edits: &[(Position, Position, String)]) -> usize {
        if self.readonly || edits.is_empty() {
            return 0;
        }
        let mut edits = edits.to_vec();
        edits.sort_by_key(|(start, end, _)| {
            std::cmp::Reverse((start.line, start.column, end.line, end.column))
        });
        let count = edits.len();
        self.checkpoint();
        for (start, end, text) in edits {
            self.replace_between_unchecked(start, end, &text);
        }
        count
    }
    fn replace_between_unchecked(&mut self, start: Position, end: Position, replacement: &str) {
        if start.line == end.line {
            let line = &mut self.lines[start.line];
            let start_byte = line
                .char_indices()
                .nth(start.column)
                .map_or(line.len(), |(i, _)| i);
            let end_byte = line
                .char_indices()
                .nth(end.column)
                .map_or(line.len(), |(i, _)| i);
            line.replace_range(start_byte..end_byte, replacement);
        } else {
            let prefix: String = self.lines[start.line].chars().take(start.column).collect();
            let suffix: String = self.lines[end.line].chars().skip(end.column).collect();
            let mut replacement_lines = split_lines(&format!("{prefix}{replacement}{suffix}"));
            self.lines
                .splice(start.line..=end.line, replacement_lines.drain(..));
        }
        let replacement_lines = split_lines(replacement);
        self.cursor = Position {
            line: start.line + replacement_lines.len().saturating_sub(1),
            column: if replacement_lines.len() == 1 {
                start.column + replacement.chars().count()
            } else {
                replacement_lines
                    .last()
                    .map_or(0, |line| line.chars().count())
            },
        };
    }
    pub fn dirty(&self) -> bool {
        self.saved_state_id != Some(self.state_id) || self.lines != self.saved_lines
    }

    pub fn has_unresolved_conflicts(&self) -> bool {
        self.lines.iter().any(|line| {
            line.starts_with("<<<<<<<")
                || line.starts_with("=======")
                || line.starts_with(">>>>>>>")
        })
    }

    pub fn insert(&mut self, text: &str) {
        if self.readonly {
            return;
        }
        self.checkpoint();
        for character in text.chars() {
            if character == '\n' {
                let byte = self.char_to_byte();
                let remainder = self.lines[self.cursor.line].split_off(byte);
                self.lines.insert(self.cursor.line + 1, remainder);
                self.cursor.line += 1;
                self.cursor.column = 0;
            } else {
                let byte = self.char_to_byte();
                self.lines[self.cursor.line].insert(byte, character);
                self.cursor.column += 1;
            }
        }
    }

    pub fn insert_at_positions(&mut self, positions: &[Position], text: &str) -> Vec<Position> {
        if self.readonly {
            return positions.to_vec();
        }
        if positions.is_empty() || text.is_empty() {
            return positions.to_vec();
        }
        self.checkpoint();
        let mut order: Vec<usize> = (0..positions.len()).collect();
        order.sort_by_key(|&index| {
            std::cmp::Reverse((positions[index].line, positions[index].column))
        });
        let mut result = positions.to_vec();
        for index in order {
            let position = positions[index];
            let newline_count = text.chars().filter(|&character| character == '\n').count();
            let suffix_column = text
                .rsplit('\n')
                .next()
                .map_or(0, |line| line.chars().count());
            let inserted_columns = text.chars().count();
            for (other, other_position) in result.iter_mut().enumerate() {
                if other == index {
                    continue;
                }
                if other_position.line > position.line {
                    other_position.line += newline_count;
                } else if other_position.line == position.line
                    && other_position.column >= position.column
                {
                    if newline_count > 0 {
                        other_position.line += newline_count;
                        other_position.column =
                            suffix_column + other_position.column.saturating_sub(position.column);
                    } else {
                        other_position.column += inserted_columns;
                    }
                }
            }
            if text.contains('\n') {
                let line = &self.lines[position.line];
                let byte = line
                    .char_indices()
                    .nth(position.column)
                    .map_or(line.len(), |(offset, _)| offset);
                let replacement = format!("{}{}{}", &line[..byte], text, &line[byte..]);
                let replacement_lines = split_lines(&replacement);
                self.lines.splice(
                    position.line..=position.line,
                    replacement_lines.iter().cloned(),
                );
                result[index] = Position {
                    line: position.line + replacement_lines.len().saturating_sub(1),
                    column: if replacement_lines.len() == 1 {
                        position.column + text.chars().count()
                    } else {
                        text.rsplit('\n')
                            .next()
                            .map_or(0, |line| line.chars().count())
                    },
                };
            } else {
                let line = &mut self.lines[position.line];
                let byte = line
                    .char_indices()
                    .nth(position.column)
                    .map_or(line.len(), |(offset, _)| offset);
                line.insert_str(byte, text);
                result[index].column += text.chars().count();
            }
        }
        result
    }

    pub fn backspace_at_positions(&mut self, positions: &[Position]) -> Vec<Position> {
        if self.readonly || positions.is_empty() {
            return positions.to_vec();
        }
        self.checkpoint();
        let mut order: Vec<usize> = (0..positions.len()).collect();
        order.sort_by_key(|&index| {
            std::cmp::Reverse((positions[index].line, positions[index].column))
        });
        let mut result = positions.to_vec();
        for index in order {
            let position = positions[index];
            if position.column > 0 {
                let line = &mut self.lines[position.line];
                let byte = line
                    .char_indices()
                    .nth(position.column - 1)
                    .map_or(0, |(offset, _)| offset);
                line.remove(byte);
                for (other, other_position) in result.iter_mut().enumerate() {
                    if other != index
                        && other_position.line == position.line
                        && other_position.column > position.column
                    {
                        other_position.column -= 1;
                    }
                }
                result[index].column -= 1;
            } else if position.line > 0 {
                let current = self.lines.remove(position.line);
                let previous_column = self.lines[position.line - 1].chars().count();
                let previous = &mut self.lines[position.line - 1];
                previous.push_str(&current);
                for (other, other_position) in result.iter_mut().enumerate() {
                    if other == index {
                        continue;
                    }
                    if other_position.line > position.line {
                        other_position.line -= 1;
                    } else if other_position.line == position.line {
                        other_position.line -= 1;
                        other_position.column += previous_column;
                    }
                }
                result[index] = Position {
                    line: position.line - 1,
                    column: previous_column,
                };
            }
        }
        result
    }

    pub fn delete_at_positions(&mut self, positions: &[Position]) -> Vec<Position> {
        if self.readonly {
            return positions.to_vec();
        }
        if positions.is_empty() {
            return positions.to_vec();
        }
        self.checkpoint();
        let mut order: Vec<usize> = (0..positions.len()).collect();
        order.sort_by_key(|&index| {
            std::cmp::Reverse((positions[index].line, positions[index].column))
        });
        let mut result = positions.to_vec();
        for index in order {
            let position = positions[index];
            let line_len = self.lines[position.line].chars().count();
            if position.column < line_len {
                let line = &mut self.lines[position.line];
                let byte = line
                    .char_indices()
                    .nth(position.column)
                    .map_or(line.len(), |(offset, _)| offset);
                line.remove(byte);
                for (other, other_position) in result.iter_mut().enumerate() {
                    if other != index
                        && other_position.line == position.line
                        && other_position.column > position.column
                    {
                        other_position.column -= 1;
                    }
                }
            } else if position.line + 1 < self.lines.len() {
                let current_len = line_len;
                let next = self.lines.remove(position.line + 1);
                self.lines[position.line].push_str(&next);
                for (other, other_position) in result.iter_mut().enumerate() {
                    if other == index {
                        continue;
                    }
                    if other_position.line > position.line + 1 {
                        other_position.line -= 1;
                    } else if other_position.line == position.line + 1 {
                        other_position.line = position.line;
                        other_position.column += current_len;
                    }
                }
            }
        }
        result
    }

    /// Deletes the word immediately before the cursor, including the
    /// whitespace separating it from the previous word.  The operation uses
    /// rune columns and therefore remains safe for UTF-8 text.
    pub fn delete_word_backward(&mut self) {
        if self.readonly {
            return;
        }
        let cursor = self.cursor;
        if cursor.column == 0 && cursor.line == 0 {
            return;
        }
        let start = if cursor.column > 0 {
            let characters: Vec<char> = self.lines[cursor.line].chars().collect();
            let mut column = cursor.column.min(characters.len());
            while column > 0 && characters[column - 1].is_whitespace() {
                column -= 1;
            }
            while column > 0 && !characters[column - 1].is_whitespace() {
                column -= 1;
            }
            Position {
                line: cursor.line,
                column,
            }
        } else {
            let previous_line = cursor.line - 1;
            let characters: Vec<char> = self.lines[previous_line].chars().collect();
            let mut column = characters.len();
            while column > 0 && characters[column - 1].is_whitespace() {
                column -= 1;
            }
            while column > 0 && !characters[column - 1].is_whitespace() {
                column -= 1;
            }
            Position {
                line: previous_line,
                column,
            }
        };
        self.delete_between(start, cursor);
    }

    /// Deletes the word immediately after the cursor, including the
    /// separating whitespace.  At the end of a line it also removes the line
    /// break so the next line is joined naturally.
    pub fn delete_word_forward(&mut self) {
        if self.readonly {
            return;
        }
        let cursor = self.cursor;
        let line = &self.lines[cursor.line];
        let characters: Vec<char> = line.chars().collect();
        if cursor.column < characters.len() {
            let mut column = cursor.column;
            while column < characters.len() && characters[column].is_whitespace() {
                column += 1;
            }
            while column < characters.len() && !characters[column].is_whitespace() {
                column += 1;
            }
            self.delete_between(
                cursor,
                Position {
                    line: cursor.line,
                    column,
                },
            );
        } else if cursor.line + 1 < self.lines.len() {
            self.delete_between(
                cursor,
                Position {
                    line: cursor.line + 1,
                    column: 0,
                },
            );
        }
    }

    pub fn backspace(&mut self) {
        if self.readonly {
            return;
        }
        if self.cursor.column == 0 && self.cursor.line == 0 {
            return;
        }
        self.checkpoint();
        if self.cursor.column > 0 {
            self.cursor.column -= 1;
            let byte = self.char_to_byte();
            self.lines[self.cursor.line].remove(byte);
        } else {
            let current = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.column = self.lines[self.cursor.line].chars().count();
            self.lines[self.cursor.line].push_str(&current);
        }
    }

    pub fn delete(&mut self) {
        if self.readonly {
            return;
        }
        let len = self.lines[self.cursor.line].chars().count();
        if self.cursor.column == len && self.cursor.line + 1 >= self.lines.len() {
            return;
        }
        self.checkpoint();
        if self.cursor.column < len {
            let byte = self.char_to_byte();
            self.lines[self.cursor.line].remove(byte);
        } else {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
    }

    pub fn move_cursor(&mut self, line_delta: isize, column_delta: isize) {
        let line = (self.cursor.line as isize + line_delta).clamp(0, self.lines.len() as isize - 1)
            as usize;
        let max = self.lines[line].chars().count();
        let column = (self.cursor.column as isize + column_delta).clamp(0, max as isize) as usize;
        self.cursor = Position { line, column };
    }

    pub fn move_word_left(&mut self) {
        if self.cursor.column > 0 {
            let characters: Vec<char> = self.lines[self.cursor.line].chars().collect();
            let mut column = self.cursor.column.min(characters.len());
            while column > 0 && characters[column - 1].is_whitespace() {
                column -= 1;
            }
            while column > 0 && !characters[column - 1].is_whitespace() {
                column -= 1;
            }
            self.cursor.column = column;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.column = self.lines[self.cursor.line].chars().count();
        }
    }

    pub fn move_word_right(&mut self) {
        let characters: Vec<char> = self.lines[self.cursor.line].chars().collect();
        if self.cursor.column < characters.len() {
            let mut column = self.cursor.column;
            while column < characters.len() && characters[column].is_whitespace() {
                column += 1;
            }
            while column < characters.len() && !characters[column].is_whitespace() {
                column += 1;
            }
            self.cursor.column = column;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.column = 0;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor.column = 0;
    }
    pub fn set_cursor(&mut self, line: usize, column: usize) {
        let line = line.min(self.lines.len().saturating_sub(1));
        let column = column.min(self.lines[line].chars().count());
        self.cursor = Position { line, column };
    }
    pub fn move_end(&mut self) {
        self.cursor.column = self.lines[self.cursor.line].chars().count();
    }
    pub fn move_file_start(&mut self) {
        self.cursor = Position { line: 0, column: 0 };
    }
    pub fn move_file_end(&mut self) {
        let line = self.lines.len().saturating_sub(1);
        self.cursor = Position {
            line,
            column: self.lines[line].chars().count(),
        };
    }

    pub fn undo(&mut self) {
        if self.readonly {
            return;
        }
        if let Some(snapshot) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(snapshot);
        }
    }
    pub fn redo(&mut self) {
        if self.readonly {
            return;
        }
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(snapshot);
        }
    }

    pub fn begin_undo_group(&mut self) {
        self.undo_group_depth += 1;
    }

    pub fn end_undo_group(&mut self) {
        self.undo_group_depth = self.undo_group_depth.saturating_sub(1);
        if self.undo_group_depth == 0 {
            self.undo_group_checkpointed = false;
        }
    }

    pub fn save(&mut self) -> AppResult<()> {
        self.save_with_options(false, false)
    }

    pub fn save_with_options(
        &mut self,
        trim_trailing_whitespace: bool,
        insert_final_newline: bool,
    ) -> AppResult<()> {
        if self.readonly {
            return Err(crate::error::AppError::Workspace(
                "buffer is read-only".to_owned(),
            ));
        }
        if let Some(path) = &self.path {
            if trim_trailing_whitespace {
                for line in &mut self.lines {
                    let trimmed = line.trim_end_matches(char::is_whitespace).to_owned();
                    *line = trimmed;
                }
            }
            let mut text = self.lines.join("\n");
            if insert_final_newline && !text.ends_with('\n') {
                text.push('\n');
            }
            fs::write(path, text)?;
            self.saved_lines = self.lines.clone();
            self.saved_state_id = Some(self.state_id);
            self.disk_mtime = fs::metadata(path)
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            self.external_change = false;
        }
        Ok(())
    }
    pub fn save_as(
        &mut self,
        path: impl AsRef<Path>,
        trim_trailing_whitespace: bool,
        insert_final_newline: bool,
    ) -> AppResult<()> {
        if self.readonly {
            return Err(crate::error::AppError::Workspace(
                "buffer is read-only".to_owned(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        if trim_trailing_whitespace {
            for line in &mut self.lines {
                let trimmed = line.trim_end_matches(char::is_whitespace).to_owned();
                *line = trimmed;
            }
        }
        let mut text = self.lines.join("\n");
        if insert_final_newline && !text.ends_with('\n') {
            text.push('\n');
        }
        fs::write(&path, text)?;
        self.path = Some(path);
        self.saved_lines = self.lines.clone();
        self.saved_state_id = Some(self.state_id);
        self.disk_mtime = self
            .path
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok());
        self.external_change = false;
        Ok(())
    }
    pub fn replace_all(&mut self, needle: &str, replacement: &str) -> usize {
        if self.readonly {
            return 0;
        }
        if needle.is_empty() {
            return 0;
        }
        let count = self
            .lines
            .iter()
            .map(|line| line.matches(needle).count())
            .sum();
        if count > 0 {
            self.checkpoint();
            for line in &mut self.lines {
                *line = line.replace(needle, replacement);
            }
        }
        count
    }
    pub fn replace_all_smart_case(&mut self, needle: &str, replacement: &str) -> usize {
        if self.readonly {
            return 0;
        }
        if needle.is_empty() {
            return 0;
        }
        let case_sensitive = needle.chars().any(char::is_uppercase);
        let mut count = 0;
        let mut replacements = Vec::with_capacity(self.lines.len());
        for line in &self.lines {
            let (replacement_line, line_count) = if case_sensitive {
                (
                    line.replace(needle, replacement),
                    line.matches(needle).count(),
                )
            } else {
                replace_line_case_insensitive(line, needle, replacement)
            };
            replacements.push(replacement_line);
            count += line_count;
        }
        if count > 0 {
            self.checkpoint();
            self.lines = replacements;
        }
        count
    }
    pub fn duplicate_line(&mut self) {
        if self.readonly {
            return;
        }
        self.checkpoint();
        let line = self.lines[self.cursor.line].clone();
        self.lines.insert(self.cursor.line + 1, line);
        self.cursor.line += 1;
    }

    pub fn move_lines(&mut self, start: usize, end: usize, upward: bool) -> bool {
        if self.readonly || start >= end || end > self.lines.len() {
            return false;
        }
        if upward {
            if start == 0 {
                return false;
            }
            self.checkpoint();
            let previous = self.lines.remove(start - 1);
            self.lines.insert(end - 1, previous);
            self.cursor.line = if (start..end).contains(&self.cursor.line) {
                self.cursor.line.saturating_sub(1)
            } else if self.cursor.line == start - 1 {
                end - 1
            } else {
                self.cursor.line
            };
        } else {
            if end >= self.lines.len() {
                return false;
            }
            self.checkpoint();
            let next = self.lines.remove(end);
            self.lines.insert(start, next);
            self.cursor.line = if (start..end).contains(&self.cursor.line) {
                self.cursor.line + 1
            } else if self.cursor.line == end {
                start
            } else {
                self.cursor.line
            };
        }
        true
    }

    pub fn join_lines(&mut self, start: usize, end: usize) -> bool {
        if self.readonly || end.saturating_sub(start) < 2 || end > self.lines.len() {
            return false;
        }
        self.checkpoint();
        let mut joined = self.lines[start].trim_end().to_owned();
        for line in &self.lines[start + 1..end] {
            let right = line.trim_start();
            if !joined.is_empty() && !right.is_empty() {
                joined.push(' ');
            }
            joined.push_str(right);
        }
        self.lines.splice(start..end, [joined]);
        self.cursor.line = start;
        self.cursor.column = self.cursor.column.min(self.lines[start].chars().count());
        true
    }

    pub fn sort_lines(&mut self, start: usize, end: usize, reverse: bool) -> bool {
        if self.readonly || end.saturating_sub(start) < 2 || end > self.lines.len() {
            return false;
        }
        self.checkpoint();
        self.lines[start..end].sort_by(|left, right| {
            if reverse {
                right.cmp(left)
            } else {
                left.cmp(right)
            }
        });
        true
    }

    pub fn transform_lines_case(&mut self, start: usize, end: usize, uppercase: bool) -> bool {
        if self.readonly || start >= end || end > self.lines.len() {
            return false;
        }
        self.checkpoint();
        for line in &mut self.lines[start..end] {
            *line = if uppercase {
                line.chars().flat_map(char::to_uppercase).collect()
            } else {
                line.chars().flat_map(char::to_lowercase).collect()
            };
        }
        self.cursor.column = self.cursor.column.min(
            self.lines
                .get(self.cursor.line)
                .map_or(0, |line| line.chars().count()),
        );
        true
    }

    pub fn delete_line(&mut self) {
        if self.readonly {
            return;
        }
        if self.lines.len() == 1 {
            self.checkpoint();
            self.lines[0].clear();
            self.cursor.column = 0;
            return;
        }
        self.checkpoint();
        self.lines.remove(self.cursor.line);
        self.cursor.line = self.cursor.line.min(self.lines.len() - 1);
        self.cursor.column = self
            .cursor
            .column
            .min(self.lines[self.cursor.line].chars().count());
    }

    pub(crate) fn replace_line_range(&mut self, start: usize, end: usize, replacement: &[String]) {
        if self.readonly {
            return;
        }
        let start = start.min(self.lines.len());
        let end = end.clamp(start, self.lines.len());
        self.checkpoint();
        self.lines.splice(start..end, replacement.iter().cloned());
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor.line = start.min(self.lines.len().saturating_sub(1));
        self.cursor.column = 0;
    }

    pub fn toggle_comment(&mut self, marker: &str) {
        if self.readonly {
            return;
        }
        self.checkpoint();
        let line = &mut self.lines[self.cursor.line];
        let indentation = line.len() - line.trim_start().len();
        let rest = line[indentation..]
            .strip_prefix(marker)
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest).to_owned());
        if let Some(rest) = rest {
            line.replace_range(indentation.., &rest);
            self.cursor.column = self
                .cursor
                .column
                .saturating_sub(marker.chars().count() + 1);
        } else {
            line.insert_str(indentation, &format!("{marker} "));
            if self.cursor.column >= indentation {
                self.cursor.column += marker.chars().count() + 1;
            }
        }
    }

    pub fn indent_lines(&mut self, start: usize, end: usize, prefix: &str) {
        if self.readonly || prefix.is_empty() {
            return;
        }
        let start = start.min(self.lines.len().saturating_sub(1));
        let end = end.min(self.lines.len().saturating_sub(1)).max(start);
        self.checkpoint();
        for line in &mut self.lines[start..=end] {
            line.insert_str(0, prefix);
        }
        if (start..=end).contains(&self.cursor.line) {
            self.cursor.column += prefix.chars().count();
        }
    }

    pub fn outdent_lines(&mut self, start: usize, end: usize, width: usize) {
        if self.readonly || width == 0 {
            return;
        }
        let start = start.min(self.lines.len().saturating_sub(1));
        let end = end.min(self.lines.len().saturating_sub(1)).max(start);
        let removals: Vec<usize> = self.lines[start..=end]
            .iter()
            .map(|line| {
                if line.starts_with('\t') {
                    1
                } else {
                    line.chars()
                        .take(width)
                        .take_while(|character| *character == ' ')
                        .count()
                }
            })
            .collect();
        if removals.iter().all(|removed| *removed == 0) {
            return;
        }
        self.checkpoint();
        for (line, removed) in self.lines[start..=end].iter_mut().zip(removals.iter()) {
            if line.starts_with('\t') {
                line.remove(0);
            } else {
                *line = line.chars().skip(*removed).collect();
            }
        }
        if (start..=end).contains(&self.cursor.line) {
            let removed = removals[self.cursor.line - start];
            self.cursor.column = self.cursor.column.saturating_sub(removed);
        }
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            state_id: self.state_id,
        }
    }
    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor = snapshot.cursor;
        self.state_id = snapshot.state_id;
    }
    fn checkpoint(&mut self) {
        if self.undo_group_depth > 0 && self.undo_group_checkpointed {
            if self.saved_state_id == Some(self.state_id) {
                self.saved_state_id = None;
            }
            self.state_id = self.next_state_id;
            self.next_state_id = self.next_state_id.saturating_add(1);
            return;
        }
        if self.undo_group_depth > 0 {
            self.undo_group_checkpointed = true;
        }
        if self
            .saved_state_id
            .is_some_and(|saved| self.redo.iter().any(|snapshot| snapshot.state_id == saved))
        {
            self.saved_state_id = None;
        }
        self.undo.push(self.snapshot());
        self.redo.clear();
        self.state_id = self.next_state_id;
        self.next_state_id = self.next_state_id.saturating_add(1);
    }
    fn char_to_byte(&self) -> usize {
        self.lines[self.cursor.line]
            .char_indices()
            .nth(self.cursor.column)
            .map_or(self.lines[self.cursor.line].len(), |(i, _)| i)
    }
}

fn split_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn replace_line_case_insensitive(line: &str, needle: &str, replacement: &str) -> (String, usize) {
    let mut output = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut count = 0;
    while cursor < line.len() {
        let matched = line
            .get(cursor..cursor.saturating_add(needle.len()))
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle));
        if matched {
            output.push_str(replacement);
            cursor += needle.len();
            count += 1;
        } else if let Some(character) = line[cursor..].chars().next() {
            output.push(character);
            cursor += character.len_utf8();
        } else {
            break;
        }
    }
    (output, count)
}

fn ordered_positions(first: Position, second: Position) -> (Position, Position) {
    if (first.line, first.column) <= (second.line, second.column) {
        (first, second)
    } else {
        (second, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_edit_uses_character_columns() {
        let mut b = Buffer::new(vec!["你好".into()], None);
        b.move_end();
        b.insert("界");
        assert_eq!(b.cursor().column, 3);
        b.backspace();
        assert_eq!(b.lines(), &["你好"]);
    }
    #[test]
    fn undo_restores_dirty_baseline() {
        let mut b = Buffer::new(vec!["a".into()], None);
        b.insert("b");
        assert!(b.dirty());
        b.undo();
        assert!(!b.dirty());
    }

    #[test]
    fn branching_after_save_does_not_recreate_the_old_save_point() {
        let path =
            std::env::temp_dir().join(format!("mtide-undo-save-point-{}", std::process::id()));
        std::fs::write(&path, "hello").expect("initial file");
        let mut buffer = Buffer::open(&path).expect("open file");
        buffer.insert("A");
        buffer.save().expect("save A");
        buffer.undo();
        buffer.insert("B");
        buffer.undo();
        assert_eq!(buffer.lines(), &["hello"]);
        assert!(buffer.dirty());
        buffer.redo();
        assert_eq!(buffer.lines(), &["Bhello"]);
        assert!(buffer.dirty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn save_inside_undo_group_is_invalidated_by_a_later_group_edit() {
        let path = std::env::temp_dir().join(format!(
            "mtide-undo-group-save-point-{}",
            std::process::id()
        ));
        std::fs::write(&path, "hello").expect("initial file");
        let mut buffer = Buffer::open(&path).expect("open file");
        buffer.begin_undo_group();
        buffer.insert("A");
        buffer.save().expect("save inside group");
        buffer.insert("B");
        buffer.end_undo_group();
        assert!(buffer.dirty());
        buffer.undo();
        assert_eq!(buffer.lines(), &["hello"]);
        assert!(buffer.dirty());
        buffer.redo();
        assert_eq!(buffer.lines(), &["ABhello"]);
        assert!(buffer.dirty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detect_indent_uses_smallest_size_on_frequency_tie() {
        let lines = vec![
            "root".into(),
            "  two".into(),
            "    four".into(),
            "      six".into(),
            "    four".into(),
            "  two".into(),
        ];
        let info = detect_indent(&lines);
        assert_eq!(info.size, Some(2));
        assert!(!info.use_tabs);
    }

    #[test]
    fn detect_indent_identifies_tabs_and_limits_sample() {
        let mut lines = vec!["root".to_owned()];
        lines.extend((0..120).map(|_| "\tchild".to_owned()));
        lines.extend((0..130).map(|_| "  spaces".to_owned()));
        let info = detect_indent(&lines);
        assert!(info.has_tabs);
        assert!(info.use_tabs);
    }

    #[test]
    fn undo_group_collapses_multiple_edits_and_supports_nesting() {
        let mut b = Buffer::new(vec!["".into()], None);
        b.begin_undo_group();
        b.insert("a");
        b.begin_undo_group();
        b.insert("b");
        b.end_undo_group();
        b.insert("c");
        b.end_undo_group();
        assert_eq!(b.lines(), &["abc"]);
        b.undo();
        assert_eq!(b.lines(), &[""]);
        b.redo();
        assert_eq!(b.lines(), &["abc"]);
    }
    #[test]
    fn split_and_join_lines() {
        let mut b = Buffer::new(vec!["ab".into()], None);
        b.move_end();
        b.move_cursor(0, -1);
        b.insert("\n");
        assert_eq!(b.lines(), &["a", "b"]);
        b.backspace();
        assert_eq!(b.lines(), &["ab"]);
    }

    #[test]
    fn moving_and_joining_lines_each_use_one_undo_checkpoint() {
        let mut moved = Buffer::new(vec!["a".into(), "b".into(), "c".into(), "d".into()], None);
        moved.set_cursor(1, 0);
        assert!(moved.move_lines(1, 3, true));
        assert_eq!(moved.lines(), &["b", "c", "a", "d"]);
        moved.undo();
        assert_eq!(moved.lines(), &["a", "b", "c", "d"]);

        let mut joined = Buffer::new(vec!["one ".into(), " two".into(), "three".into()], None);
        assert!(joined.join_lines(0, 3));
        assert_eq!(joined.lines(), &["one two three"]);
        joined.undo();
        assert_eq!(joined.lines(), &["one ", " two", "three"]);
    }

    #[test]
    fn sorting_and_case_transforms_are_single_undo_batches() {
        let mut sorted = Buffer::new(vec!["b".into(), "a".into(), "c".into()], None);
        assert!(sorted.sort_lines(0, 3, false));
        assert_eq!(sorted.lines(), &["a", "b", "c"]);
        sorted.undo();
        assert_eq!(sorted.lines(), &["b", "a", "c"]);

        let mut transformed = Buffer::new(vec!["Hello".into(), "世界".into()], None);
        assert!(transformed.transform_lines_case(0, 2, true));
        assert_eq!(transformed.lines(), &["HELLO", "世界"]);
        transformed.undo();
        assert_eq!(transformed.lines(), &["Hello", "世界"]);
    }

    #[test]
    fn replace_all_is_one_undo_batch() {
        let mut b = Buffer::new(vec!["a a".into(), "ba".into()], None);
        assert_eq!(b.replace_all("a", "x"), 3);
        assert_eq!(b.lines(), &["x x", "bx"]);
        b.undo();
        assert_eq!(b.lines(), &["a a", "ba"]);
    }

    #[test]
    fn replace_ranges_are_one_undo_batch() {
        let mut b = Buffer::new(vec!["one two one".into()], None);
        let ranges = [
            (
                Position { line: 0, column: 0 },
                Position { line: 0, column: 3 },
            ),
            (
                Position { line: 0, column: 8 },
                Position {
                    line: 0,
                    column: 11,
                },
            ),
        ];
        assert_eq!(b.replace_ranges(&ranges, "x"), 2);
        assert_eq!(b.lines(), &["x two x"]);
        b.undo();
        assert_eq!(b.lines(), &["one two one"]);
    }

    #[test]
    fn replace_ranges_merge_overlaps_before_editing() {
        let mut b = Buffer::new(vec!["abcdef".into()], None);
        let ranges = [
            (
                Position { line: 0, column: 1 },
                Position { line: 0, column: 4 },
            ),
            (
                Position { line: 0, column: 3 },
                Position { line: 0, column: 5 },
            ),
        ];
        assert_eq!(b.replace_ranges(&ranges, "X"), 1);
        assert_eq!(b.lines(), &["aXf"]);
    }

    #[test]
    fn line_commands_preserve_undo_and_toggle_comment() {
        let mut b = Buffer::new(vec!["  value".into()], None);
        b.toggle_comment("//");
        assert_eq!(b.lines(), &["  // value"]);
        b.toggle_comment("//");
        assert_eq!(b.lines(), &["  value"]);
        b.duplicate_line();
        assert_eq!(b.lines(), &["  value", "  value"]);
        b.delete_line();
        assert_eq!(b.lines(), &["  value"]);
    }

    #[test]
    fn indent_and_outdent_lines_are_single_undoable_operations() {
        let mut b = Buffer::new(vec!["one".into(), "two".into()], None);
        b.set_cursor(1, 1);
        b.indent_lines(0, 1, "  ");
        assert_eq!(b.lines(), &["  one", "  two"]);
        assert_eq!(b.cursor(), Position { line: 1, column: 3 });
        b.outdent_lines(0, 1, 2);
        assert_eq!(b.lines(), &["one", "two"]);
        assert_eq!(b.cursor(), Position { line: 1, column: 1 });
        b.undo();
        assert_eq!(b.lines(), &["  one", "  two"]);
        b.undo();
        assert_eq!(b.lines(), &["one", "two"]);
    }

    #[test]
    fn word_deletion_is_unicode_safe_and_single_undoable_operation() {
        let mut backward = Buffer::new(vec!["你好  world".into(), "next line".into()], None);
        backward.set_cursor(0, 9);
        backward.delete_word_backward();
        assert_eq!(
            backward.lines(),
            &["你好  ".to_owned(), "next line".to_owned()]
        );
        assert_eq!(backward.cursor(), Position { line: 0, column: 4 });
        backward.undo();
        assert_eq!(
            backward.lines(),
            &["你好  world".to_owned(), "next line".to_owned()]
        );

        backward.set_cursor(1, 0);
        backward.delete_word_backward();
        assert_eq!(backward.lines(), &["你好  next line".to_owned()]);

        let mut forward = Buffer::new(vec!["你好  world".into(), "next".into()], None);
        forward.set_cursor(0, 3);
        forward.delete_word_forward();
        assert_eq!(forward.lines(), &["你好 ".to_owned(), "next".to_owned()]);
        forward.set_cursor(0, 3);
        forward.delete_word_forward();
        assert_eq!(forward.lines(), &["你好 next".to_owned()]);
    }

    #[test]
    fn word_navigation_uses_rune_columns_and_crosses_lines() {
        let mut buffer = Buffer::new(vec!["你好  world".into(), "next".into()], None);
        buffer.set_cursor(0, 9);
        buffer.move_word_left();
        assert_eq!(buffer.cursor(), Position { line: 0, column: 4 });
        buffer.move_word_left();
        assert_eq!(buffer.cursor(), Position { line: 0, column: 0 });
        buffer.move_word_right();
        assert_eq!(buffer.cursor(), Position { line: 0, column: 2 });
        buffer.move_word_right();
        assert_eq!(buffer.cursor(), Position { line: 0, column: 9 });
        buffer.move_word_right();
        assert_eq!(buffer.cursor(), Position { line: 1, column: 0 });
    }

    #[test]
    fn file_navigation_jumps_to_document_boundaries() {
        let mut buffer = Buffer::new(vec!["first".into(), "中间".into(), "last".into()], None);
        buffer.set_cursor(1, 1);
        buffer.move_file_end();
        assert_eq!(buffer.cursor(), Position { line: 2, column: 4 });
        buffer.move_file_start();
        assert_eq!(buffer.cursor(), Position { line: 0, column: 0 });
    }

    #[test]
    fn selection_copy_and_delete_are_unicode_safe() {
        let mut b = Buffer::new(vec!["你好 world".into(), "next".into()], None);
        let start = Position { line: 0, column: 2 };
        let end = Position { line: 1, column: 2 };
        assert_eq!(b.text_between(start, end), " world\nne");
        b.delete_between(start, end);
        assert_eq!(b.lines(), &["你好xt"]);
        b.undo();
        assert_eq!(b.lines(), &["你好 world", "next"]);
    }

    #[test]
    fn save_options_trim_lines_and_add_final_newline() {
        let path = std::env::temp_dir().join(format!("mtide-save-{}", std::process::id()));
        fs::write(&path, "value  ").expect("seed file");
        let mut buffer = Buffer::open(&path).expect("open file");
        buffer.save_with_options(true, true).expect("save options");
        assert_eq!(fs::read_to_string(&path).expect("read file"), "value\n");
        assert!(!buffer.dirty());
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn clean_buffer_reloads_external_changes_and_resets_undo() {
        let path =
            std::env::temp_dir().join(format!("mtide-external-clean-{}", std::process::id()));
        fs::write(&path, "before").expect("seed file");
        let mut buffer = Buffer::open(&path).expect("open file");
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "after\nsecond").expect("external write");
        assert_eq!(
            buffer.poll_external_change().expect("poll"),
            Some(ExternalChange::Reloaded)
        );
        assert_eq!(buffer.lines(), &["after", "second"]);
        buffer.undo();
        assert_eq!(buffer.lines(), &["after", "second"]);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn dirty_buffer_does_not_overwrite_local_edits_on_external_change() {
        let path =
            std::env::temp_dir().join(format!("mtide-external-dirty-{}", std::process::id()));
        fs::write(&path, "before").expect("seed file");
        let mut buffer = Buffer::open(&path).expect("open file");
        buffer.insert(" local");
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "external").expect("external write");
        assert_eq!(
            buffer.poll_external_change().expect("poll"),
            Some(ExternalChange::Dirty)
        );
        assert_eq!(buffer.lines(), &[" localbefore"]);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn clean_buffer_reports_external_file_deletion() {
        let path =
            std::env::temp_dir().join(format!("mtide-external-deleted-{}", std::process::id()));
        fs::write(&path, "before").expect("seed file");
        let mut buffer = Buffer::open(&path).expect("open file");
        fs::remove_file(&path).expect("delete file");
        assert_eq!(
            buffer.poll_external_change().expect("poll"),
            Some(ExternalChange::Deleted)
        );
        assert_eq!(buffer.lines(), &["before"]);
        assert_eq!(
            buffer.poll_external_change().expect("deduplicated poll"),
            None
        );
        fs::write(&path, "restored").expect("restore file");
        assert_eq!(
            buffer.poll_external_change().expect("restored poll"),
            Some(ExternalChange::Reloaded)
        );
        assert_eq!(buffer.lines(), &["restored"]);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn inserts_at_multiple_positions_in_reverse_order_as_one_undo() {
        let mut b = Buffer::new(vec!["abcd".into(), "efgh".into()], None);
        let positions = [
            Position { line: 0, column: 1 },
            Position { line: 1, column: 2 },
        ];
        let result = b.insert_at_positions(&positions, "X");
        assert_eq!(b.lines(), &["aXbcd", "efXgh"]);
        assert_eq!(result[0].column, 2);
        assert_eq!(result[1].column, 3);
        b.undo();
        assert_eq!(b.lines(), &["abcd", "efgh"]);
    }

    #[test]
    fn multi_position_newline_preserves_cursor_locations() {
        let mut b = Buffer::new(vec!["abcd".into(), "efgh".into()], None);
        let positions = [
            Position { line: 0, column: 2 },
            Position { line: 1, column: 2 },
        ];
        let result = b.insert_at_positions(&positions, "\n");
        assert_eq!(b.lines(), &["ab", "cd", "ef", "gh"]);
        assert_eq!(
            result,
            vec![
                Position { line: 1, column: 0 },
                Position { line: 3, column: 0 }
            ]
        );
    }

    #[test]
    fn multi_position_backspace_is_one_undo_batch() {
        let mut b = Buffer::new(vec!["ab".into(), "cd".into()], None);
        let positions = [
            Position { line: 0, column: 1 },
            Position { line: 1, column: 1 },
        ];
        let result = b.backspace_at_positions(&positions);
        assert_eq!(b.lines(), &["b", "d"]);
        assert_eq!(
            result,
            vec![
                Position { line: 0, column: 0 },
                Position { line: 1, column: 0 }
            ]
        );
        b.undo();
        assert_eq!(b.lines(), &["ab", "cd"]);
    }

    #[test]
    fn readonly_buffer_rejects_edits_and_saves() {
        let path = std::env::temp_dir().join(format!("mtide-readonly-{}", std::process::id()));
        fs::write(&path, "before").expect("seed file");
        let mut buffer = Buffer::open(&path).expect("open file");
        buffer.set_readonly(true);
        buffer.insert(" after");
        buffer.delete();
        assert_eq!(
            buffer.apply_text_edits(&[(
                Position { line: 0, column: 0 },
                Position { line: 0, column: 0 },
                "prefix".into(),
            )]),
            0
        );
        let readonly_positions = buffer.backspace_at_positions(&[Position { line: 0, column: 3 }]);
        assert_eq!(readonly_positions, vec![Position { line: 0, column: 3 }]);
        assert_eq!(buffer.lines(), &["before"]);
        assert!(buffer.save().is_err());
        assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn deleting_the_only_line_is_undoable() {
        let mut buffer = Buffer::new(vec!["content".into()], None);
        buffer.delete_line();
        assert_eq!(buffer.lines(), &[""]);
        buffer.undo();
        assert_eq!(buffer.lines(), &["content"]);
    }

    #[test]
    fn detects_unresolved_merge_conflict_markers() {
        let clean = Buffer::from_text("<<<<<<< not a marker\nvalue");
        assert!(clean.has_unresolved_conflicts());
        let resolved = Buffer::from_text("value\n======= is ordinary text");
        assert!(resolved.has_unresolved_conflicts());
        let normal = Buffer::from_text("value\nnext");
        assert!(!normal.has_unresolved_conflicts());
    }
}
