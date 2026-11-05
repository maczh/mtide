use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChange {
    pub index: char,
    pub worktree: char,
    pub path: String,
}

impl GitChange {
    pub const fn is_conflict(&self) -> bool {
        matches!(self.index, 'U' | 'A' | 'D') && matches!(self.worktree, 'U' | 'A' | 'D')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: char,
    pub text: String,
}

/// Backend boundary used by the UI so Git-specific commands do not leak into
/// workspace state handling. Future VCS implementations can provide the same
/// status/diff surface without changing panel code.
pub trait VcsBackend {
    fn root(&self) -> &Path;
    fn status(&self) -> Vec<GitChange>;
    fn diff(&self, change: &GitChange) -> Vec<DiffLine>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBackend {
    root: PathBuf,
}

impl GitBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl VcsBackend for GitBackend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn status(&self) -> Vec<GitChange> {
        status(&self.root)
    }

    fn diff(&self, change: &GitChange) -> Vec<DiffLine> {
        diff(&self.root, change)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestComment {
    pub id: u64,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub side: Option<String>,
    pub user: Option<PullRequestCommentUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestCommentUser {
    pub login: String,
}

#[derive(Debug, Clone, Serialize)]
struct PullRequestCommentRequest<'a> {
    body: &'a str,
    commit_id: &'a str,
    path: &'a str,
    line: u32,
    side: &'a str,
}

#[allow(dead_code)]
pub fn list_pull_request_comments(
    repository: &str,
    number: u64,
) -> AppResult<Vec<PullRequestComment>> {
    let endpoint = github_pull_request_api_endpoint(repository, number)?;
    let token = github_token()?;
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| AppError::Git(format!("build GitHub client: {error}")))?
        .get(endpoint)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "mtide")
        .send()
        .map_err(|error| AppError::Git(format!("list pull request comments: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError::Git(format!(
            "list pull request comments: HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .map_err(|error| AppError::Git(format!("read pull request comments: {error}")))?;
    serde_json::from_str(&body)
        .map_err(|error| AppError::Git(format!("decode pull request comments: {error}")))
}

#[allow(dead_code)]
pub fn create_pull_request_comment(
    repository: &str,
    number: u64,
    commit_id: &str,
    path: &str,
    line: u32,
    body: &str,
) -> AppResult<PullRequestComment> {
    if commit_id.trim().is_empty() || path.trim().is_empty() || line == 0 || body.trim().is_empty()
    {
        return Err(AppError::Git("invalid pull request comment".into()));
    }
    let endpoint = github_pull_request_api_endpoint(repository, number)?;
    let token = github_token()?;
    let payload = PullRequestCommentRequest {
        body,
        commit_id,
        path,
        line,
        side: "RIGHT",
    };
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| AppError::Git(format!("build GitHub client: {error}")))?
        .post(endpoint)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "mtide")
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&payload)
                .map_err(|error| AppError::Git(format!("encode pull request comment: {error}")))?,
        )
        .send()
        .map_err(|error| AppError::Git(format!("create pull request comment: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError::Git(format!(
            "create pull request comment: HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .map_err(|error| AppError::Git(format!("read pull request comment: {error}")))?;
    serde_json::from_str(&body)
        .map_err(|error| AppError::Git(format!("decode pull request comment: {error}")))
}

fn github_pull_request_api_endpoint(repository: &str, number: u64) -> AppResult<String> {
    if number == 0 || !repository.starts_with("https://github.com/") {
        return Err(AppError::Git("invalid GitHub pull request target".into()));
    }
    let path = repository
        .trim_start_matches("https://github.com/")
        .trim_end_matches(".git");
    let valid = path.split('/').count() == 2 && path.split('/').all(|part| !part.is_empty());
    if !valid || path.contains(['?', '#', ' ']) {
        return Err(AppError::Git("invalid GitHub repository".into()));
    }
    Ok(format!(
        "https://api.github.com/repos/{path}/pulls/{number}/comments"
    ))
}

fn github_token() -> AppResult<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| AppError::Git("GITHUB_TOKEN or GH_TOKEN is required".into()))
        .and_then(|token| {
            (!token.trim().is_empty())
                .then_some(token)
                .ok_or_else(|| AppError::Git("GitHub token must not be empty".into()))
        })
}

pub fn hunk_count(lines: &[DiffLine]) -> usize {
    lines.iter().filter(|line| line.kind == '@').count()
}

pub fn hunk_start_line(lines: &[DiffLine], hunk_index: usize) -> Option<usize> {
    lines
        .iter()
        .filter(|line| line.kind == '@')
        .nth(hunk_index)
        .and_then(|line| {
            let header = line.text.strip_prefix('@')?.trim_start();
            let old_range = header.strip_prefix('-')?.split_whitespace().next()?;
            old_range
                .split(',')
                .next()
                .and_then(|value| value.parse::<usize>().ok())
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub date: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBlock {
    pub start_line: usize,
    pub base_start: Option<usize>,
    pub base_end: Option<usize>,
    pub ours_start: usize,
    pub ours_end: usize,
    pub theirs_start: usize,
    pub theirs_end: usize,
    pub end_line: usize,
}

pub fn conflict_blocks(lines: &[String]) -> Vec<ConflictBlock> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !lines[index].starts_with("<<<<<<<") {
            index += 1;
            continue;
        }
        // Do not pair markers across another conflict start. A partially
        // written or hand-edited conflict must not hide later valid blocks.
        let mut separator = None;
        let mut cursor = index + 1;
        while cursor < lines.len() {
            if lines[cursor].starts_with("<<<<<<<") {
                break;
            }
            if lines[cursor].starts_with("=======") {
                separator = Some(cursor);
                break;
            }
            cursor += 1;
        }
        let Some(separator) = separator else {
            index += 1;
            continue;
        };
        let mut end = None;
        cursor = separator + 1;
        while cursor < lines.len() {
            if lines[cursor].starts_with("<<<<<<<") {
                break;
            }
            if lines[cursor].starts_with(">>>>>>>") {
                end = Some(cursor);
                break;
            }
            cursor += 1;
        }
        let Some(end) = end else {
            index += 1;
            continue;
        };
        let base_marker = lines[index + 1..separator]
            .iter()
            .position(|line| line.starts_with("|||||||"))
            .map(|offset| index + 1 + offset);
        let (ours_start, ours_end, base_start, base_end) = if let Some(base_marker) = base_marker {
            (
                index + 1,
                base_marker,
                Some(base_marker + 1),
                Some(separator),
            )
        } else {
            (index + 1, separator, None, None)
        };
        blocks.push(ConflictBlock {
            start_line: index,
            base_start,
            base_end,
            ours_start,
            ours_end,
            theirs_start: separator + 1,
            theirs_end: end,
            end_line: end,
        });
        index = end + 1;
    }
    blocks
}

pub fn status(root: &Path) -> Vec<GitChange> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_status_bytes(&output.stdout)
}

pub fn current_branch(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!branch.is_empty()).then_some(branch)
}

