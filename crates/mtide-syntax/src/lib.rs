//! Lightweight language detection and semantic-token-ready spans.

use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Rust,
    Go,
    Python,
    JavaScript,
    Json,
    Markdown,
    PlainText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Keyword,
    Comment,
    String,
    Number,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldRegion {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub line: usize,
    pub kind: &'static str,
}

/// Extracts a conservative symbol list for jump-to-symbol. Byte positions are
/// deliberately omitted here; callers can resolve the line and name in their
/// document so this index remains valid after unrelated edits.
pub fn symbols(text: &str, language: Language) -> Vec<Symbol> {
    let prefixes: &[(&str, &str)] = match language {
        Language::Rust | Language::Go => &[
            ("fn ", "function"),
            ("func ", "function"),
            ("struct ", "type"),
        ],
        Language::Python => &[("def ", "function"), ("class ", "class")],
        Language::JavaScript => &[("function ", "function"), ("class ", "class")],
        _ => &[],
    };
    text.lines()
        .enumerate()
        .filter_map(|(line, source)| {
            let trimmed = source.trim_start();
            prefixes.iter().find_map(|(prefix, kind)| {
                let rest = trimmed.strip_prefix(prefix)?;
                let name = rest
                    .split(|character: char| !(character.is_alphanumeric() || character == '_'))
                    .next()
                    .filter(|name| !name.is_empty())?;
                Some(Symbol {
                    name: name.to_owned(),
                    line,
                    kind,
                })
            })
        })
        .collect()
}

