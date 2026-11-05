use regex::Regex;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

// Keep interactive searches responsive even when a file is opened directly
// from a system directory containing many unrelated entries.
const MAX_SEARCH_ENTRIES: usize = 5_000;

struct SearchBudget {
    remaining: usize,
    remaining_bytes: u64,
}

#[allow(dead_code)]
pub fn search_workspace_roots(roots: &[PathBuf], query: &str) -> Vec<SearchResult> {
    search_workspace_roots_with_globs(roots, query, &[], &[])
}

pub fn search_workspace_roots_with_globs(
    roots: &[PathBuf],
    query: &str,
    include: &[String],
    exclude: &[String],
) -> Vec<SearchResult> {
    search_workspace_roots_with_options(roots, query, include, exclude, false)
}

pub fn search_workspace_roots_with_options(
    roots: &[PathBuf],
    query: &str,
    include: &[String],
    exclude: &[String],
    use_regex: bool,
) -> Vec<SearchResult> {
    search_workspace_roots_with_options_cancelled(roots, query, include, exclude, use_regex, None)
}

pub fn search_workspace_roots_with_options_cancelled(
    roots: &[PathBuf],
    query: &str,
    include: &[String],
    exclude: &[String],
    use_regex: bool,
    cancellation: Option<(&AtomicUsize, usize)>,
) -> Vec<SearchResult> {
    if query.is_empty() {
        return Vec::new();
    }
    if use_regex && Regex::new(query).is_err() {
        return Vec::new();
    }
    let mut results = Vec::new();
    let mut budget = SearchBudget {
        remaining: MAX_SEARCH_ENTRIES,
        remaining_bytes: 64 * 1024 * 1024,
    };
    for root in roots {
        visit(
            root,
            root,
            query,
            include,
            exclude,
            use_regex,
            &mut results,
            &mut budget,
            cancellation,
        );
        if budget.remaining == 0 || is_cancelled(cancellation) {
            break;
        }
    }
    results
}

/// Replaces matches using the same ASCII smart-case rule as workspace search.
/// A query containing an uppercase character is case-sensitive; otherwise only
/// ASCII letter case is ignored and non-ASCII characters remain exact.
pub fn replace_smart_case(content: &str, query: &str, replacement: &str) -> (String, usize) {
    if query.is_empty() {
        return (content.to_owned(), 0);
    }
    if query.chars().any(char::is_uppercase) {
        let count = content.matches(query).count();
        return (content.replace(query, replacement), count);
    }
    let mut output = String::with_capacity(content.len());
    let mut count = 0;
    for segment in content.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let (line, line_count) = replace_line_case_insensitive(line, query, replacement);
        output.push_str(&line);
        output.push_str(newline);
        count += line_count;
    }
    (output, count)
}

pub fn replace_with_options(
    content: &str,
    query: &str,
    replacement: &str,
    use_regex: bool,
) -> (String, usize) {
    if !use_regex {
        return replace_smart_case(content, query, replacement);
    }
    let Ok(regex) = Regex::new(query) else {
        return (content.to_owned(), 0);
    };
    let count = regex.find_iter(content).count();
    (regex.replace_all(content, replacement).into_owned(), count)
}

/// Replaces the first line-local match at or after `preferred_column`.
///
/// The caller supplies a freshly scanned line position rather than reusing a
/// previously cached byte range. This keeps single replacement safe after the
/// editor buffer changed while the search panel remained open.
pub fn replace_first_with_options(
    line: &str,
    query: &str,
    replacement: &str,
    use_regex: bool,
    preferred_column: usize,
) -> (String, bool) {
    if query.is_empty() {
        return (line.to_owned(), false);
    }
    if use_regex {
        let Ok(regex) = Regex::new(query) else {
            return (line.to_owned(), false);
        };
        if let Some(found) = regex
            .find_iter(line)
            .find(|matched| line[..matched.start()].chars().count() >= preferred_column)
        {
            let mut output = String::with_capacity(line.len());
            output.push_str(&line[..found.start()]);
            output.push_str(&regex.replace(&line[found.start()..found.end()], replacement));
            output.push_str(&line[found.end()..]);
            return (output, true);
        }
        return (line.to_owned(), false);
    }
    let case_sensitive = query.chars().any(char::is_uppercase);
    let query_len = query.chars().count();
    let mut candidate: Option<(usize, usize)> = None;
    for (column, _) in line.char_indices() {
        let text = &line[column..];
        let matches = text.chars().take(query_len).collect::<String>();
        if matches.chars().count() == query_len
            && (case_sensitive && matches == query
                || !case_sensitive && matches.eq_ignore_ascii_case(query))
            && line[..column].chars().count() >= preferred_column
        {
            candidate = Some((
                column,
                text.chars().take(query_len).map(char::len_utf8).sum(),
            ));
            break;
        }
    }
    let Some((start, length)) = candidate else {
        return (line.to_owned(), false);
    };
    let mut output = String::with_capacity(line.len());
    output.push_str(&line[..start]);
    output.push_str(replacement);
    output.push_str(&line[start + length..]);
    (output, true)
}