pub fn branches(root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn checkout(root: &Path, branch: &str) -> AppResult<()> {
    run_git(root, ["switch", branch])
}

pub fn create_branch(root: &Path, branch: &str) -> AppResult<()> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(AppError::Git("branch name must not be empty".into()));
    }
    run_git_with_args(root, &["switch", "-c", branch])
}

pub fn delete_branch(root: &Path, branch: &str) -> AppResult<()> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(AppError::Git("branch name must not be empty".into()));
    }
    run_git_with_args(root, &["branch", "-d", branch])
}

pub fn diff(root: &Path, change: &GitChange) -> Vec<DiffLine> {
    let path = PathBuf::from(&change.path);
    let diff_args = if change.index != ' ' && change.worktree == ' ' {
        ["diff", "--cached", "--no-ext-diff", "--unified=3"]
    } else {
        ["diff", "--no-ext-diff", "--unified=3", ""]
    };
    let mut command = Command::new("git");
    command
        .args(diff_args.iter().filter(|argument| !argument.is_empty()))
        .arg("--")
        .arg(&path)
        .current_dir(root);
    let output = command.output();
    if let Ok(output) = output {
        if !output.status.success() {
            return Vec::new();
        }
        let lines = parse_diff(&String::from_utf8_lossy(&output.stdout));
        if !lines.is_empty() {
            return lines;
        }
    }
    if change.index == '?' {
        let Ok(contents) = fs::read_to_string(root.join(&path)) else {
            return Vec::new();
        };
        return contents
            .lines()
            .map(|line| DiffLine {
                kind: '+',
                text: line.to_owned(),
            })
            .collect();
    }
    Vec::new()
}

