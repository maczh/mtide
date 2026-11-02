//! Shared line diff and selective hunk application primitives.

use std::ops::Range;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffOp {
    Equal(String),
    Delete(String),
    Insert(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hunk {
    pub id: usize,
    pub old_range: Range<usize>,
    pub new_range: Range<usize>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineDiff {
    pub operations: Vec<DiffOp>,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DiffError {
    #[error("hunk {0} is not part of this diff")]
    UnknownHunk(usize),
    #[error("hunk application context does not match the original text")]
    ContextMismatch,
}

pub fn diff_lines(old: &str, new: &str) -> LineDiff {
    diff_lines_with_options(old, new, false)
}

/// Computes a line diff while optionally comparing lines by their normalized
/// whitespace. The original lines are retained in operations and hunks so a
/// caller can still render and apply the exact source text.
pub fn diff_lines_with_options(old: &str, new: &str, ignore_whitespace: bool) -> LineDiff {
    let old_lines = old.lines().map(str::to_owned).collect::<Vec<_>>();
    let new_lines = new.lines().map(str::to_owned).collect::<Vec<_>>();
    let old_keys = old_lines
        .iter()
        .map(|line| diff_key(line, ignore_whitespace))
        .collect::<Vec<_>>();
    let new_keys = new_lines
        .iter()
        .map(|line| diff_key(line, ignore_whitespace))
        .collect::<Vec<_>>();
    let lengths = (old_lines.len(), new_lines.len());
    let mut lcs = vec![vec![0usize; lengths.1 + 1]; lengths.0 + 1];
    for old_index in (0..lengths.0).rev() {
        for new_index in (0..lengths.1).rev() {
            lcs[old_index][new_index] = if old_keys[old_index] == new_keys[new_index] {
                lcs[old_index + 1][new_index + 1] + 1
            } else {
                lcs[old_index + 1][new_index].max(lcs[old_index][new_index + 1])
            };
        }
    }
    let mut operations = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < lengths.0 && new_index < lengths.1 {
        if old_keys[old_index] == new_keys[new_index] {
            operations.push(DiffOp::Equal(old_lines[old_index].clone()));
            old_index += 1;
            new_index += 1;
        } else if lcs[old_index + 1][new_index] >= lcs[old_index][new_index + 1] {
            operations.push(DiffOp::Delete(old_lines[old_index].clone()));
            old_index += 1;
        } else {
            operations.push(DiffOp::Insert(new_lines[new_index].clone()));
            new_index += 1;
        }
    }
    while old_index < lengths.0 {
        operations.push(DiffOp::Delete(old_lines[old_index].clone()));
        old_index += 1;
    }
    while new_index < lengths.1 {
        operations.push(DiffOp::Insert(new_lines[new_index].clone()));
        new_index += 1;
    }
    let hunks = make_hunks(&operations);
    LineDiff { operations, hunks }
}

fn diff_key(line: &str, ignore_whitespace: bool) -> String {
    if ignore_whitespace {
        line.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    } else {
        line.to_owned()
    }
}

fn make_hunks(operations: &[DiffOp]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut index = 0;
    let mut old_line = 0;
    let mut new_line = 0;
    while index < operations.len() {
        if matches!(operations[index], DiffOp::Equal(_)) {
            old_line += 1;
            new_line += 1;
            index += 1;
            continue;
        }
        let start_old = old_line;
        let start_new = new_line;
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        while index < operations.len() && !matches!(operations[index], DiffOp::Equal(_)) {
            match &operations[index] {
                DiffOp::Delete(line) => {
                    old_lines.push(line.clone());
                    old_line += 1;
                }
                DiffOp::Insert(line) => {
                    new_lines.push(line.clone());
                    new_line += 1;
                }
                DiffOp::Equal(_) => unreachable!(),
            }
            index += 1;
        }
        hunks.push(Hunk {
            id: hunks.len(),
            old_range: start_old..old_line,
            new_range: start_new..new_line,
            old_lines,
            new_lines,
        });
    }
    hunks
}

pub fn apply_hunks(
    original: &str,
    diff: &LineDiff,
    accepted: &[usize],
) -> Result<String, DiffError> {
    let mut accepted = accepted.to_vec();
    accepted.sort_unstable();
    accepted.dedup();
    for id in &accepted {
        if diff.hunks.get(*id).is_none() {
            return Err(DiffError::UnknownHunk(*id));
        }
    }
    let original_lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut cursor = 0;
    for hunk in &diff.hunks {
        let start = hunk.old_range.start;
        if original_lines.get(start..hunk.old_range.end) != Some(hunk.old_lines.as_slice()) {
            return Err(DiffError::ContextMismatch);
        }
        result.extend_from_slice(&original_lines[cursor..start]);
        if accepted.binary_search(&hunk.id).is_ok() {
            result.extend(hunk.new_lines.clone());
        } else {
            result.extend(hunk.old_lines.clone());
        }
        cursor = hunk.old_range.end;
    }
    result.extend_from_slice(&original_lines[cursor..]);
    let mut text = result.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{apply_hunks, diff_lines, diff_lines_with_options, DiffError, DiffOp};

    #[test]
    fn fr_gt06_diff_computes_hunks_and_applies_selected_change_only() {
        let diff = diff_lines("a\nb\nc\n", "a\nB\nc\nD\n");
        assert!(diff.operations.contains(&DiffOp::Delete("b".into())));
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(
            apply_hunks("a\nb\nc\n", &diff, &[0]).expect("apply hunk"),
            "a\nB\nc\n"
        );
        assert_eq!(
            apply_hunks("a\nb\nc\n", &diff, &[1]).expect("apply hunk"),
            "a\nb\nc\nD\n"
        );
    }

    #[test]
    fn fr_gt06_diff_rejects_unknown_or_changed_context() {
        let diff = diff_lines("a\nb", "a\nB");
        assert_eq!(
            apply_hunks("a\nb", &diff, &[9]),
            Err(DiffError::UnknownHunk(9))
        );
        assert_eq!(
            apply_hunks("a\nc", &diff, &[0]),
            Err(DiffError::ContextMismatch)
        );
    }

    #[test]
    fn fr_gt06_diff_can_ignore_whitespace_without_losing_original_lines() {
        let diff = diff_lines_with_options("let value = 1;\n", "  let   value=1;  \n", true);
        assert!(diff.hunks.is_empty());
        assert_eq!(
            diff.operations,
            vec![DiffOp::Equal("let value = 1;".into())]
        );
        assert_eq!(
            diff_lines("let value = 1;\n", "  let   value=1;  \n")
                .hunks
                .len(),
            1
        );
    }
}
