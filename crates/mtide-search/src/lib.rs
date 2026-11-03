//! Dependency-free, cancellable workspace search used before the indexed backend.

use regex::Regex;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub max_file_bytes: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            regex: false,
            case_sensitive: true,
            whole_word: false,
            max_file_bytes: 50 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("search I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub fn search(
    root: &Path,
    query: &str,
    token: &CancellationToken,
) -> Result<Vec<SearchMatch>, SearchError> {
    search_with_options(root, query, &SearchOptions::default(), token)
}

pub fn search_with_options(
    root: &Path,
    query: &str,
    options: &SearchOptions,
    token: &CancellationToken,
) -> Result<Vec<SearchMatch>, SearchError> {
    search_roots_with_options(&[root.to_path_buf()], query, options, token)
}

/// Replaces matches in one UTF-8 document. Regex mode supports capture-group
/// expansion using the same `$1` syntax as the `regex` crate.
pub fn replace_text(
    text: &str,
    query: &str,
    replacement: &str,
    options: &SearchOptions,
) -> Result<(String, usize), SearchError> {
    if query.is_empty() {
        return Ok((text.to_owned(), 0));
    }
    let matcher = Matcher::new(query, options)?;
    let count = matcher.regex.find_iter(text).count();
    if count == 0 {
        return Ok((text.to_owned(), 0));
    }
    Ok((
        matcher.regex.replace_all(text, replacement).into_owned(),
        count,
    ))
}

/// Searches all workspace roots and keeps absolute paths so callers can
/// display the owning root without losing identity.
pub fn search_roots(
    roots: &[PathBuf],
    query: &str,
    token: &CancellationToken,
) -> Result<Vec<SearchMatch>, SearchError> {
    search_roots_with_options(roots, query, &SearchOptions::default(), token)
}

pub fn search_roots_with_options(
    roots: &[PathBuf],
    query: &str,
    options: &SearchOptions,
    token: &CancellationToken,
) -> Result<Vec<SearchMatch>, SearchError> {
    let mut results = Vec::new();
    let matcher = Matcher::new(query, options)?;
    if query.is_empty() {
        return Ok(results);
    }
    for root in roots {
        if token.is_cancelled() {
            break;
        }
        let mut rules = IgnoreRules::load(root);
        visit(
            root,
            root,
            &matcher,
            options.max_file_bytes,
            token,
            &mut rules,
            &mut results,
        )?;
    }
    results.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
    Ok(results)
}

pub fn find_files(
    root: &Path,
    query: &str,
    token: &CancellationToken,
) -> Result<Vec<PathBuf>, SearchError> {
    let mut scored = Vec::new();
    let mut rules = IgnoreRules::load(root);
    find_files_inner(
        root,
        root,
        &query.to_lowercase(),
        token,
        &mut rules,
        &mut scored,
    )?;
    scored.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(scored.into_iter().map(|(_, path)| path).collect())
}

pub fn find_files_roots(
    roots: &[PathBuf],
    query: &str,
    token: &CancellationToken,
) -> Result<Vec<PathBuf>, SearchError> {
    let mut results = Vec::new();
    for root in roots {
        if token.is_cancelled() {
            break;
        }
        let mut local = find_files(root, query, token)?;
        results.append(&mut local);
    }
    results.sort();
    results.dedup();
    Ok(results)
}