fn replace_line_case_insensitive(line: &str, query: &str, replacement: &str) -> (String, usize) {
    let mut output = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut count = 0;
    while cursor < line.len() {
        let matched = line
            .get(cursor..cursor.saturating_add(query.len()))
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(query));
        if matched {
            output.push_str(replacement);
            cursor += query.len();
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

#[allow(clippy::too_many_arguments)]
fn visit(
    path: &Path,
    root: &Path,
    query: &str,
    include: &[String],
    exclude: &[String],
    use_regex: bool,
    results: &mut Vec<SearchResult>,
    budget: &mut SearchBudget,
    cancellation: Option<(&AtomicUsize, usize)>,
) {
    if budget.remaining == 0 || is_cancelled(cancellation) {
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if is_cancelled(cancellation) {
                return;
            }
            budget.remaining = budget.remaining.saturating_sub(1);
            if budget.remaining == 0 {
                return;
            }
            // Do not follow symlinked directories. Apart from making workspace
            // search unexpectedly escape its roots, this can create cycles.
            if entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_symlink())
            {
                continue;
            }
            let child = entry.path();
            let relative = child.strip_prefix(root).unwrap_or(&child);
            if child
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with('.') || matches!(name, "target" | "node_modules" | "vendor")
                })
            {
                continue;
            }
            if matches_glob(relative, exclude, true) {
                continue;
            }
            visit(
                &child,
                root,
                query,
                include,
                exclude,
                use_regex,
                results,
                budget,
                cancellation,
            );
        }
    } else if metadata.len() <= 4 * 1024 * 1024
        && metadata.len() <= budget.remaining_bytes
        && (include.is_empty()
            || matches_glob(path.strip_prefix(root).unwrap_or(path), include, false))
        && !matches_glob(path.strip_prefix(root).unwrap_or(path), exclude, false)
    {
        budget.remaining_bytes -= metadata.len();
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        results.extend(search_text_with_options(path, &text, query, use_regex));
    }
}

fn is_cancelled(cancellation: Option<(&AtomicUsize, usize)>) -> bool {
    cancellation
        .is_some_and(|(generation, expected)| generation.load(Ordering::Acquire) != expected)
}

fn matches_glob(path: &Path, patterns: &[String], directory: bool) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let path = path.to_string_lossy().replace('\\', "/");
    let file_name = path.rsplit('/').next().unwrap_or(&path);
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().trim_matches('/').replace('\\', "/");
        if pattern.is_empty() {
            return false;
        }
        let candidate = if pattern.contains('/') {
            &path
        } else {
            file_name
        };
        (directory && (pattern == path || pattern.strip_suffix("/**") == Some(path.as_str())))
            || wildcard_match(candidate, &pattern)
            || pattern
                .strip_prefix("**/")
                .is_some_and(|suffix| wildcard_match(candidate, suffix))
            || (directory && wildcard_match(&format!("{path}/"), &format!("{pattern}/")))
    })
}

fn wildcard_match(value: &str, pattern: &str) -> bool {
    let value: Vec<char> = value.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut dp = vec![vec![false; pattern.len() + 1]; value.len() + 1];
    dp[0][0] = true;
    for row in 0..=value.len() {
        for column in 0..pattern.len() {
            if !dp[row][column] {
                continue;
            }
            match pattern[column] {
                '*' => {
                    dp[row][column + 1] = true;
                    if row < value.len() {
                        dp[row + 1][column] = true;
                    }
                }
                '?' if row < value.len() => dp[row + 1][column + 1] = true,
                character if row < value.len() && character == value[row] => {
                    dp[row + 1][column + 1] = true
                }
                _ => {}
            }
        }
    }
    dp[value.len()][pattern.len()]
}

#[allow(dead_code)]
pub fn search_text(path: &Path, text: &str, query: &str) -> Vec<SearchResult> {
    search_text_with_options(path, text, query, false)
}