#[allow(dead_code)]
pub fn diff_refs(root: &Path, base: &str, head: &str) -> Vec<DiffLine> {
    if base.trim().is_empty() || head.trim().is_empty() {
        return Vec::new();
    }
    let output = Command::new("git")
        .args(["diff", "--no-ext-diff", "--unified=3", base, head, "--"])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_diff(&String::from_utf8_lossy(&output.stdout))
}

#[allow(dead_code)]
pub fn resolve_ref(root: &Path, reference: &str) -> AppResult<String> {
    if reference.trim().is_empty() {
        return Err(AppError::Git("reference must not be empty".into()));
    }
    let output = Command::new("git")
        .args(["rev-parse", "--verify", reference])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(AppError::Git(format!("unable to resolve ref {reference}")));
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!oid.is_empty())
        .then_some(oid)
        .ok_or_else(|| AppError::Git(format!("unable to resolve ref {reference}")))
}

pub fn base_file(root: &Path, path: &str) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["show", &format!("HEAD:{path}")])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

pub fn stage(root: &Path, path: &str) -> AppResult<()> {
    run_git(root, ["add", "--", path])
}

pub fn unstage(root: &Path, path: &str) -> AppResult<()> {
    run_git(root, ["restore", "--staged", "--", path])
}

pub fn apply_hunk(
    root: &Path,
    change: &GitChange,
    hunk_index: usize,
    stage: bool,
) -> AppResult<()> {
    if change.index == '?' || change.worktree == '?' {
        return Err(AppError::Git("untracked files have no patch hunk".into()));
    }
    let staged_source = !stage;
    let raw = diff_text(root, &change.path, staged_source)?;
    let patch = extract_hunk_patch(&raw, hunk_index)
        .ok_or_else(|| AppError::Git("selected diff hunk is unavailable".into()))?;
    let mut args = vec!["apply", "--cached"];
    if !stage {
        args.push("--reverse");
    }
    let mut child = Command::new("git")
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| AppError::Git("git apply stdin unavailable".into()))?
        .write_all(patch.as_bytes())?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn diff_text(root: &Path, path: &str, staged: bool) -> AppResult<String> {
    let mut command = Command::new("git");
    command.args(["diff"]);
    if staged {
        command.arg("--cached");
    }
    let output = command
        .args(["--no-ext-diff", "--unified=3", "--", path])
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(root)
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(AppError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn extract_hunk_patch(raw: &str, hunk_index: usize) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let first_hunk = lines.iter().position(|line| line.starts_with("@@"))?;
    let hunk_starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("@@").then_some(index))
        .collect();
    let start = *hunk_starts.get(hunk_index)?;
    let end = hunk_starts
        .get(hunk_index + 1)
        .copied()
        .unwrap_or(lines.len());
    let mut patch = lines[..first_hunk].to_vec();
    patch.extend_from_slice(&lines[start..end]);
    Some(format!("{}\n", patch.join("\n")))
}

pub fn checkout_conflict_side(root: &Path, path: &str, ours: bool) -> AppResult<()> {
    let side = if ours { "--ours" } else { "--theirs" };
    run_git_with_args(root, &["checkout", side, "--", path])
}

pub fn commit(root: &Path, message: &str) -> AppResult<()> {
    let message = message.trim();
    if message.is_empty() {
        return Err(AppError::Git("commit message must not be empty".into()));
    }
    run_git_with_args(root, &["commit", "-m", message])
}

