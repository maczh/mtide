//! Shared line diff and selective hunk application primitives.
//!
//! Exact LCS is computed for small ranges (it is both minimal and cheap there).
//! Larger ranges are split on "patience" anchors — lines that are unique in
//! both sides — and the gaps are diffed recursively. That keeps memory
//! proportional to the inputs instead of quadratic, which is what the Git diff
//! view, the editable diff and the AI patch gate need on files with tens of
//! thousands of lines.

use std::borrow::Cow;
use std::collections::HashMap;
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

/// One step of the edit script, expressed as indices into the two inputs so line
/// contents are materialised once at the end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Ranges up to this many cell comparisons are diffed exactly with LCS.
const EXACT_CELL_LIMIT: usize = 262_144;

/// Recursion guard for pathological inputs; past this depth the remaining range
/// is emitted as a plain replacement. The result is still a valid edit script.
const MAX_RECURSION_DEPTH: u32 = 64;

pub fn diff_lines(old: &str, new: &str) -> LineDiff {
    diff_lines_with_options(old, new, false)
}

/// Computes a line diff while optionally comparing lines by their normalized
/// (whitespace-free) form.
pub fn diff_lines_with_options(old: &str, new: &str, ignore_whitespace: bool) -> LineDiff {
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let mut steps = Vec::new();
    diff_slice(
        &old_lines,
        &new_lines,
        0,
        0,
        ignore_whitespace,
        0,
        &mut steps,
    );
    let operations = steps
        .into_iter()
        .map(|step| match step {
            Step::Equal(old_index, _) => DiffOp::Equal(old_lines[old_index].to_owned()),
            Step::Delete(old_index) => DiffOp::Delete(old_lines[old_index].to_owned()),
            Step::Insert(new_index) => DiffOp::Insert(new_lines[new_index].to_owned()),
        })
        .collect::<Vec<_>>();
    let hunks = make_hunks(&operations);
    LineDiff { operations, hunks }
}

fn same_line(old: &str, new: &str, ignore_whitespace: bool) -> bool {
    if old == new {
        return true;
    }
    if !ignore_whitespace {
        return false;
    }
    old.chars()
        .filter(|character| !character.is_whitespace())
        .eq(new.chars().filter(|character| !character.is_whitespace()))
}

fn diff_slice(
    old: &[&str],
    new: &[&str],
    old_base: usize,
    new_base: usize,
    ignore_whitespace: bool,
    depth: u32,
    out: &mut Vec<Step>,
) {
    let mut prefix = 0;
    while prefix < old.len()
        && prefix < new.len()
        && same_line(old[prefix], new[prefix], ignore_whitespace)
    {
        out.push(Step::Equal(old_base + prefix, new_base + prefix));
        prefix += 1;
    }

    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > prefix
        && new_end > prefix
        && same_line(old[old_end - 1], new[new_end - 1], ignore_whitespace)
    {
        old_end -= 1;
        new_end -= 1;
    }
    let suffix = new.len() - new_end;

    let old_mid = &old[prefix..old_end];
    let new_mid = &new[prefix..new_end];
    if new_mid.is_empty() {
        for index in 0..old_mid.len() {
            out.push(Step::Delete(old_base + prefix + index));
        }
    } else if old_mid.is_empty() {
        for index in 0..new_mid.len() {
            out.push(Step::Insert(new_base + prefix + index));
        }
    } else if old_mid.len().saturating_mul(new_mid.len()) <= EXACT_CELL_LIMIT {
        exact_diff(
            old_mid,
            new_mid,
            old_base + prefix,
            new_base + prefix,
            ignore_whitespace,
            out,
        );
    } else if depth >= MAX_RECURSION_DEPTH {
        emit_replacement(old_mid, new_mid, old_base + prefix, new_base + prefix, out);
    } else {
        let anchors = unique_anchors(old_mid, new_mid, ignore_whitespace);
        if anchors.is_empty() {
            emit_replacement(old_mid, new_mid, old_base + prefix, new_base + prefix, out);
        } else {
            let (mut old_cursor, mut new_cursor) = (0, 0);
            for (old_index, new_index) in anchors {
                diff_slice(
                    &old_mid[old_cursor..old_index],
                    &new_mid[new_cursor..new_index],
                    old_base + prefix + old_cursor,
                    new_base + prefix + new_cursor,
                    ignore_whitespace,
                    depth + 1,
                    out,
                );
                out.push(Step::Equal(
                    old_base + prefix + old_index,
                    new_base + prefix + new_index,
                ));
                old_cursor = old_index + 1;
                new_cursor = new_index + 1;
            }
            diff_slice(
                &old_mid[old_cursor..],
                &new_mid[new_cursor..],
                old_base + prefix + old_cursor,
                new_base + prefix + new_cursor,
                ignore_whitespace,
                depth + 1,
                out,
            );
        }
    }

    for index in 0..suffix {
        out.push(Step::Equal(
            old_base + old_end + index,
            new_base + new_end + index,
        ));
    }
}