/// Computes conservative fold ranges from braces and region markers. The
/// result is intentionally language-agnostic so missing syntax packages can
/// safely fall back to it.
pub fn fold_regions(text: &str, language: Language) -> Vec<FoldRegion> {
    let mut regions = Vec::new();
    let mut braces = Vec::new();
    let mut explicit = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("#region")
            || lower.contains("// region")
            || lower.contains("#endregion")
            || lower.contains("// endregion")
        {
            explicit.push(line_number);
        }
        let ignored = highlight(line, language)
            .into_iter()
            .filter(|span| matches!(span.kind, TokenKind::String | TokenKind::Comment))
            .map(|span| span.start..span.end)
            .collect::<Vec<_>>();
        for (index, character) in line.char_indices() {
            if ignored.iter().any(|range| range.contains(&index)) {
                continue;
            }
            match character {
                '{' => braces.push(line_number),
                '}' => {
                    if let Some(start) = braces.pop() {
                        if start < line_number {
                            regions.push(FoldRegion {
                                start_line: start,
                                end_line: line_number,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for pair in explicit.chunks_exact(2) {
        if pair[0] < pair[1] {
            regions.push(FoldRegion {
                start_line: pair[0],
                end_line: pair[1],
            });
        }
    }
    regions.sort_by_key(|region| (region.start_line, region.end_line));
    regions.dedup();
    regions
}

pub fn detect(path: &Path) -> Language {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Language::Rust,
        "go" => Language::Go,
        "py" => Language::Python,
        "js" | "ts" => Language::JavaScript,
        "json" => Language::Json,
        "md" | "markdown" => Language::Markdown,
        _ => Language::PlainText,
    }
}

/// Detects a language using the filename first and a Unix shebang as a
/// fallback for extensionless files.
pub fn detect_with_text(path: &Path, text: &str) -> Language {
    let by_path = detect(path);
    if by_path != Language::PlainText {
        return by_path;
    }
    let first = text.lines().next().unwrap_or_default().to_ascii_lowercase();
    if first.starts_with("#!") && first.contains("python") {
        Language::Python
    } else if first.starts_with("#!") && (first.contains("node") || first.contains("deno")) {
        Language::JavaScript
    } else {
        Language::PlainText
    }
}

/// Finds safe, byte-indexed spans for common syntax without changing source text.
pub fn highlight(line: &str, language: Language) -> Vec<Span> {
    let keywords: &[&str] = match language {
        Language::Rust => &["fn", "let", "pub", "struct", "impl", "use", "mod", "match"],
        Language::Go => &["func", "package", "import", "type", "struct", "go"],
        Language::Python => &["def", "class", "import", "from", "return", "if", "for"],
        Language::JavaScript => &["function", "const", "let", "class", "return", "import"],
        _ => &[],
    };
    let mut spans = Vec::new();
    let comment_start = find_comment_start(line, language);
    let code = &line[..comment_start.unwrap_or(line.len())];
    for (start, word) in code
        .split_inclusive(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .scan(0, |offset, piece| {
            let current = *offset;
            *offset += piece.len();
            Some((current, piece))
        })
    {
        let word = word.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
        let word_start = start + line[start..].find(word).unwrap_or(0);
        if keywords.contains(&word) {
            spans.push(Span {
                start: word_start,
                end: word_start + word.len(),
                kind: TokenKind::Keyword,
            });
        }
    }
    let mut string_delimiter = None;
    let mut escaped = false;
    let mut string_start = 0;
    for (index, character) in code.char_indices() {
        if let Some(delimiter) = string_delimiter {
            if character == delimiter && !escaped {
                spans.push(Span {
                    start: string_start,
                    end: index + character.len_utf8(),
                    kind: TokenKind::String,
                });
                string_delimiter = None;
            }
            escaped = character == '\\' && !escaped;
            if character != '\\' {
                escaped = false;
            }
        } else if character == '"' || character == '\'' {
            string_start = index;
            string_delimiter = Some(character);
            escaped = false;
        }
    }
    let string_ranges = spans
        .iter()
        .filter(|span| span.kind == TokenKind::String)
        .map(|span| span.start..span.end)
        .collect::<Vec<_>>();
    spans.retain(|span| {
        span.kind == TokenKind::String
            || !string_ranges
                .iter()
                .any(|range| span.start < range.end && range.start < span.end)
    });
    let mut number_start = None;
    for (index, character) in code.char_indices() {
        if character.is_ascii_digit() {
            number_start.get_or_insert(index);
        } else if let Some(start) = number_start.take() {
            spans.push(Span {
                start,
                end: index,
                kind: TokenKind::Number,
            });
        }
    }
    if let Some(start) = number_start {
        spans.push(Span {
            start,
            end: code.len(),
            kind: TokenKind::Number,
        });
    }
    if let Some(start) = comment_start {
        spans.push(Span {
            start,
            end: line.len(),
            kind: TokenKind::Comment,
        });
    }
    spans
}

fn find_comment_start(line: &str, language: Language) -> Option<usize> {
    let hash_comments = matches!(language, Language::Python | Language::Markdown);
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if let Some(delimiter) = quote {
            if character == delimiter && !escaped {
                quote = None;
            }
            escaped = character == '\\' && !escaped;
            if character != '\\' {
                escaped = false;
            }
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
        } else if (hash_comments && character == '#')
            || (character == '/' && line[index..].starts_with("//"))
        {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fr_t07_detects_languages_and_byte_spans() {
        assert_eq!(detect(Path::new("main.rs")), Language::Rust);
        assert_eq!(detect(Path::new("README.MD")), Language::Markdown);
        assert_eq!(detect(Path::new("MAIN.RS")), Language::Rust);
        let spans = highlight("fn 中文() {} // 注释", Language::Rust);
        assert!(spans.contains(&Span {
            start: 0,
            end: 2,
            kind: TokenKind::Keyword
        }));
        assert_eq!(spans.last().map(|span| span.kind), Some(TokenKind::Comment));
        let literals = highlight("let value = 42; let text = \"fn\"", Language::Rust);
        assert!(literals.contains(&Span {
            start: 12,
            end: 14,
            kind: TokenKind::Number
        }));
        assert!(literals.iter().any(|span| span.kind == TokenKind::String));
        assert!(!literals
            .iter()
            .any(|span| span.kind == TokenKind::Keyword && span.start >= 27));
        let url = highlight("let url = \"https://example.test/#x\"", Language::Rust);
        assert!(!url.iter().any(|span| span.kind == TokenKind::Comment));
        let single = highlight("value = 'return # not a comment'", Language::Python);
        assert!(single.iter().any(|span| span.kind == TokenKind::String));
        assert!(!single.iter().any(|span| span.kind == TokenKind::Comment));
        let rust_hash = highlight("let value = 1 # not a Rust comment", Language::Rust);
        assert!(!rust_hash.iter().any(|span| span.kind == TokenKind::Comment));
    }

    #[test]
    fn fr_ed06_indexes_symbols_for_jump() {
        let symbols = symbols("fn main() {}\nstruct App {}\n", Language::Rust);
        assert_eq!(symbols[0].name, "main");
        assert_eq!(symbols[1].line, 1);
        assert_eq!(symbols[1].kind, "type");
    }

    #[test]
    fn fr_ed07_fold_regions_ignore_strings_and_comments() {
        let text = "fn main() {\n  let text = \"}\";\n  // { ignored\n}\n// region demo\nvalue\n// endregion\n";
        assert_eq!(
            fold_regions(text, Language::Rust),
            vec![
                FoldRegion {
                    start_line: 0,
                    end_line: 3
                },
                FoldRegion {
                    start_line: 4,
                    end_line: 6
                },
            ]
        );
    }

    #[test]
    fn fr_t07_detects_extensionless_shebang_languages() {
        assert_eq!(
            detect_with_text(Path::new("script"), "#!/usr/bin/env python3\nprint(1)"),
            Language::Python
        );
        assert_eq!(
            detect_with_text(Path::new("script"), "#!/usr/bin/node\nconsole.log(1)"),
            Language::JavaScript
        );
        assert_eq!(
            detect_with_text(Path::new("main.rs"), "#!/usr/bin/env python3"),
            Language::Rust
        );
    }
}