pub fn blame(root: &Path, path: &str) -> Vec<BlameLine> {
    let output = Command::new("git")
        .args(["blame", "--line-porcelain", "--", path])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_blame(&String::from_utf8_lossy(&output.stdout))
}

pub fn history_page(root: &Path, skip: usize, limit: usize) -> (Vec<GitCommit>, bool) {
    let limit = limit.clamp(1, 200);
    let fetch_limit = limit.saturating_add(1);
    let output = Command::new("git")
        .args([
            "log",
            &format!("-{fetch_limit}"),
            &format!("--skip={skip}"),
            "--date=short",
            "--pretty=format:%H%x1f%an%x1f%ad%x1f%s",
        ])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return (Vec::new(), false);
    };
    if !output.status.success() {
        return (Vec::new(), false);
    }
    let mut commits: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_history_line)
        .collect();
    let has_more = commits.len() > limit;
    commits.truncate(limit);
    (commits, has_more)
}

pub fn fetch(root: &Path) -> AppResult<()> {
    remote_action(root, ["fetch", "--all", "--prune"])
}

#[allow(dead_code)]
pub fn fetch_pull_request(root: &Path, repository: &str, number: u64) -> AppResult<String> {
    if number == 0 || !is_remote_repository(repository) {
        return Err(AppError::Git(
            "invalid pull request repository or number".into(),
        ));
    }
    let destination = format!("refs/mtide/pr/{number}");
    let refspec = format!("pull/{number}/head:{destination}");
    run_git_with_args(root, &["fetch", "--no-tags", repository, &refspec])?;
    Ok(destination)
}

#[allow(dead_code)]
fn is_remote_repository(repository: &str) -> bool {
    repository.starts_with("https://")
        || repository.starts_with("http://")
        || repository.starts_with("git@")
        || repository.starts_with("ssh://")
}

pub fn pull(root: &Path) -> AppResult<()> {
    remote_action(root, ["pull", "--ff-only"])
}

pub fn push(root: &Path) -> AppResult<()> {
    remote_action(root, ["push"])
}

pub fn sync(root: &Path) -> AppResult<()> {
    pull(root)?;
    push(root)
}

pub fn remote_with_credentials(
    root: &Path,
    operation: &str,
    username: &str,
    password: &str,
) -> AppResult<()> {
    if operation == "sync" {
        run_git_with_credentials(root, &["pull", "--ff-only"], username, password)?;
        return run_git_with_credentials(root, &["push"], username, password);
    }
    let args: &[&str] = match operation {
        "fetch" => &["fetch", "--all", "--prune"],
        "pull" => &["pull", "--ff-only"],
        "push" => &["push"],
        _ => {
            return Err(AppError::Git(format!(
                "unsupported remote operation: {operation}"
            )))
        }
    };
    run_git_with_credentials(root, args, username, password)
}

fn run_git_with_credentials(
    root: &Path,
    args: &[&str],
    username: &str,
    password: &str,
) -> AppResult<()> {
    let helper = "!f() { printf 'username=%s\\npassword=%s\\n' \"$MTIDE_GIT_USERNAME\" \"$MTIDE_GIT_PASSWORD\"; }; f";
    let output = Command::new("git")
        .arg("-c")
        .arg(format!("credential.helper={helper}"))
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("MTIDE_GIT_USERNAME", username)
        .env("MTIDE_GIT_PASSWORD", password)
        .current_dir(root)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(AppError::Git(if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        }))
    }
}

fn run_git<const N: usize>(root: &Path, args: [&str; N]) -> AppResult<()> {
    run_git_with_args(root, &args)
}

fn run_git_with_args(root: &Path, args: &[&str]) -> AppResult<()> {
    let output = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(root)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(AppError::Git(if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        }))
    }
}

fn remote_action<const N: usize>(root: &Path, args: [&str; N]) -> AppResult<()> {
    run_git_with_args(root, &args)
}