fn find_files_inner(
    root: &Path,
    path: &Path,
    query: &str,
    token: &CancellationToken,
    rules: &mut IgnoreRules,
    results: &mut Vec<(usize, PathBuf)>,
) -> Result<(), SearchError> {
    if token.is_cancelled() {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if path != root
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if path != root && rules.matches(root, path, metadata.is_dir()) {
        return Ok(());
    }
    if metadata.is_dir() {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".git" || name == ".mtide")
        {
            return Ok(());
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if path != root && error.kind() == io::ErrorKind::PermissionDenied => {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        rules.push_dir(path);
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            find_files_inner(root, &entry.path(), query, token, rules, results)?;
        }
        rules.pop_dir(path);
    } else if metadata.is_file() {
        let name = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_lowercase();
        if let Some(score) = fuzzy_score(&name, query) {
            results.push((score, path.to_path_buf()));
        }
    }
    Ok(())
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let mut position = 0;
    let mut score = 0;
    for wanted in query.chars() {
        let relative = candidate[position..].find(wanted)?;
        position += relative + wanted.len_utf8();
        score += relative;
    }
    Some(score)
}

struct Matcher {
    regex: Regex,
}

impl Matcher {
    fn new(query: &str, options: &SearchOptions) -> Result<Self, SearchError> {
        let pattern = if options.regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let pattern = if options.whole_word {
            format!(r"\b(?:{pattern})\b")
        } else {
            pattern
        };
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .build()
            .map_err(|error| SearchError::InvalidPattern(error.to_string()))?;
        Ok(Self { regex })
    }
}

fn visit(
    root: &Path,
    path: &Path,
    matcher: &Matcher,
    max_file_bytes: u64,
    token: &CancellationToken,
    rules: &mut IgnoreRules,
    results: &mut Vec<SearchMatch>,
) -> Result<(), SearchError> {
    if token.is_cancelled() {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if path != root
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    };
    if path != root && rules.matches(root, path, metadata.is_dir()) {
        return Ok(());
    }
    if metadata.is_dir() {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".git" || name == ".mtide")
        {
            return Ok(());
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if path != root && error.kind() == io::ErrorKind::PermissionDenied => {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        rules.push_dir(path);
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            visit(
                root,
                &entry.path(),
                matcher,
                max_file_bytes,
                token,
                rules,
                results,
            )?;
        }
        rules.pop_dir(path);
    } else if metadata.is_file() {
        if max_file_bytes > 0 && metadata.len() > max_file_bytes {
            return Ok(());
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        if bytes.contains(&0) {
            return Ok(());
        }
        if let Ok(text) = String::from_utf8(bytes) {
            for (line_index, line) in text.lines().enumerate() {
                if token.is_cancelled() {
                    return Ok(());
                }
                for matched in matcher.regex.find_iter(line) {
                    let byte_column = matched.start();
                    results.push(SearchMatch {
                        path: path.to_path_buf(),
                        line: line_index + 1,
                        column: line[..byte_column].chars().count(),
                        text: line.to_owned(),
                    });
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct IgnoreRules {
    /// One pattern set per directory that contributed a `.gitignore`, deepest
    /// last. Nested files override their parents, mirroring git semantics.
    sets: Vec<IgnoreSet>,
}

#[derive(Clone, Debug)]
struct IgnoreSet {
    base: PathBuf,
    patterns: Vec<IgnorePattern>,
}

#[derive(Clone, Debug)]
struct IgnorePattern {
    segments: Vec<String>,
    negated: bool,
    directory_only: bool,
}

impl IgnoreRules {
    fn load(root: &Path) -> Self {
        let mut rules = Self::default();
        rules.push_dir(root);
        rules
    }

    /// Loads `dir/.gitignore` (when present) as the innermost pattern set.
    fn push_dir(&mut self, dir: &Path) {
        let Ok(text) = fs::read_to_string(dir.join(".gitignore")) else {
            return;
        };
        let patterns = text
            .lines()
            .filter_map(parse_ignore_pattern)
            .collect::<Vec<_>>();
        if !patterns.is_empty() {
            self.sets.push(IgnoreSet {
                base: dir.to_path_buf(),
                patterns,
            });
        }
    }

    fn pop_dir(&mut self, dir: &Path) {
        if self.sets.last().is_some_and(|set| set.base == dir) {
            self.sets.pop();
        }
    }

    fn matches(&self, root: &Path, path: &Path, is_dir: bool) -> bool {
        for set in self.sets.iter().rev() {
            let Ok(relative) = path.strip_prefix(&set.base) else {
                continue;
            };
            let segments = relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(value) => {
                        Some(value.to_string_lossy().into_owned())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if segments.is_empty() {
                continue;
            }
            for pattern in set.patterns.iter().rev() {
                if pattern_matches(pattern, &segments, is_dir) {
                    return !pattern.negated;
                }
            }
        }
        let _ = root;
        false
    }
}

fn parse_ignore_pattern(line: &str) -> Option<IgnorePattern> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (negated, body) = match trimmed.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let (directory_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let body = body.trim_start_matches('/');
    if body.is_empty() {
        return None;
    }
    Some(IgnorePattern {
        segments: body.split('/').map(str::to_owned).collect(),
        negated,
        directory_only,
    })
}

fn pattern_matches(pattern: &IgnorePattern, path: &[String], is_dir: bool) -> bool {
    let pattern_segments = pattern
        .segments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let text = path.iter().map(String::as_str).collect::<Vec<_>>();
    let single = pattern.segments.len() == 1;
    let match_here = |candidate: &[&str]| {
        if single {
            candidate
                .last()
                .is_some_and(|name| glob_segment(pattern_segments[0], name))
        } else {
            glob_segments(&pattern_segments, candidate)
        }
    };
    if pattern.directory_only {
        for depth in 1..=text.len() {
            let is_leaf = depth == text.len();
            if match_here(&text[..depth]) && (!is_leaf || is_dir) {
                return true;
            }
        }
        return false;
    }
    match_here(&text)
}

fn glob_segments(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&"**", rest)), _) => {
            glob_segments(rest, path) || (!path.is_empty() && glob_segments(pattern, &path[1..]))
        }
        (Some(_), None) => false,
        (Some((&head, rest)), Some((&name, remaining))) => {
            glob_segment(head, name) && glob_segments(rest, remaining)
        }
    }
}

fn glob_segment(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let (mut pattern_index, mut text_index) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;
    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            mark = text_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            mark += 1;
            text_index = mark;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_fixture(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn fr_sr02_nested_gitignore_and_negation_are_honoured() {
        let root = std::env::temp_dir().join(format!(
            "mtide-search-ignore-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("packages").join("a").join("dist")).expect("create fixture");
        fs::write(root.join(".gitignore"), "*.log\n!keep.log\n").expect("write root ignore");
        fs::write(
            root.join("packages").join("a").join(".gitignore"),
            "dist/\n",
        )
        .expect("write nested ignore");
        fs::write(root.join("debug.log"), "hidden").expect("write log");
        fs::write(root.join("keep.log"), "kept").expect("write keep");
        fs::write(
            root.join("packages").join("a").join("dist").join("out.txt"),
            "dist",
        )
        .expect("write dist");
        fs::write(root.join("packages").join("a").join("src.rs"), "source").expect("write src");

        let token = CancellationToken::default();
        let matches = search_roots(std::slice::from_ref(&root), "e", &token).expect("search");
        let found = matches
            .iter()
            .filter(|entry| entry.text.contains('e'))
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            found.contains("keep.log"),
            "negation must re-include the file"
        );
        assert!(!found.contains("debug.log"), "root ignore must hide *.log");
        assert!(
            !found.iter().any(|path| path.contains("dist")),
            "nested .gitignore must hide its subtree"
        );
        assert!(found.contains("packages/a/src.rs"));
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn fr_sr01_finds_text_and_supports_cancellation() {
        let root = unique_fixture("mtide-search");
        fs::create_dir_all(&root).expect("create fixture");
        fs::write(root.join("main.rs"), "fn main() {}\n// target target").expect("write fixture");
        let token = CancellationToken::default();
        let hits = search(&root, "target", &token).expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].column, 10);
        assert!(search(&root, "", &token).expect("empty search").is_empty());
        fs::create_dir_all(root.join(".git")).expect("create ignored directory");
        fs::write(root.join(".git").join("ignored.rs"), "target").expect("write ignored file");
        fs::create_dir_all(root.join(".mtide")).expect("create session directory");
        fs::write(root.join(".mtide").join("ignored.rs"), "target").expect("write ignored file");
        fs::write(root.join("中文.rs"), "中文 target").expect("write unicode fixture");
        fs::write(root.join(".gitignore"), "ignored/\n*.secret\n").expect("write gitignore");
        fs::create_dir_all(root.join("ignored")).expect("create ignored directory");
        fs::write(root.join("ignored").join("hidden.rs"), "target").expect("write ignored file");
        fs::write(root.join("private.secret"), "target").expect("write ignored suffix");
        let unicode_hits = search(&root, "target", &token).expect("unicode search");
        assert_eq!(unicode_hits[0].column, 3);
        assert!(!unicode_hits
            .iter()
            .any(|hit| hit.path.ends_with("hidden.rs")));
        assert!(!unicode_hits
            .iter()
            .any(|hit| hit.path.ends_with("private.secret")));
        let files = find_files(&root, "main", &token).expect("file search");
        assert!(files.contains(&root.join("main.rs")));
        fs::create_dir_all(root.join("src")).expect("create source directory");
        fs::write(root.join("src").join("main_runner.rs"), "").expect("write fuzzy file");
        let fuzzy = find_files(&root, "mnrs", &token).expect("fuzzy search");
        assert!(fuzzy.contains(&root.join("src").join("main_runner.rs")));
        token.cancel();
        assert!(search(&root, "target", &token)
            .expect("cancelled search")
            .is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws01_searches_multiple_roots_and_preserves_root_paths() {
        let base = unique_fixture("mtide-search-roots");
        let first = base.join("one");
        let second = base.join("two");
        fs::create_dir_all(&first).expect("first root");
        fs::create_dir_all(&second).expect("second root");
        fs::write(first.join("a.txt"), "needle").expect("first file");
        fs::write(second.join("b.txt"), "needle").expect("second file");
        let token = CancellationToken::default();
        let hits =
            search_roots(&[first.clone(), second.clone()], "needle", &token).expect("root search");
        let paths = hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths, vec![first.join("a.txt"), second.join("b.txt")]);
        let files = find_files_roots(&[first.clone(), second.clone()], ".txt", &token)
            .expect("root file search");
        assert_eq!(files.len(), 2);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn fr_sr02_search_options_support_regex_case_and_whole_word() {
        let root = unique_fixture("mtide-search-options");
        fs::create_dir_all(&root).expect("create fixture");
        fs::write(root.join("main.rs"), "Foo foo food\n").expect("write fixture");
        let token = CancellationToken::default();
        let options = SearchOptions {
            regex: false,
            case_sensitive: false,
            whole_word: true,
            ..SearchOptions::default()
        };
        let hits = search_with_options(&root, "foo", &options, &token).expect("search options");
        assert_eq!(hits.len(), 2);
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let hits = search_with_options(&root, r"F.o", &options, &token).expect("regex search");
        assert_eq!(hits.len(), 1);
        let error = search_with_options(&root, "[", &options, &token).expect_err("bad regex");
        assert!(matches!(error, SearchError::InvalidPattern(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_sr03_replace_text_supports_capture_groups_and_literal_mode() {
        let regex_options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        let (text, count) = replace_text(
            "name=alice\nname=bob",
            r"name=(\w+)",
            "user=$1",
            &regex_options,
        )
        .expect("replace regex");
        assert_eq!(count, 2);
        assert_eq!(text, "user=alice\nuser=bob");
        let (text, count) = replace_text("a.b a.b", "a.b", "x", &SearchOptions::default())
            .expect("replace literal");
        assert_eq!(count, 2);
        assert_eq!(text, "x x");
    }
}