fn emit_replacement(
    old: &[&str],
    new: &[&str],
    old_base: usize,
    new_base: usize,
    out: &mut Vec<Step>,
) {
    for index in 0..old.len() {
        out.push(Step::Delete(old_base + index));
    }
    for index in 0..new.len() {
        out.push(Step::Insert(new_base + index));
    }
}

/// Minimal line diff for a range that is small enough for an exact LCS table.
fn exact_diff(
    old: &[&str],
    new: &[&str],
    old_base: usize,
    new_base: usize,
    ignore_whitespace: bool,
    out: &mut Vec<Step>,
) {
    let rows = old.len();
    let columns = new.len();
    let width = columns + 1;
    let mut table = vec![0u32; (rows + 1) * width];
    for old_index in (0..rows).rev() {
        for new_index in (0..columns).rev() {
            table[old_index * width + new_index] =
                if same_line(old[old_index], new[new_index], ignore_whitespace) {
                    table[(old_index + 1) * width + new_index + 1] + 1
                } else {
                    table[(old_index + 1) * width + new_index]
                        .max(table[old_index * width + new_index + 1])
                };
        }
    }
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < rows && new_index < columns {
        if same_line(old[old_index], new[new_index], ignore_whitespace) {
            out.push(Step::Equal(old_base + old_index, new_base + new_index));
            old_index += 1;
            new_index += 1;
        } else if table[(old_index + 1) * width + new_index]
            >= table[old_index * width + new_index + 1]
        {
            out.push(Step::Delete(old_base + old_index));
            old_index += 1;
        } else {
            out.push(Step::Insert(new_base + new_index));
            new_index += 1;
        }
    }
    while old_index < rows {
        out.push(Step::Delete(old_base + old_index));
        old_index += 1;
    }
    while new_index < columns {
        out.push(Step::Insert(new_base + new_index));
        new_index += 1;
    }
}

fn anchor_key(line: &str, ignore_whitespace: bool) -> Cow<'_, str> {
    if ignore_whitespace {
        Cow::Owned(line.chars().filter(|c| !c.is_whitespace()).collect())
    } else {
        Cow::Borrowed(line)
    }
}

/// Lines that appear exactly once on both sides, kept in longest-increasing
/// order. They are safe split points: every occurrence is known to correspond.
fn unique_anchors(old: &[&str], new: &[&str], ignore_whitespace: bool) -> Vec<(usize, usize)> {
    let mut old_positions: HashMap<Cow<'_, str>, (usize, usize)> =
        HashMap::with_capacity(old.len());
    for (index, line) in old.iter().enumerate() {
        let entry = old_positions
            .entry(anchor_key(line, ignore_whitespace))
            .or_insert((0, index));
        entry.0 += 1;
        entry.1 = index;
    }
    let mut new_counts: HashMap<Cow<'_, str>, usize> = HashMap::with_capacity(new.len());
    for line in new {
        *new_counts
            .entry(anchor_key(line, ignore_whitespace))
            .or_insert(0) += 1;
    }

    let mut candidates = Vec::new();
    for (new_index, line) in new.iter().enumerate() {
        let key = anchor_key(line, ignore_whitespace);
        if new_counts.get(&key) != Some(&1) {
            continue;
        }
        if let Some(&(1, old_index)) = old_positions.get(&key) {
            candidates.push((old_index, new_index));
        }
    }
    longest_increasing_old(candidates)
}

/// Keeps the longest subsequence whose old indices increase. Candidate new
/// indices already increase because they were collected in document order.
fn longest_increasing_old(candidates: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if candidates.len() < 2 {
        return candidates;
    }
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; candidates.len()];
    for (index, (_, _)) in candidates.iter().enumerate() {
        let value = candidates[index].0;
        let position = tails.partition_point(|&tail_index| candidates[tail_index].0 < value);
        if position > 0 {
            previous[index] = tails[position - 1];
        }
        if position == tails.len() {
            tails.push(index);
        } else {
            tails[position] = index;
        }
    }
    let mut anchors = Vec::with_capacity(tails.len());
    let mut cursor = *tails.last().expect("tails is not empty");
    loop {
        anchors.push(candidates[cursor]);
        if previous[cursor] == usize::MAX {
            break;
        }
        cursor = previous[cursor];
    }
    anchors.reverse();
    anchors
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

/// Applies the accepted hunks to the original text, preserving the file's line
/// ending so a CRLF file is not silently rewritten as LF (PRD ED-13).
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
    let original_lines = original.lines().collect::<Vec<_>>();
    let mut result: Vec<&str> = Vec::new();
    let mut cursor = 0;
    for hunk in &diff.hunks {
        let start = hunk.old_range.start;
        let expected = hunk
            .old_lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if original_lines.get(start..hunk.old_range.end) != Some(expected.as_slice()) {
            return Err(DiffError::ContextMismatch);
        }
        result.extend_from_slice(&original_lines[cursor..start]);
        if accepted.binary_search(&hunk.id).is_ok() {
            result.extend(hunk.new_lines.iter().map(String::as_str));
        } else {
            result.extend_from_slice(&original_lines[start..hunk.old_range.end]);
        }
        cursor = hunk.old_range.end;
    }
    result.extend_from_slice(&original_lines[cursor..]);
    let ending = line_ending(original);
    let mut text = result.join(ending);
    if original.ends_with('\n') {
        text.push_str(ending);
    }
    Ok(text)
}