pub fn search_text_with_options(
    path: &Path,
    text: &str,
    query: &str,
    use_regex: bool,
) -> Vec<SearchResult> {
    if query.is_empty() {
        return Vec::new();
    }
    if use_regex {
        let Ok(regex) = Regex::new(query) else {
            return Vec::new();
        };
        return text
            .lines()
            .enumerate()
            .flat_map(|(line, content)| {
                regex
                    .find_iter(content)
                    .map(|matched| SearchResult {
                        path: path.to_path_buf(),
                        line: line + 1,
                        column: content[..matched.start()].chars().count() + 1,
                        text: content.to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    text.lines()
        .enumerate()
        .flat_map(|(line, content)| {
            match_offsets(content, query)
                .into_iter()
                .map(|column| SearchResult {
                    path: path.to_path_buf(),
                    line: line + 1,
                    column: content[..column].chars().count() + 1,
                    text: content.to_owned(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn match_offsets(content: &str, query: &str) -> Vec<usize> {
    let case_sensitive = query.chars().any(char::is_uppercase);
    let (content, query) = if case_sensitive {
        (content.to_owned(), query.to_owned())
    } else {
        (content.to_ascii_lowercase(), query.to_ascii_lowercase())
    };
    let mut offsets = Vec::new();
    let mut start = 0;
    while let Some(offset) = content[start..].find(&query) {
        let column = start + offset;
        offsets.push(column);
        start = column + query.len();
        if start >= content.len() {
            break;
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn finds_multiple_matches_and_skips_hidden_directories() {
        let root = std::env::temp_dir().join(format!(
            "mtide-search-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".hidden")).expect("directory");
        fs::write(root.join("main.rs"), "let x = 1;\nlet x = 2;").expect("file");
        fs::write(root.join(".hidden/secret.rs"), "let x = 3;").expect("file");
        let found = search_workspace_roots(std::slice::from_ref(&root), "x");
        assert_eq!(found.len(), 2);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn smart_case_searches_ascii_without_losing_columns() {
        let root = std::env::temp_dir().join(format!("mtide-smart-search-{}", std::process::id()));
        fs::create_dir_all(&root).expect("directory");
        fs::write(root.join("main.rs"), "Alpha alpha ALPHA").expect("file");
        assert_eq!(
            search_workspace_roots(std::slice::from_ref(&root), "alpha").len(),
            3
        );
        assert_eq!(
            search_workspace_roots(std::slice::from_ref(&root), "Alpha")[0].column,
            1
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn searches_all_workspace_roots() {
        let root = std::env::temp_dir().join(format!("mtide-roots-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).expect("first directory");
        fs::create_dir_all(&second).expect("second directory");
        fs::write(first.join("one.txt"), "needle").expect("first file");
        fs::write(second.join("two.txt"), "needle").expect("second file");
        let found = search_workspace_roots(&[first, second], "needle");
        assert_eq!(found.len(), 2);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cancelled_workspace_search_stops_before_traversal() {
        let root = std::env::temp_dir().join(format!("mtide-search-cancel-{}", std::process::id()));
        fs::create_dir_all(&root).expect("directory");
        fs::write(root.join("main.rs"), "needle").expect("file");
        let generation = AtomicUsize::new(1);
        let found = search_workspace_roots_with_options_cancelled(
            std::slice::from_ref(&root),
            "needle",
            &[],
            &[],
            false,
            Some((&generation, 2)),
        );
        assert!(found.is_empty());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn smart_case_replacement_matches_search_semantics() {
        let (replaced, count) = replace_smart_case("Alpha alpha ALPHA", "alpha", "done");
        assert_eq!(replaced, "done done done");
        assert_eq!(count, 3);
        let (replaced, count) = replace_smart_case("Alpha alpha ALPHA", "Alpha", "done");
        assert_eq!(replaced, "done alpha ALPHA");
        assert_eq!(count, 1);
    }

    #[test]
    fn include_and_exclude_globs_filter_workspace_files() {
        let root = std::env::temp_dir().join(format!("mtide-search-glob-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).expect("src directory");
        fs::create_dir_all(root.join("generated")).expect("generated directory");
        fs::write(root.join("src/main.rs"), "needle").expect("source");
        fs::write(root.join("root.rs"), "needle").expect("root source");
        fs::write(root.join("src/readme.md"), "needle").expect("markdown");
        fs::write(root.join("generated/code.rs"), "needle").expect("generated");
        let found = search_workspace_roots_with_globs(
            std::slice::from_ref(&root),
            "needle",
            &["**/*.rs".to_owned()],
            &["generated/**".to_owned()],
        );
        let mut paths: Vec<_> = found.iter().map(|result| result.path.clone()).collect();
        paths.sort();
        assert_eq!(paths, vec![root.join("root.rs"), root.join("src/main.rs")]);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn search_results_report_character_columns_for_unicode_text() {
        let path = PathBuf::from("unicode.rs");
        let found = search_text(&path, "你好 needle", "needle");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].column, 4);
    }

    #[test]
    fn regex_search_and_replacement_preserve_rune_columns() {
        let path = PathBuf::from("regex.rs");
        let found = search_text_with_options(&path, "你好 item1\nitem22", r"item\d+", true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].column, 4);
        let (replaced, count) = replace_with_options("item1 item22", r"item(\d+)", "value$1", true);
        assert_eq!(replaced, "value1 value22");
        assert_eq!(count, 2);
        assert!(search_text_with_options(&path, "text", "[", true).is_empty());
    }

    #[test]
    fn replace_first_rescans_from_a_character_column() {
        let (replaced, changed) =
            replace_first_with_options("alpha ALPHA", "alpha", "done", false, 0);
        assert!(changed);
        assert_eq!(replaced, "done ALPHA");
        let (replaced, changed) =
            replace_first_with_options("alpha ALPHA", "alpha", "done", false, 1);
        assert!(changed);
        assert_eq!(replaced, "alpha done");
    }

    #[test]
    fn replace_first_supports_regex_capture_replacement() {
        let (replaced, changed) =
            replace_first_with_options("item1 item22", r"item(\d+)", "value$1", true, 0);
        assert!(changed);
        assert_eq!(replaced, "value1 item22");
    }
}
