//! Synchronous document and edit primitives. This crate intentionally knows
//! nothing about terminals, files, or asynchronous runtimes.

use regex::Regex;
use ropey::Rope;
use std::ops::Range;
use std::path::PathBuf;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DocId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub insert: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub active: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationLocation {
    pub path: Option<PathBuf>,
    pub byte: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NavigationStack {
    back: Vec<NavigationLocation>,
    forward: Vec<NavigationLocation>,
}

impl NavigationStack {
    pub fn push(&mut self, location: NavigationLocation) {
        if self.back.last() == Some(&location) {
            return;
        }
        self.back.push(location);
        self.forward.clear();
    }

    pub fn back(&mut self, current: NavigationLocation) -> Option<NavigationLocation> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    pub fn forward(&mut self, current: NavigationLocation) -> Option<NavigationLocation> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

impl Selection {
    pub fn new(anchor: usize, active: usize) -> Self {
        Self { anchor, active }
    }
    pub fn range(self) -> Range<usize> {
        self.anchor.min(self.active)..self.anchor.max(self.active)
    }
    pub fn is_empty(self) -> bool {
        self.anchor == self.active
    }
    pub fn reversed(self) -> bool {
        self.active < self.anchor
    }
}

/// Splits clipboard lines across multiple cursors when the cardinalities
/// match; otherwise every cursor receives the complete clipboard payload.
pub fn paste_for_cursors(clipboard: &str, cursor_count: usize) -> Vec<String> {
    if cursor_count == 0 {
        return Vec::new();
    }
    let lines = clipboard.lines().map(str::to_owned).collect::<Vec<_>>();
    if lines.len() == cursor_count {
        lines
    } else {
        vec![clipboard.to_owned(); cursor_count]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetExpansion {
    pub text: String,
    pub tabstops: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamingStyle {
    Lower,
    Upper,
    Camel,
    Pascal,
    Snake,
    Kebab,
    Constant,
}

pub fn convert_identifier(input: &str, style: NamingStyle) -> String {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    let mut previous_digit = false;
    for character in input.chars() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            previous_digit = false;
        } else {
            if character.is_uppercase() && (previous_lower || previous_digit) && !current.is_empty()
            {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = character.is_lowercase();
            previous_digit = character.is_ascii_digit();
            current.push(character);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty()
        || words
            .iter()
            .all(|word| word.chars().all(|c| c.is_ascii_digit()))
    {
        return input.to_owned();
    }
    let lower = words
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>();
    match style {
        NamingStyle::Lower => lower.join(""),
        NamingStyle::Upper => lower.join("").to_uppercase(),
        NamingStyle::Snake => lower.join("_"),
        NamingStyle::Kebab => lower.join("-"),
        NamingStyle::Constant => lower.join("_").to_uppercase(),
        NamingStyle::Camel => lower
            .iter()
            .enumerate()
            .map(|(index, word)| {
                if index == 0 {
                    word.clone()
                } else {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                }
            })
            .collect(),
        NamingStyle::Pascal => lower
            .iter()
            .map(|word| {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect(),
    }
}

/// Expands VSCode-compatible numbered placeholders and simple variables.
/// Unknown variables are retained so callers can provide a later resolver.
pub fn expand_snippet(body: &str, filename: Option<&str>) -> SnippetExpansion {
    let mut text = String::new();
    let mut tabstops = Vec::new();
    let mut chars = body.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        if character != '$' {
            text.push(character);
            continue;
        }
        let Some(&(_, next)) = chars.peek() else {
            text.push('$');
            continue;
        };
        if next.is_ascii_digit() {
            let mut number = String::new();
            while let Some(&(_, digit)) = chars.peek() {
                if !digit.is_ascii_digit() {
                    break;
                }
                number.push(digit);
                chars.next();
            }
            if let Ok(index) = number.parse::<usize>() {
                tabstops.push((index, text.len(), text.len()));
                continue;
            }
        } else if next.is_ascii_alphabetic() || next == '_' {
            let mut name = String::new();
            while let Some(&(_, current)) = chars.peek() {
                if !(current.is_ascii_alphanumeric() || current == '_') {
                    break;
                }
                name.push(current);
                chars.next();
            }
            if name == "TM_FILENAME" {
                text.push_str(filename.unwrap_or_default());
                continue;
            }
            text.push('$');
            text.push_str(&name);
            continue;
        } else if next == '{' {
            chars.next();
            let mut content = String::new();
            let mut closed = false;
            for (_, current) in chars.by_ref() {
                if current == '}' {
                    closed = true;
                    break;
                }
                content.push(current);
            }
            if closed {
                if let Some((index, default)) = content.split_once(':') {
                    if let Ok(index) = index.parse::<usize>() {
                        let begin = text.len();
                        text.push_str(default);
                        tabstops.push((index, begin, text.len()));
                        continue;
                    }
                } else if content == "TM_FILENAME" {
                    text.push_str(filename.unwrap_or_default());
                    continue;
                } else if let Ok(index) = content.parse::<usize>() {
                    tabstops.push((index, text.len(), text.len()));
                    continue;
                }
            }
            text.push('$');
            text.push('{');
            text.push_str(&content);
            if !closed {
                break;
            }
            text.push('}');
            continue;
        }
        let _ = start;
        text.push('$');
    }
    tabstops.sort_by_key(|(index, _, _)| if *index == 0 { usize::MAX } else { *index });
    SnippetExpansion {
        text,
        tabstops: tabstops
            .into_iter()
            .map(|(_, start, end)| (start, end))
            .collect(),
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum EditError {
    #[error("edit range {start}..{end} is outside a document of {len} bytes")]
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("edit range {start}..{end} is not on UTF-8 character boundaries")]
    InvalidBoundary { start: usize, end: usize },
    #[error("edit ranges overlap")]
    Overlap,
    #[error("invalid regular expression: {0}")]
    Regex(String),
}

#[derive(Debug)]
pub struct Document {
    id: DocId,
    rope: Rope,
    dirty: bool,
    version: i32,
    undo: Vec<Rope>,
    redo: Vec<Rope>,
    redo_branches: Vec<Vec<Rope>>,
}

impl Document {
    pub fn new(id: DocId, text: &str) -> Self {
        Self {
            id,
            rope: Rope::from_str(text),
            dirty: false,
            version: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            redo_branches: Vec::new(),
        }
    }

    pub fn id(&self) -> DocId {
        self.id
    }
    pub fn text(&self) -> String {
        self.rope.to_string()
    }
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }
    pub fn line_to_byte(&self, line: usize) -> Option<usize> {
        (line < self.rope.len_lines()).then(|| self.rope.line_to_byte(line))
    }
    pub fn byte_to_line(&self, byte: usize) -> Option<usize> {
        (byte <= self.rope.len_bytes()).then(|| self.rope.byte_to_line(byte))
    }
    pub fn line_column_to_byte(&self, line: usize, column: usize) -> Option<usize> {
        let start = self.line_to_byte(line)?;
        let line_text = self.rope.line(line).to_string();
        let content_end = line_text
            .len()
            .saturating_sub(usize::from(line_text.ends_with('\n')));
        let byte_column = line_text[..content_end]
            .grapheme_indices(true)
            .nth(column)
            .map(|(byte, _)| byte)
            .unwrap_or(content_end);
        Some(start + byte_column)
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    /// Marks content restored from a session snapshot as locally modified.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn find_literal(&self, query: &str) -> Vec<Range<usize>> {
        if query.is_empty() {
            return Vec::new();
        }
        let text = self.text();
        text.match_indices(query)
            .map(|(start, _)| start..start + query.len())
            .collect()
    }

    /// Replaces every literal match as one undoable transaction.
    pub fn replace_all_literal(
        &mut self,
        query: &str,
        replacement: &str,
    ) -> Result<usize, EditError> {
        let matches = self.find_literal(query);
        let count = matches.len();
        let edits = matches
            .into_iter()
            .map(|range| Edit {
                range,
                insert: replacement.to_owned(),
            })
            .collect::<Vec<_>>();
        if !edits.is_empty() {
            self.apply(&edits)?;
        }
        Ok(count)
    }

    pub fn find_regex(&self, pattern: &str) -> Result<Vec<Range<usize>>, regex::Error> {
        let regex = Regex::new(pattern)?;
        Ok(regex
            .find_iter(&self.text())
            .map(|matched| matched.start()..matched.end())
            .collect())
    }

    pub fn replace_all_regex(
        &mut self,
        pattern: &str,
        replacement: &str,
    ) -> Result<usize, EditError> {
        let regex = Regex::new(pattern).map_err(|error| EditError::Regex(error.to_string()))?;
        let text = self.text();
        let matches = regex
            .captures_iter(&text)
            .filter_map(|captures| {
                let matched = captures
                    .get(0)
                    .map(|matched| (matched.start(), matched.end()));
                matched.map(|(start, end)| {
                    let mut insert = String::new();
                    captures.expand(replacement, &mut insert);
                    Edit {
                        range: start..end,
                        insert,
                    }
                })
            })
            .collect::<Vec<_>>();
        let count = matches.len();
        if !matches.is_empty() {
            self.apply(&matches)?;
        }
        Ok(count)
    }

    pub fn insert_at_cursors(&mut self, cursors: &[usize], text: &str) -> Result<usize, EditError> {
        let mut positions = cursors.to_vec();
        positions.sort_unstable();
        positions.dedup();
        positions.truncate(1000);
        let edits = positions
            .iter()
            .map(|&position| Edit {
                range: position..position,
                insert: text.to_owned(),
            })
            .collect::<Vec<_>>();
        let count = edits.len();
        if count > 0 {
            self.apply(&edits)?;
        }
        Ok(count)
    }

    pub fn replace_selection(
        &mut self,
        selection: Selection,
        text: &str,
    ) -> Result<usize, EditError> {
        let range = selection.range();
        self.apply(&[Edit {
            range,
            insert: text.to_owned(),
        }])?;
        Ok(text.len())
    }

    pub fn selected_text(&self, selection: Selection) -> Result<String, EditError> {
        let range = selection.range();
        let text = self.text();
        if range.end > text.len()
            || !is_grapheme_boundary(&text, range.start)
            || !is_grapheme_boundary(&text, range.end)
        {
            return Err(EditError::InvalidBoundary {
                start: range.start,
                end: range.end,
            });
        }
        Ok(text[range].to_owned())
    }

    pub fn delete_line(&mut self, line: usize) -> Result<bool, EditError> {
        let Some(start) = self.line_to_byte(line) else {
            return Ok(false);
        };
        let end = self
            .line_to_byte(line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        self.apply(&[Edit {
            range: start..end,
            insert: String::new(),
        }])?;
        Ok(true)
    }

    pub fn toggle_line_comment(&mut self, line: usize, marker: &str) -> Result<bool, EditError> {
        let Some(start) = self.line_to_byte(line) else {
            return Ok(false);
        };
        let end = self
            .line_to_byte(line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let raw = self.rope.byte_slice(start..end).to_string();
        let leading = raw.len() - raw.trim_start_matches(char::is_whitespace).len();
        let content_start = start + leading;
        let trimmed = &raw[leading..];
        if let Some(stripped) = trimmed.strip_prefix(marker) {
            let remove_end = content_start + marker.len() + usize::from(stripped.starts_with(' '));
            self.apply(&[Edit {
                range: content_start..remove_end,
                insert: String::new(),
            }])?;
        } else {
            self.apply(&[Edit {
                range: content_start..content_start,
                insert: format!("{marker} "),
            }])?;
        }
        Ok(true)
    }

    pub fn toggle_line_comments(
        &mut self,
        start_line: usize,
        end_line: usize,
        marker: &str,
    ) -> Result<bool, EditError> {
        if start_line > end_line || end_line >= self.rope.len_lines() {
            return Ok(false);
        }
        let start = self.line_to_byte(start_line).unwrap_or(0);
        let end = self
            .line_to_byte(end_line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let original = self.rope.byte_slice(start..end).to_string();
        let lines = original.lines().collect::<Vec<_>>();
        let nonempty = lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let commented = nonempty.iter().all(|line| {
            let indent = line.len() - line.trim_start().len();
            line[indent..].starts_with(marker)
        });
        let mut transformed = lines
            .iter()
            .map(|line| {
                if line.trim().is_empty() {
                    return (*line).to_owned();
                }
                let indent = line.len() - line.trim_start().len();
                if commented {
                    let tail = &line[indent..];
                    let remove = marker.len()
                        + usize::from(
                            tail.get(marker.len()..)
                                .is_some_and(|rest| rest.starts_with(' ')),
                        );
                    format!("{}{}", &line[..indent], &tail[remove.min(tail.len())..])
                } else {
                    format!("{}{} {}", &line[..indent], marker, &line[indent..])
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if original.ends_with('\n') {
            transformed.push('\n');
        }
        self.apply(&[Edit {
            range: start..end,
            insert: transformed,
        }])?;
        Ok(true)
    }

    pub fn duplicate_line(&mut self, line: usize) -> Result<bool, EditError> {
        let Some(start) = self.line_to_byte(line) else {
            return Ok(false);
        };
        let end = self
            .line_to_byte(line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let content = self.rope.byte_slice(start..end).to_string();
        let insert = if content.ends_with('\n') {
            content
        } else {
            format!("{content}\n")
        };
        self.apply(&[Edit {
            range: end..end,
            insert,
        }])?;
        Ok(true)
    }

    pub fn move_line(&mut self, line: usize, direction: isize) -> Result<bool, EditError> {
        let target = line as isize + direction;
        if target < 0 || target as usize >= self.rope.len_lines() {
            return Ok(false);
        }
        let target = target as usize;
        let first = self.line_to_byte(line).unwrap_or(0);
        let after = self
            .line_to_byte(line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let other_start = self.line_to_byte(target).unwrap_or(0);
        let other_end = self
            .line_to_byte(target + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let (range, replacement) = if direction < 0 {
            (
                other_start..after,
                format!(
                    "{}{}",
                    self.rope.byte_slice(first..after),
                    self.rope.byte_slice(other_start..first)
                ),
            )
        } else {
            (
                first..other_end,
                format!(
                    "{}{}",
                    self.rope.byte_slice(other_start..other_end),
                    self.rope.byte_slice(first..other_start)
                ),
            )
        };
        self.apply(&[Edit {
            range,
            insert: replacement,
        }])?;
        Ok(true)
    }

    pub fn sort_lines(
        &mut self,
        start_line: usize,
        end_line: usize,
        descending: bool,
        unique: bool,
    ) -> Result<bool, EditError> {
        if start_line > end_line || end_line >= self.rope.len_lines() {
            return Ok(false);
        }
        let start = self.line_to_byte(start_line).unwrap_or(0);
        let end = self
            .line_to_byte(end_line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let original = self.rope.byte_slice(start..end).to_string();
        let had_newline = original.ends_with('\n');
        let mut lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
        lines.sort();
        if descending {
            lines.reverse();
        }
        if unique {
            lines.dedup();
        }
        let mut replacement = lines.join("\n");
        if had_newline {
            replacement.push('\n');
        }
        if replacement == original {
            return Ok(false);
        }
        self.apply(&[Edit {
            range: start..end,
            insert: replacement,
        }])?;
        Ok(true)
    }

    pub fn join_lines(
        &mut self,
        start_line: usize,
        end_line: usize,
        separator: &str,
    ) -> Result<bool, EditError> {
        if start_line >= end_line || end_line >= self.rope.len_lines() {
            return Ok(false);
        }
        let start = self.line_to_byte(start_line).unwrap_or(0);
        let end = self
            .line_to_byte(end_line + 1)
            .unwrap_or_else(|| self.rope.len_bytes());
        let original = self.rope.byte_slice(start..end).to_string();
        let mut joined = original
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(separator);
        if original.ends_with('\n') {
            joined.push('\n');
        }
        self.apply(&[Edit {
            range: start..end,
            insert: joined,
        }])?;
        Ok(true)
    }

    /// Applies all edits as one transaction. The input may be in any order;
    /// edits are applied from the end so earlier byte offsets remain stable.
    pub fn apply(&mut self, edits: &[Edit]) -> Result<(), EditError> {
        let inverse = self.validate_and_inverse(edits)?;
        let previous = self.rope.clone();
        let mut ordered: Vec<&Edit> = edits.iter().collect();
        ordered.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in ordered {
            self.apply_edit(edit);
        }
        let _ = inverse;
        self.undo.push(previous);
        if !self.redo.is_empty() {
            self.redo_branches.push(std::mem::take(&mut self.redo));
        }
        self.dirty = true;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn undo(&mut self) -> Option<()> {
        let target = self.undo.pop()?;
        self.redo.push(self.rope.clone());
        self.restore_text(target);
        self.dirty = true;
        self.version = self.version.saturating_add(1);
        Some(())
    }

    pub fn redo(&mut self) -> Option<()> {
        let target = self.redo.pop()?;
        self.undo.push(self.rope.clone());
        self.restore_text(target);
        self.dirty = true;
        self.version = self.version.saturating_add(1);
        Some(())
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.undo.clear();
        self.redo.clear();
        self.redo_branches.clear();
    }

    pub fn redo_branch_count(&self) -> usize {
        self.redo_branches.len()
    }

    /// Switches to a preserved post-undo branch. Branches are exposed as a
    /// small explicit API so a command palette can present them later.
    pub fn switch_redo_branch(&mut self, index: usize) -> Option<()> {
        let (target, empty) = {
            let branch = self.redo_branches.get_mut(index)?;
            let target = branch.pop()?;
            (target, branch.is_empty())
        };
        self.undo.push(self.rope.clone());
        self.restore_text(target);
        if empty {
            self.redo_branches.remove(index);
        }
        self.dirty = true;
        self.version = self.version.saturating_add(1);
        Some(())
    }

    fn restore_text(&mut self, rope: Rope) {
        self.rope = rope;
    }

    fn apply_edit(&mut self, edit: &Edit) {
        let start = self.rope.byte_to_char(edit.range.start);
        let end = self.rope.byte_to_char(edit.range.end);
        self.rope.remove(start..end);
        self.rope.insert(start, &edit.insert);
    }

    fn validate_and_inverse(&self, edits: &[Edit]) -> Result<Vec<Edit>, EditError> {
        let len = self.rope.len_bytes();
        let mut ranges: Vec<Range<usize>> = edits.iter().map(|edit| edit.range.clone()).collect();
        ranges.sort_by_key(|range| range.start);
        for (index, range) in ranges.iter().enumerate() {
            if range.start > range.end || range.end > len {
                return Err(EditError::OutOfBounds {
                    start: range.start,
                    end: range.end,
                    len,
                });
            }
            if !is_rope_grapheme_boundary(&self.rope, range.start)
                || !is_rope_grapheme_boundary(&self.rope, range.end)
            {
                return Err(EditError::InvalidBoundary {
                    start: range.start,
                    end: range.end,
                });
            }
            if index > 0 && ranges[index - 1].end > range.start {
                return Err(EditError::Overlap);
            }
        }
        let mut inverse = Vec::with_capacity(edits.len());
        let mut delta: isize = 0;
        for range in &ranges {
            let new_start = (range.start as isize + delta) as usize;
            let Some(source) = edits.iter().find(|edit| edit.range == *range) else {
                return Err(EditError::Overlap);
            };
            inverse.push(Edit {
                range: new_start..new_start + source.insert.len(),
                insert: self.rope.byte_slice(range.clone()).to_string(),
            });
            delta += source.insert.len() as isize - (range.end - range.start) as isize;
        }
        inverse.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        Ok(inverse)
    }
}

fn is_grapheme_boundary(text: &str, byte: usize) -> bool {
    byte == 0 || byte == text.len() || text.grapheme_indices(true).any(|(start, _)| start == byte)
}

fn is_rope_grapheme_boundary(rope: &Rope, byte: usize) -> bool {
    if byte == 0 || byte == rope.len_bytes() {
        return true;
    }
    let Ok(char_index) = rope.try_byte_to_char(byte) else {
        return false;
    };
    let start = char_index.saturating_sub(16);
    let end = (char_index + 16).min(rope.len_chars());
    let sample = rope.slice(start..end).to_string();
    if sample.is_ascii() {
        return true;
    }
    // Non-ASCII grapheme clusters can span arbitrary numbers of combining
    // marks or joiners. Use the complete text only for this correctness path;
    // ASCII-heavy large files stay on the O(log n) rope path above.
    is_grapheme_boundary(&rope.to_string(), byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fr_ed22_snippet_expands_placeholders_and_filename() {
        let expansion = expand_snippet(
            "for ${1:item} in ${2:items} { $0 } // $TM_FILENAME",
            Some("main.rs"),
        );
        assert_eq!(expansion.text, "for item in items {  } // main.rs");
        assert_eq!(expansion.tabstops, vec![(4, 8), (12, 17), (20, 20)]);
    }

    #[test]
    fn fr_ed23_identifier_conversion_preserves_numeric_only_values() {
        assert_eq!(
            convert_identifier("user_name2Value", NamingStyle::Camel),
            "userName2Value"
        );
        assert_eq!(
            convert_identifier("userName2Value", NamingStyle::Snake),
            "user_name2_value"
        );
        assert_eq!(convert_identifier("12345", NamingStyle::Camel), "12345");
    }

    #[test]
    fn fr_ed01_utf8_line_and_cursor_mapping() {
        let doc = Document::new(DocId(1), "你好 🌍\n第二行\n");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.byte_to_line("你好 🌍\n".len()), Some(1));
        assert_eq!(doc.line_to_byte(1), Some("你好 🌍\n".len()));
        assert_eq!(doc.line_column_to_byte(0, 2), Some("你好".len()));
    }

    #[test]
    fn fr_ed01_grapheme_cursor_mapping_keeps_combining_and_zwj_clusters_together() {
        let document = Document::new(DocId(8), "A e\u{301} 👩‍💻 Z");
        assert_eq!(document.line_column_to_byte(0, 0), Some(0));
        assert_eq!(document.line_column_to_byte(0, 1), Some(1));
        assert_eq!(document.line_column_to_byte(0, 2), Some(2));
        assert_eq!(document.line_column_to_byte(0, 3), Some("A e\u{301}".len()));
        assert_eq!(
            document.line_column_to_byte(0, 4),
            Some("A e\u{301} ".len())
        );
        assert_eq!(
            document.line_column_to_byte(0, 5),
            Some("A e\u{301} 👩‍💻".len())
        );
    }

    #[test]
    fn fr_ed03_transaction_is_undoable_as_one_unit() {
        let mut doc = Document::new(DocId(1), "ab🙂");
        doc.apply(&[
            Edit {
                range: 0..0,
                insert: "X".into(),
            },
            Edit {
                range: 1..1,
                insert: "Y".into(),
            },
        ])
        .expect("valid transaction");
        assert_eq!(doc.text(), "XaYb🙂");
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "ab🙂");
        assert!(doc.redo().is_some());
        assert_eq!(doc.text(), "XaYb🙂");
    }

    #[test]
    fn fr_ed03_new_edit_preserves_a_switchable_undo_branch() {
        let mut doc = Document::new(DocId(1), "a");
        doc.apply(&[Edit {
            range: 1..1,
            insert: "b".into(),
        }])
        .expect("first edit");
        doc.undo();
        doc.apply(&[Edit {
            range: 1..1,
            insert: "c".into(),
        }])
        .expect("branch edit");
        assert_eq!(doc.text(), "ac");
        assert_eq!(doc.redo_branch_count(), 1);
        doc.undo();
        assert_eq!(doc.text(), "a");
        doc.switch_redo_branch(0).expect("switch branch");
        assert_eq!(doc.text(), "ab");
    }

    #[test]
    fn fr_ed01_rejects_mid_character_ranges() {
        let mut doc = Document::new(DocId(1), "中");
        assert_eq!(
            doc.apply(&[Edit {
                range: 1..1,
                insert: "x".into()
            }]),
            Err(EditError::InvalidBoundary { start: 1, end: 1 })
        );
    }

    #[test]
    fn fr_ed01_rejects_mid_grapheme_ranges() {
        let mut doc = Document::new(DocId(7), "e\u{301}👩‍💻");
        let result = doc.apply(&[Edit {
            range: 1..1,
            insert: "x".into(),
        }]);
        assert_eq!(result, Err(EditError::InvalidBoundary { start: 1, end: 1 }));
    }

    #[test]
    fn fr_ed05_find_and_replace_cjk_is_one_undoable_transaction() {
        let mut doc = Document::new(DocId(1), "foo 中文 foo");
        assert_eq!(doc.find_literal("foo").len(), 2);
        assert_eq!(doc.replace_all_literal("foo", "函数").expect("replace"), 2);
        assert_eq!(doc.text(), "函数 中文 函数");
        assert!(doc.find_literal("").is_empty());
        assert_eq!(doc.replace_all_literal("", "x").expect("empty replace"), 0);
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "foo 中文 foo");
    }

    #[test]
    fn fr_ed05_regex_find_and_capture_group_replace_is_atomic() {
        let mut doc = Document::new(DocId(10), "foo=一 bar=二");
        assert_eq!(doc.find_regex(r"(\w+)=(\S+)").expect("regex").len(), 2);
        assert_eq!(
            doc.replace_all_regex(r"(\w+)=(\S+)", "$2:$1")
                .expect("replace"),
            2
        );
        assert_eq!(doc.text(), "一:foo 二:bar");
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "foo=一 bar=二");
    }

    #[test]
    fn fr_ed06_navigation_stack_supports_cross_file_back_and_forward() {
        let a = NavigationLocation {
            path: Some("a.rs".into()),
            byte: 4,
        };
        let b = NavigationLocation {
            path: Some("b.rs".into()),
            byte: 8,
        };
        let mut stack = NavigationStack::default();
        stack.push(a.clone());
        assert_eq!(stack.back(b.clone()), Some(a.clone()));
        assert_eq!(stack.forward(a.clone()), Some(b));
    }

    #[test]
    fn fr_ed02_multicursor_is_deduplicated() {
        let mut doc = Document::new(DocId(1), "abc");
        assert_eq!(
            doc.insert_at_cursors(&[0, 0, 1, 2], "x")
                .expect("multi insert"),
            3
        );
        assert_eq!(doc.text(), "xaxbxc");
    }

    #[test]
    fn fr_ed04_clipboard_lines_are_distributed_only_when_counts_match() {
        assert_eq!(
            paste_for_cursors("one\ntwo\nthree", 3),
            vec!["one", "two", "three"]
        );
        assert_eq!(
            paste_for_cursors("one\ntwo", 3),
            vec!["one\ntwo", "one\ntwo", "one\ntwo"]
        );
    }

    #[test]
    fn fr_ed02_reverse_selection_replacement_is_grapheme_safe_and_undoable() {
        let mut doc = Document::new(DocId(11), "甲 e\u{301} 👩‍💻 乙");
        let start = "甲 ".len();
        let end = "甲 e\u{301} 👩‍💻".len();
        let selection = Selection::new(end, start);
        assert!(selection.reversed());
        assert_eq!(
            doc.selected_text(selection).expect("selected text"),
            "e\u{301} 👩‍💻"
        );
        assert_eq!(
            doc.replace_selection(selection, "中").expect("replace"),
            "中".len()
        );
        assert_eq!(doc.text(), "甲 中 乙");
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "甲 e\u{301} 👩‍💻 乙");
    }

    #[test]
    fn fr_ed08_line_comment_and_delete_are_atomic() {
        let mut doc = Document::new(DocId(1), "one\ntwo\n");
        doc.toggle_line_comment(0, "//").expect("comment");
        assert_eq!(doc.text(), "// one\ntwo\n");
        doc.toggle_line_comment(0, "//").expect("uncomment");
        assert_eq!(doc.text(), "one\ntwo\n");
        doc.delete_line(0).expect("delete");
        assert_eq!(doc.text(), "two\n");
    }

    #[test]
    fn fr_ed08_multi_line_comment_uses_minimum_indent_and_undoes_once() {
        let mut doc = Document::new(DocId(12), "  一\n    二\n三\n");
        assert!(doc.toggle_line_comments(0, 2, "//").expect("comment lines"));
        assert_eq!(doc.text(), "  // 一\n    // 二\n// 三\n");
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "  一\n    二\n三\n");
    }

    #[test]
    fn fr_ed08_duplicate_and_move_line_preserve_utf8() {
        let mut doc = Document::new(DocId(1), "一\n二\n三\n");
        doc.duplicate_line(1).expect("duplicate");
        assert_eq!(doc.text(), "一\n二\n二\n三\n");
        doc.move_line(2, -1).expect("move up");
        assert_eq!(doc.text(), "一\n二\n二\n三\n");
        doc.move_line(0, 1).expect("move down");
        assert_eq!(doc.text(), "二\n一\n二\n三\n");
    }

    #[test]
    fn fr_ed08_sort_join_and_undo_are_one_transaction() {
        let mut doc = Document::new(DocId(9), "香蕉\n苹果\n苹果\n梨\n");
        assert!(doc.sort_lines(0, 3, false, true).expect("sort"));
        assert_eq!(doc.text(), "梨\n苹果\n香蕉\n");
        assert!(doc.undo().is_some());
        assert_eq!(doc.text(), "香蕉\n苹果\n苹果\n梨\n");
        assert!(doc.join_lines(0, 1, ", ").expect("join"));
        assert_eq!(doc.text(), "香蕉, 苹果\n苹果\n梨\n");
    }
}