/// Detects the dominant line ending of a document.
fn line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    if crlf > 0 && crlf == lf {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_hunks, diff_lines, diff_lines_with_options, DiffError, DiffOp, LineDiff};

    fn reconstruct(diff: &LineDiff) -> String {
        let mut lines = Vec::new();
        for operation in &diff.operations {
            match operation {
                DiffOp::Equal(line) | DiffOp::Insert(line) => lines.push(line.clone()),
                DiffOp::Delete(_) => {}
            }
        }
        lines.join("\n")
    }

    fn equal_count(diff: &LineDiff) -> usize {
        diff.operations
            .iter()
            .filter(|operation| matches!(operation, DiffOp::Equal(_)))
            .count()
    }

    fn lcs_len(old: &[&str], new: &[&str]) -> usize {
        let mut table = vec![vec![0usize; new.len() + 1]; old.len() + 1];
        for old_index in (0..old.len()).rev() {
            for new_index in (0..new.len()).rev() {
                table[old_index][new_index] = if old[old_index] == new[new_index] {
                    table[old_index + 1][new_index + 1] + 1
                } else {
                    table[old_index + 1][new_index].max(table[old_index][new_index + 1])
                };
            }
        }
        table[0][0]
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

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

    #[test]
    fn fr_ed13_diff_preserves_crlf_line_endings() {
        let old = "fn main() {\r\n    let a = 1;\r\n}\r\n";
        let new = "fn main() {\r\n    let a = 2;\r\n}\r\n";
        let diff = diff_lines(old, new);
        assert_eq!(diff.hunks.len(), 1);
        let applied = apply_hunks(old, &diff, &[0]).expect("apply hunk");
        assert_eq!(applied, new);
        assert!(
            !applied.replace("\r\n", "").contains('\n'),
            "a CRLF file must not gain stray LF endings"
        );
    }

    #[test]
    fn fr_gt06_diff_matches_optimal_lcs_on_small_random_inputs() {
        let mut rng = Rng(0x5eed_1234_abcd_9876);
        for _ in 0..400 {
            let old_lines = (0..rng.below(14))
                .map(|_| WORDS[rng.below(WORDS.len())])
                .collect::<Vec<_>>();
            let new_lines = (0..rng.below(14))
                .map(|_| WORDS[rng.below(WORDS.len())])
                .collect::<Vec<_>>();
            let old_text = old_lines.join("\n");
            let new_text = new_lines.join("\n");
            let diff = diff_lines(&old_text, &new_text);
            assert_eq!(reconstruct(&diff), new_text, "reconstruction failed");
            assert_eq!(
                equal_count(&diff),
                lcs_len(&old_lines, &new_lines),
                "diff is not minimal for {old_lines:?} -> {new_lines:?}"
            );
        }
    }

    #[test]
    fn fr_gt06_diff_applies_every_hunk_to_reproduce_the_new_text() {
        let mut rng = Rng(0xdead_beef_0bad_f00d);
        for _ in 0..100 {
            let old_lines = (0..rng.below(60))
                .map(|_| WORDS[rng.below(WORDS.len())])
                .collect::<Vec<_>>();
            let new_lines = (0..rng.below(60))
                .map(|_| WORDS[rng.below(WORDS.len())])
                .collect::<Vec<_>>();
            let old_text = old_lines.join("\n");
            let new_text = new_lines.join("\n");
            let diff = diff_lines(&old_text, &new_text);
            let all = (0..diff.hunks.len()).collect::<Vec<_>>();
            assert_eq!(
                apply_hunks(&old_text, &diff, &all).expect("apply"),
                new_text
            );
        }
    }

    #[test]
    fn fr_gt06_diff_scales_to_twenty_thousand_lines_with_sparse_edits() {
        let old_lines = (0..20_000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let mut new_lines = old_lines.clone();
        for index in (0..new_lines.len()).step_by(997) {
            new_lines[index] = format!("edited {index}");
        }
        new_lines.insert(1_000, "inserted".to_owned());
        new_lines.remove(15_000);
        let old_text = old_lines.join("\n");
        let new_text = new_lines.join("\n");
        let diff = diff_lines(&old_text, &new_text);
        assert_eq!(reconstruct(&diff), new_text);
        assert!(
            (20..=24).contains(&diff.hunks.len()),
            "{} hunks",
            diff.hunks.len()
        );
        assert!(
            equal_count(&diff) >= 19_900,
            "anchored diff must keep the untouched lines equal"
        );
    }
}