fn parse_history_line(line: &str) -> Option<GitCommit> {
    let mut fields = line.split('\x1f');
    Some(GitCommit {
        hash: fields.next()?.to_owned(),
        author: fields.next()?.to_owned(),
        date: fields.next()?.to_owned(),
        subject: fields.next()?.to_owned(),
    })
}

fn parse_blame(text: &str) -> Vec<BlameLine> {
    let mut lines = Vec::new();
    let mut commit = String::new();
    let mut author = String::new();
    let mut date = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("author ") {
            author = value.to_owned();
        } else if let Some(value) = line.strip_prefix("author-time ") {
            date = value.to_owned();
        } else if let Some(content) = line.strip_prefix('\t') {
            lines.push(BlameLine {
                commit: commit.clone(),
                author: author.clone(),
                date: date.clone(),
                line: content.to_owned(),
            });
        } else if line.split_once(' ').is_some()
            && !line.starts_with(' ')
            && line.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
        {
            commit = line
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
        }
    }
    lines
}

fn parse_diff(text: &str) -> Vec<DiffLine> {
    text.lines()
        .filter_map(|line| {
            if line.starts_with("@@") {
                return Some(DiffLine {
                    kind: '@',
                    text: line.get(1..).unwrap_or_default().to_owned(),
                });
            }
            let kind = line.chars().next()?;
            if matches!(kind, '+' | '-' | ' ') {
                Some(DiffLine {
                    kind,
                    text: line.get(1..).unwrap_or_default().to_owned(),
                })
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
fn parse_status_line(line: &str) -> Option<GitChange> {
    let bytes = line.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    Some(GitChange {
        index: bytes[0] as char,
        worktree: bytes[1] as char,
        path: line[3..].to_owned(),
    })
}

fn parse_status_bytes(bytes: &[u8]) -> Vec<GitChange> {
    let records: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while let Some(record) = records.get(index) {
        if record.len() < 4 {
            index += 1;
            continue;
        }
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        let status = GitChange {
            index: record[0] as char,
            worktree: record[1] as char,
            path,
        };
        if (matches!(status.index, 'R' | 'C') || matches!(status.worktree, 'R' | 'C'))
            && records.get(index + 1).is_some()
        {
            changes.push(status);
            index += 2;
            continue;
        }
        changes.push(status);
        index += 1;
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn parses_untracked_status() {
        assert_eq!(
            parse_status_line("?? src/main.rs"),
            Some(GitChange {
                index: '?',
                worktree: '?',
                path: "src/main.rs".into()
            })
        );
    }
    #[test]
    fn parses_modified_status() {
        assert_eq!(
            parse_status_line(" M src/app.rs"),
            Some(GitChange {
                index: ' ',
                worktree: 'M',
                path: "src/app.rs".into()
            })
        );
    }

    #[test]
    fn parses_porcelain_status_with_nul_separated_rename_and_special_paths() {
        let changes = parse_status_bytes(
            b"R  new name [draft].rs\0old name\0 M path with spaces.rs\0?? weird\nname\0",
        );
        assert_eq!(
            changes,
            vec![
                GitChange {
                    index: 'R',
                    worktree: ' ',
                    path: "new name [draft].rs".into(),
                },
                GitChange {
                    index: ' ',
                    worktree: 'M',
                    path: "path with spaces.rs".into(),
                },
                GitChange {
                    index: '?',
                    worktree: '?',
                    path: "weird\nname".into(),
                },
            ]
        );
    }

    #[test]
    fn reads_real_renamed_worktree_path_from_porcelain_status() {
        let root = std::env::temp_dir().join(format!("mtide-git-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git");
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(root.join("old name.txt"), "content\n").expect("file");
        run(&["add", "--", "old name.txt"]);
        run(&["commit", "-qm", "initial"]);
        fs::rename(root.join("old name.txt"), root.join("new name.txt")).expect("rename");
        run(&["add", "-A"]);
        assert_eq!(
            status(&root),
            vec![GitChange {
                index: 'R',
                worktree: ' ',
                path: "new name.txt".into(),
            }]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn identifies_conflict_status() {
        assert!(GitChange {
            index: 'U',
            worktree: 'U',
            path: "src/lib.rs".into(),
        }
        .is_conflict());
        assert!(!GitChange {
            index: ' ',
            worktree: 'M',
            path: "src/lib.rs".into(),
        }
        .is_conflict());
    }

    #[test]
    fn parses_conflict_blocks_with_optional_base_section() {
        let lines = vec![
            "before".into(),
            "<<<<<<< ours".into(),
            "ours".into(),
            "||||||| base".into(),
            "base".into(),
            "=======".into(),
            "theirs".into(),
            ">>>>>>> theirs".into(),
            "after".into(),
        ];
        assert_eq!(
            conflict_blocks(&lines),
            vec![ConflictBlock {
                start_line: 1,
                base_start: Some(4),
                base_end: Some(5),
                ours_start: 2,
                ours_end: 3,
                theirs_start: 6,
                theirs_end: 7,
                end_line: 7,
            }]
        );
    }

    #[test]
    fn skips_malformed_conflict_without_hiding_later_valid_block() {
        let lines = vec![
            "<<<<<<< broken".into(),
            "content without a separator".into(),
            "<<<<<<< ours".into(),
            "ours".into(),
            "=======".into(),
            "theirs".into(),
            ">>>>>>> theirs".into(),
        ];

        let blocks = conflict_blocks(&lines);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_line, 2);
        assert_eq!(blocks[0].ours_start, 3);
        assert_eq!(blocks[0].theirs_start, 5);
    }

    #[test]
    fn does_not_pair_nested_conflict_markers_across_blocks() {
        let lines = vec![
            "<<<<<<< outer".into(),
            "<<<<<<< inner".into(),
            "inner ours".into(),
            "=======".into(),
            "inner theirs".into(),
            ">>>>>>> inner".into(),
        ];

        let blocks = conflict_blocks(&lines);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_line, 1);
    }

    #[test]
    fn ignores_short_lines() {
        assert!(parse_status_line("M").is_none());
    }

    #[test]
    fn accepts_sync_as_a_credential_operation() {
        let error = remote_with_credentials(Path::new("/missing"), "sync", "user", "secret")
            .expect_err("missing repository");
        assert!(!error.to_string().contains("unsupported remote operation"));
    }

    #[test]
    fn parses_unified_diff_lines() {
        assert_eq!(
            parse_diff("@@ -1 +1 @@\n-old\n+new\n"),
            vec![
                DiffLine {
                    kind: '@',
                    text: "@ -1 +1 @@".into()
                },
                DiffLine {
                    kind: '-',
                    text: "old".into()
                },
                DiffLine {
                    kind: '+',
                    text: "new".into()
                },
            ]
        );
    }

    #[test]
    fn extracts_one_hunk_while_retaining_patch_headers() {
        let raw = "diff --git a/a.rs b/a.rs\nindex 111..222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n@@ -4 +4 @@\n-old2\n+new2\n";
        let patch = extract_hunk_patch(raw, 1).expect("hunk");
        assert!(patch.starts_with("diff --git a/a.rs b/a.rs\n"));
        assert!(patch.contains("@@ -4 +4 @@\n-old2\n+new2"));
        assert!(!patch.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn applies_and_reverses_one_real_worktree_hunk() {
        let root = std::env::temp_dir().join(format!("mtide-git-hunk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git");
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(
            root.join("file.txt"),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\n",
        )
        .expect("file");
        run(&["add", "file.txt"]);
        run(&["commit", "-qm", "initial"]);
        fs::write(
            root.join("file.txt"),
            "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\neleven\ntwelve\n",
        )
        .expect("modified file");
        let change = GitChange {
            index: ' ',
            worktree: 'M',
            path: "file.txt".into(),
        };
        let lines = diff(&root, &change);
        assert_eq!(hunk_count(&lines), 2);
        apply_hunk(&root, &change, 0, true).expect("stage first hunk");
        let staged = Command::new("git")
            .args(["diff", "--cached", "--", "file.txt"])
            .current_dir(&root)
            .output()
            .expect("staged diff");
        assert!(String::from_utf8_lossy(&staged.stdout).contains("+ONE"));
        apply_hunk(&root, &change, 0, false).expect("unstage first hunk");
        let staged_again = Command::new("git")
            .args(["diff", "--cached", "--", "file.txt"])
            .current_dir(&root)
            .output()
            .expect("staged diff");
        assert!(String::from_utf8_lossy(&staged_again.stdout).is_empty());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn keeps_hunk_headers_with_explicit_and_implicit_counts() {
        let lines = parse_diff("@@ -4,2 +4,1 @@ fn main\n-old\n+new\n@@ -9 +8 @@\n-old2\n+new2\n");
        assert_eq!(lines[0].kind, '@');
        assert_eq!(lines[0].text, "@ -4,2 +4,1 @@ fn main");
        assert_eq!(lines[3].kind, '@');
        assert_eq!(lines[3].text, "@ -9 +8 @@");
        assert_eq!(hunk_start_line(&lines, 0), Some(4));
        assert_eq!(hunk_start_line(&lines, 1), Some(9));
        assert_eq!(hunk_start_line(&lines, 2), None);
    }

    #[test]
    fn branch_reader_is_safe_outside_git() {
        assert_eq!(current_branch(Path::new("/tmp")), None);
    }

    #[test]
    fn git_backend_exposes_vcs_neutral_status_boundary() {
        let root = std::env::temp_dir().join(format!("mtide-vcs-backend-{}", std::process::id()));
        let backend = GitBackend::new(root.clone());
        assert_eq!(backend.root(), root.as_path());
        assert!(backend.status().is_empty());
    }

    #[test]
    fn rejects_empty_commit_message() {
        assert!(commit(Path::new("/tmp"), " ").is_err());
    }

    #[test]
    fn rejects_invalid_pull_request_fetch_arguments() {
        assert!(fetch_pull_request(Path::new("/tmp"), "", 1).is_err());
        assert!(fetch_pull_request(Path::new("/tmp"), "https://github.com/a/b.git", 0).is_err());
    }

    #[test]
    fn rejects_empty_ref_diff_requests_without_spawning_git() {
        assert!(diff_refs(Path::new("/tmp"), "", "HEAD").is_empty());
        assert!(diff_refs(Path::new("/tmp"), "HEAD", "").is_empty());
    }

    #[test]
    fn validates_github_comment_target_before_reading_credentials() {
        let error = github_pull_request_api_endpoint("https://example.com/org/repo", 1)
            .expect_err("non-GitHub repository");
        assert!(error.to_string().contains("invalid GitHub"));
        let error = github_pull_request_api_endpoint("https://github.com/org/repo", 0)
            .expect_err("zero PR number");
        assert!(error.to_string().contains("invalid GitHub"));
    }

    #[test]
    fn rejects_empty_comment_fields_without_network_access() {
        let error = create_pull_request_comment(
            "https://github.com/org/repo",
            1,
            "",
            "src/lib.rs",
            1,
            "comment",
        )
        .expect_err("empty commit");
        assert!(error.to_string().contains("invalid pull request comment"));
    }

    #[test]
    fn parses_porcelain_blame() {
        let text = "abc123 1 1 1\nauthor Ada\nauthor-time 1700000000\n\tlet x = 1;\n";
        assert_eq!(
            parse_blame(text),
            vec![BlameLine {
                commit: "abc123".into(),
                author: "Ada".into(),
                date: "1700000000".into(),
                line: "let x = 1;".into(),
            }]
        );
    }

    #[test]
    fn parses_history_record() {
        assert_eq!(
            parse_history_line("abc\x1fAda\x1f2026-11-04\x1fInitial commit"),
            Some(GitCommit {
                hash: "abc".into(),
                author: "Ada".into(),
                date: "2026-11-04".into(),
                subject: "Initial commit".into(),
            })
        );
    }
}
