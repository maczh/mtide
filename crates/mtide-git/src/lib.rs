//! Git CLI integration with bounded, parseable porcelain output.

use mtide_diff::{diff_lines, diff_lines_with_options, DiffOp, LineDiff};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git executable is not available")]
    Unavailable,
    #[error("path is not inside a git repository: {0}")]
    NotRepository(PathBuf),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("git I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("commit message must not be empty")]
    EmptyCommitMessage,
    #[error("git history snapshot changed; refresh before loading another page")]
    SnapshotChanged,
    #[error("git command timed out: {0}")]
    Timeout(String),
    #[error("git command cancelled: {0}")]
    Cancelled(String),
}

#[derive(Clone, Default, Debug)]
pub struct GitCancellation(Arc<AtomicBool>);

impl GitCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Unmerged,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub kind: ChangeKind,
    pub index_code: char,
    pub worktree_code: char,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    pub remote: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSummary {
    pub id: String,
    pub author: String,
    pub subject: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashEntry {
    pub reference: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    root: PathBuf,
}

impl Repository {
    pub fn discover(path: &Path) -> Result<Self, GitError> {
        let output = run_git(path, &["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if root.as_os_str().is_empty() {
            return Err(GitError::NotRepository(path.to_path_buf()));
        }
        Ok(Self { root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn status(&self) -> Result<Vec<StatusEntry>, GitError> {
        let output = run_git(&self.root, &["status", "--porcelain=v2", "-z"])?;
        parse_status(&output.stdout)
    }
    pub fn status_with_cancel(
        &self,
        cancellation: &GitCancellation,
    ) -> Result<Vec<StatusEntry>, GitError> {
        let output = run_git_cancellable(
            &self.root,
            &["status", "--porcelain=v2", "-z"],
            cancellation,
        )?;
        parse_status(&output.stdout)
    }
    pub fn diff_file(&self, path: &Path) -> Result<LineDiff, GitError> {
        self.diff_file_with_options(path, false)
    }
    pub fn diff_file_with_options(
        &self,
        path: &Path,
        ignore_whitespace: bool,
    ) -> Result<LineDiff, GitError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| GitError::NotRepository(path.to_path_buf()))?;
        let relative = relative.to_string_lossy();
        let output = run_git(&self.root, &["show", &format!("HEAD:{relative}")])?;
        let old = String::from_utf8_lossy(&output.stdout);
        let new = std::fs::read_to_string(path)?;
        Ok(if ignore_whitespace {
            diff_lines_with_options(&old, &new, true)
        } else {
            diff_lines(&old, &new)
        })
    }
    pub fn diff_file_with_options_cancellable(
        &self,
        path: &Path,
        ignore_whitespace: bool,
        cancellation: &GitCancellation,
    ) -> Result<LineDiff, GitError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| GitError::NotRepository(path.to_path_buf()))?;
        let relative = relative.to_string_lossy();
        let output = run_git_cancellable(
            &self.root,
            &["show", &format!("HEAD:{relative}")],
            cancellation,
        )?;
        let old = String::from_utf8_lossy(&output.stdout);
        let new = std::fs::read_to_string(path)?;
        Ok(if ignore_whitespace {
            diff_lines_with_options(&old, &new, true)
        } else {
            diff_lines(&old, &new)
        })
    }

    /// Computes a worktree diff against caller-provided text. This is used by
    /// the editor diff mode so unsaved edits, rather than only on-disk bytes,
    /// are reflected in the decorations.
    pub fn diff_text_with_options_cancellable(
        &self,
        path: &Path,
        text: &str,
        ignore_whitespace: bool,
        cancellation: &GitCancellation,
    ) -> Result<LineDiff, GitError> {
        let relative = self.relative_path(path)?.to_string_lossy().into_owned();
        let output = run_git_cancellable(
            &self.root,
            &["show", &format!("HEAD:{relative}")],
            cancellation,
        )?;
        let old = String::from_utf8_lossy(&output.stdout);
        Ok(if ignore_whitespace {
            diff_lines_with_options(&old, text, true)
        } else {
            diff_lines(&old, text)
        })
    }

    pub fn head_file_text_cancellable(
        &self,
        path: &Path,
        cancellation: &GitCancellation,
    ) -> Result<String, GitError> {
        let relative = self.relative_path(path)?.to_string_lossy().into_owned();
        let output = run_git_cancellable(
            &self.root,
            &["show", &format!("HEAD:{relative}")],
            cancellation,
        )?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
    /// Computes the diff between `HEAD` and the file currently in the index.
    /// This is the source of truth for selective unstage operations.
    pub fn cached_diff_file(&self, path: &Path) -> Result<LineDiff, GitError> {
        self.cached_diff_file_with_options(path, false)
    }

    pub fn cached_diff_file_with_options(
        &self,
        path: &Path,
        ignore_whitespace: bool,
    ) -> Result<LineDiff, GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        let old = run_git(&self.root, &["show", &format!("HEAD:{relative}")])?;
        let indexed = run_git(&self.root, &["show", &format!(":{relative}")])?;
        let old = String::from_utf8_lossy(&old.stdout);
        let indexed = String::from_utf8_lossy(&indexed.stdout);
        Ok(if ignore_whitespace {
            diff_lines_with_options(&old, &indexed, true)
        } else {
            diff_lines(&old, &indexed)
        })
    }
    pub fn cached_diff_file_with_options_cancellable(
        &self,
        path: &Path,
        ignore_whitespace: bool,
        cancellation: &GitCancellation,
    ) -> Result<LineDiff, GitError> {
        let relative = self.relative_path(path)?.to_string_lossy().into_owned();
        let old = run_git_cancellable(
            &self.root,
            &["show", &format!("HEAD:{relative}")],
            cancellation,
        )?;
        let indexed =
            run_git_cancellable(&self.root, &["show", &format!(":{relative}")], cancellation)?;
        let old = String::from_utf8_lossy(&old.stdout);
        let indexed = String::from_utf8_lossy(&indexed.stdout);
        Ok(if ignore_whitespace {
            diff_lines_with_options(&old, &indexed, true)
        } else {
            diff_lines(&old, &indexed)
        })
    }
    pub fn blame_line(&self, path: &Path, line: usize) -> Result<Option<String>, GitError> {
        self.blame_line_cancellable(path, line, &GitCancellation::default())
    }

    pub fn blame_line_cancellable(
        &self,
        path: &Path,
        line: usize,
        cancellation: &GitCancellation,
    ) -> Result<Option<String>, GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        let line = line.max(1).to_string();
        let output = run_git_cancellable(
            &self.root,
            &[
                "blame",
                "--porcelain",
                "-L",
                &format!("{line},{line}"),
                "--",
                relative.as_ref(),
            ],
            cancellation,
        )?;
        let mut author = None;
        let mut summary = None;
        for record in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(value) = record.strip_prefix("author ") {
                author = Some(value.to_owned());
            } else if let Some(value) = record.strip_prefix("summary ") {
                summary = Some(value.to_owned());
            }
        }
        Ok(author
            .or_else(|| summary.clone())
            .map(|author| format!("{author} — {}", summary.unwrap_or_default())))
    }
    pub fn stage(&self, path: &Path) -> Result<(), GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        run_git(&self.root, &["add", "--", relative.as_ref()]).map(|_| ())
    }
    pub fn stage_hunks(
        &self,
        path: &Path,
        diff: &LineDiff,
        accepted: &[usize],
    ) -> Result<(), GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        let mut patch = String::new();
        patch.push_str(&format!("diff --git a/{relative} b/{relative}\n"));
        let tracked = run_git(
            &self.root,
            &["ls-files", "--error-unmatch", relative.as_ref()],
        )
        .is_ok();
        if !tracked {
            patch.push_str("new file mode 100644\n--- /dev/null\n");
        } else {
            patch.push_str(&format!("--- a/{relative}\n"));
        }
        patch.push_str(&format!("+++ b/{relative}\n"));
        let mut accepted = accepted.to_vec();
        accepted.sort_unstable();
        accepted.dedup();
        for id in accepted {
            let hunk = diff
                .hunks
                .get(id)
                .ok_or_else(|| GitError::Command(format!("unknown diff hunk: {id}")))?;
            let old_count = hunk.old_lines.len();
            let new_count = hunk.new_lines.len();
            let old_start = if old_count == 0 {
                hunk.old_range.start
            } else {
                hunk.old_range.start + 1
            };
            let new_start = if new_count == 0 {
                hunk.new_range.start
            } else {
                hunk.new_range.start + 1
            };
            patch.push_str(&format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
            ));
            for line in &hunk.old_lines {
                patch.push('-');
                patch.push_str(line);
                patch.push('\n');
            }
            for line in &hunk.new_lines {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
        }
        run_git_with_input(
            &self.root,
            &["apply", "--cached", "--unidiff-zero", "--whitespace=nowarn"],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Unstages selected cached hunks while leaving the working tree intact.
    pub fn unstage_hunks(
        &self,
        path: &Path,
        diff: &LineDiff,
        accepted: &[usize],
    ) -> Result<(), GitError> {
        let relative = self.relative_path(path)?.to_string_lossy().into_owned();
        let mut patch =
            format!("diff --git a/{relative} b/{relative}\n--- a/{relative}\n+++ b/{relative}\n");
        let mut accepted = accepted.to_vec();
        accepted.sort_unstable();
        accepted.dedup();
        for id in accepted {
            let hunk = diff
                .hunks
                .get(id)
                .ok_or_else(|| GitError::Command(format!("unknown diff hunk: {id}")))?;
            let old_count = hunk.old_lines.len();
            let new_count = hunk.new_lines.len();
            let old_start = if old_count == 0 {
                hunk.old_range.start
            } else {
                hunk.old_range.start + 1
            };
            let new_start = if new_count == 0 {
                hunk.new_range.start
            } else {
                hunk.new_range.start + 1
            };
            patch.push_str(&format!(
                "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
            ));
            for line in &hunk.old_lines {
                patch.push('-');
                patch.push_str(line);
                patch.push('\n');
            }
            for line in &hunk.new_lines {
                patch.push('+');
                patch.push_str(line);
                patch.push('\n');
            }
        }
        run_git_with_input(
            &self.root,
            &[
                "apply",
                "--cached",
                "--reverse",
                "--unidiff-zero",
                "--whitespace=nowarn",
            ],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Stages only selected non-equal operations from a line diff.
    ///
    /// The patch is built with zero context so an individual inserted or
    /// deleted line can be staged. Git still validates the exact line
    /// position; a mismatch is returned as the original Git error and never
    /// silently stages a different change.
    pub fn stage_lines(
        &self,
        path: &Path,
        diff: &LineDiff,
        accepted_operations: &[usize],
    ) -> Result<(), GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        let mut accepted = accepted_operations.to_vec();
        accepted.sort_unstable();
        accepted.dedup();
        if accepted
            .iter()
            .any(|index| diff.operations.get(*index).is_none())
        {
            return Err(GitError::Command("unknown diff operation".into()));
        }
        let tracked = run_git(
            &self.root,
            &["ls-files", "--error-unmatch", relative.as_ref()],
        )
        .is_ok();
        let mut patch = format!("diff --git a/{relative} b/{relative}\n");
        if !tracked {
            patch.push_str("new file mode 100644\n--- /dev/null\n");
        } else {
            patch.push_str(&format!("--- a/{relative}\n"));
        }
        patch.push_str(&format!("+++ b/{relative}\n"));

        let mut old_line = 0usize;
        let mut new_line = 0usize;
        let mut group = Vec::new();
        let mut group_old_start = 0usize;
        let mut group_new_start = 0usize;
        let flush_group = |group: &mut Vec<(bool, String)>,
                           old_start: usize,
                           new_start: usize,
                           patch: &mut String| {
            if group.is_empty() {
                return;
            }
            let old_count = group.iter().filter(|(is_old, _)| *is_old).count();
            let new_count = group.len() - old_count;
            let old_header = if old_count == 0 {
                old_start
            } else {
                old_start + 1
            };
            let new_header = if new_count == 0 {
                new_start
            } else {
                new_start + 1
            };
            patch.push_str(&format!(
                "@@ -{old_header},{old_count} +{new_header},{new_count} @@\n"
            ));
            for (is_old, line) in group.drain(..) {
                patch.push(if is_old { '-' } else { '+' });
                patch.push_str(&line);
                patch.push('\n');
            }
        };

        for (index, operation) in diff.operations.iter().enumerate() {
            let selected = accepted.binary_search(&index).is_ok();
            match operation {
                DiffOp::Equal(_) => {
                    flush_group(&mut group, group_old_start, group_new_start, &mut patch);
                    old_line += 1;
                    new_line += 1;
                }
                DiffOp::Delete(line) => {
                    if selected {
                        if group.is_empty() {
                            group_old_start = old_line;
                            group_new_start = new_line;
                        }
                        group.push((true, line.clone()));
                    } else {
                        flush_group(&mut group, group_old_start, group_new_start, &mut patch);
                    }
                    old_line += 1;
                }
                DiffOp::Insert(line) => {
                    if selected {
                        if group.is_empty() {
                            group_old_start = old_line;
                            group_new_start = new_line;
                        }
                        group.push((false, line.clone()));
                    } else {
                        flush_group(&mut group, group_old_start, group_new_start, &mut patch);
                    }
                    new_line += 1;
                }
            }
        }
        flush_group(&mut group, group_old_start, group_new_start, &mut patch);
        run_git_with_input(
            &self.root,
            &["apply", "--cached", "--unidiff-zero", "--whitespace=nowarn"],
            patch.as_bytes(),
        )
        .map(|_| ())
    }

    /// Unstages only the selected operations from a cached diff.
    ///
    /// The cached diff is reversed into a zero-context patch and applied to
    /// the index. The working tree is never touched, so mixed staged and
    /// unstaged edits remain intact.
    pub fn unstage_lines(
        &self,
        path: &Path,
        diff: &LineDiff,
        accepted_operations: &[usize],
    ) -> Result<(), GitError> {
        let accepted = accepted_operations
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if accepted
            .iter()
            .any(|index| diff.operations.get(*index).is_none())
        {
            return Err(GitError::Command("unknown diff operation".into()));
        }
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        let tracked = run_git(
            &self.root,
            &["ls-files", "--error-unmatch", relative.as_ref()],
        )
        .is_ok();
        let mut patch = format!("diff --git a/{relative} b/{relative}\n");
        if !tracked {
            patch.push_str("new file mode 100644\n--- /dev/null\n");
        } else {
            patch.push_str(&format!("--- a/{relative}\n"));
        }
        patch.push_str(&format!("+++ b/{relative}\n"));
        let mut old_line = 0usize;
        let mut new_line = 0usize;
        let mut group = Vec::new();
        let mut group_old_start = 0usize;
        let mut group_new_start = 0usize;
        let flush = |group: &mut Vec<(bool, String)>,
                     old_start: usize,
                     new_start: usize,
                     patch: &mut String| {
            if group.is_empty() {
                return;
            }
            let old_count = group.iter().filter(|(is_old, _)| *is_old).count();
            let new_count = group.len() - old_count;
            let old_header = if old_count == 0 {
                old_start
            } else {
                old_start + 1
            };
            let new_header = if new_count == 0 {
                new_start
            } else {
                new_start + 1
            };
            patch.push_str(&format!(
                "@@ -{old_header},{old_count} +{new_header},{new_count} @@\n"
            ));
            for (is_old, line) in group.drain(..) {
                patch.push(if is_old { '-' } else { '+' });
                patch.push_str(&line);
                patch.push('\n');
            }
        };
        for (index, operation) in diff.operations.iter().enumerate() {
            match operation {
                DiffOp::Equal(_) => {
                    flush(&mut group, group_old_start, group_new_start, &mut patch);
                    old_line += 1;
                    new_line += 1;
                }
                DiffOp::Delete(line) => {
                    if accepted.contains(&index) {
                        if group.is_empty() {
                            group_old_start = old_line;
                            group_new_start = new_line;
                        }
                        group.push((true, line.clone()));
                    } else {
                        flush(&mut group, group_old_start, group_new_start, &mut patch);
                    }
                    old_line += 1;
                }
                DiffOp::Insert(line) => {
                    if accepted.contains(&index) {
                        if group.is_empty() {
                            group_old_start = old_line;
                            group_new_start = new_line;
                        }
                        group.push((false, line.clone()));
                    } else {
                        flush(&mut group, group_old_start, group_new_start, &mut patch);
                    }
                    new_line += 1;
                }
            }
        }
        flush(&mut group, group_old_start, group_new_start, &mut patch);
        run_git_with_input(
            &self.root,
            &[
                "apply",
                "--cached",
                "--reverse",
                "--unidiff-zero",
                "--whitespace=nowarn",
            ],
            patch.as_bytes(),
        )
        .map(|_| ())
    }
    pub fn unstage(&self, path: &Path) -> Result<(), GitError> {
        let relative = self.relative_path(path)?;
        let relative = relative.to_string_lossy();
        run_git(
            &self.root,
            &["reset", "-q", "HEAD", "--", relative.as_ref()],
        )
        .map(|_| ())
    }
    pub fn commit(&self, message: &str) -> Result<(), GitError> {
        self.commit_with_options(message, false)
    }
    pub fn commit_with_options(&self, message: &str, amend: bool) -> Result<(), GitError> {
        if message.trim().is_empty() {
            return Err(GitError::EmptyCommitMessage);
        }
        if amend {
            run_git(&self.root, &["commit", "--amend", "-m", message]).map(|_| ())
        } else {
            run_git(&self.root, &["commit", "-m", message]).map(|_| ())
        }
    }
    pub fn fetch(&self) -> Result<String, GitError> {
        self.fetch_cancellable(&GitCancellation::default())
    }

    pub fn fetch_cancellable(&self, cancellation: &GitCancellation) -> Result<String, GitError> {
        run_git_cancellable(&self.root, &["fetch", "--all", "--prune"], cancellation)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    pub fn pull(&self, rebase: bool) -> Result<String, GitError> {
        self.pull_cancellable(rebase, &GitCancellation::default())
    }

    pub fn pull_cancellable(
        &self,
        rebase: bool,
        cancellation: &GitCancellation,
    ) -> Result<String, GitError> {
        let args = if rebase {
            vec!["pull", "--rebase"]
        } else {
            vec!["pull", "--no-rebase"]
        };
        run_git_cancellable(&self.root, &args, cancellation)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    pub fn push(&self) -> Result<String, GitError> {
        self.push_cancellable(&GitCancellation::default())
    }

    pub fn push_cancellable(&self, cancellation: &GitCancellation) -> Result<String, GitError> {
        run_git_cancellable(&self.root, &["push"], cancellation)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    pub fn sync(&self, rebase: bool) -> Result<String, GitError> {
        self.sync_cancellable(rebase, &GitCancellation::default())
    }

    pub fn sync_cancellable(
        &self,
        rebase: bool,
        cancellation: &GitCancellation,
    ) -> Result<String, GitError> {
        self.pull_cancellable(rebase, cancellation)?;
        self.push_cancellable(cancellation)
    }
    pub fn branches(&self) -> Result<Vec<Branch>, GitError> {
        self.branches_cancellable(&GitCancellation::default())
    }

    pub fn branches_cancellable(
        &self,
        cancellation: &GitCancellation,
    ) -> Result<Vec<Branch>, GitError> {
        let output = run_git_cancellable(
            &self.root,
            &[
                "branch",
                "--all",
                "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)",
            ],
            cancellation,
        )?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        let mut branches = Vec::new();
        for chunk in fields.chunks(3) {
            if chunk.len() < 2 || chunk[1].is_empty() {
                continue;
            }
            let name = String::from_utf8_lossy(chunk[1]).into_owned();
            let remote = name == "HEAD" || name.starts_with("remotes/");
            let name = name.strip_prefix("remotes/").unwrap_or(&name).to_owned();
            branches.push(Branch {
                current: chunk[0] == b"*",
                remote,
                name,
            });
        }
        Ok(branches)
    }
    pub fn checkout_branch(&self, name: &str) -> Result<(), GitError> {
        run_git(&self.root, &["checkout", "--quiet", name]).map(|_| ())
    }
    pub fn create_branch(&self, name: &str, start_point: Option<&str>) -> Result<(), GitError> {
        match start_point {
            Some(start) => run_git(&self.root, &["branch", name, start]).map(|_| ()),
            None => run_git(&self.root, &["branch", name]).map(|_| ()),
        }
    }
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<(), GitError> {
        let flag = if force { "-D" } else { "-d" };
        run_git(&self.root, &["branch", flag, name]).map(|_| ())
    }
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<(), GitError> {
        run_git(&self.root, &["branch", "-m", old, new]).map(|_| ())
    }
    pub fn merge(&self, branch: &str) -> Result<(), GitError> {
        self.merge_cancellable(branch, &GitCancellation::default())
    }

    pub fn merge_cancellable(
        &self,
        branch: &str,
        cancellation: &GitCancellation,
    ) -> Result<(), GitError> {
        run_git_cancellable(&self.root, &["merge", "--no-edit", branch], cancellation).map(|_| ())
    }
    pub fn rebase(&self, onto: &str) -> Result<(), GitError> {
        self.rebase_cancellable(onto, &GitCancellation::default())
    }

    pub fn rebase_cancellable(
        &self,
        onto: &str,
        cancellation: &GitCancellation,
    ) -> Result<(), GitError> {
        run_git_cancellable(&self.root, &["rebase", onto], cancellation).map(|_| ())
    }
    pub fn stash_push(&self, message: &str, include_untracked: bool) -> Result<(), GitError> {
        let mut args = vec!["stash", "push"];
        if include_untracked {
            args.push("--include-untracked");
        }
        if !message.trim().is_empty() {
            args.extend(["-m", message]);
        }
        run_git(&self.root, &args).map(|_| ())
    }
    pub fn stashes(&self) -> Result<Vec<StashEntry>, GitError> {
        self.stashes_cancellable(&GitCancellation::default())
    }

    pub fn stashes_cancellable(
        &self,
        cancellation: &GitCancellation,
    ) -> Result<Vec<StashEntry>, GitError> {
        let output = run_git_cancellable(
            &self.root,
            &["stash", "list", "--format=%gd%x00%gs%x00"],
            cancellation,
        )?;
        let fields = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        Ok(fields
            .chunks(2)
            .filter(|chunk| chunk.len() == 2)
            .map(|chunk| StashEntry {
                reference: String::from_utf8_lossy(chunk[0]).into_owned(),
                message: String::from_utf8_lossy(chunk[1]).into_owned(),
            })
            .collect())
    }
    pub fn stash_detail(&self, reference: &str) -> Result<String, GitError> {
        self.stash_detail_cancellable(reference, &GitCancellation::default())
    }

    pub fn stash_detail_cancellable(
        &self,
        reference: &str,
        cancellation: &GitCancellation,
    ) -> Result<String, GitError> {
        run_git_cancellable(
            &self.root,
            &["stash", "show", "--stat", "--patch", reference],
            cancellation,
        )
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    }
    pub fn stash_apply(&self, reference: &str) -> Result<(), GitError> {
        run_git(&self.root, &["stash", "apply", reference]).map(|_| ())
    }
    pub fn stash_pop(&self, reference: &str) -> Result<(), GitError> {
        run_git(&self.root, &["stash", "pop", reference]).map(|_| ())
    }
    pub fn stash_drop(&self, reference: &str) -> Result<(), GitError> {
        run_git(&self.root, &["stash", "drop", reference]).map(|_| ())
    }
    pub fn history(&self, skip: usize, limit: usize) -> Result<Vec<CommitSummary>, GitError> {
        let snapshot = self.head_id()?;
        self.history_page(&snapshot, skip, limit)
    }
    pub fn head_id(&self) -> Result<String, GitError> {
        let output = run_git(&self.root, &["rev-parse", "HEAD"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    pub fn history_page(
        &self,
        snapshot: &str,
        skip: usize,
        limit: usize,
    ) -> Result<Vec<CommitSummary>, GitError> {
        self.history_page_filtered(snapshot, skip, limit, None, None, None)
    }
    pub fn history_page_filtered(
        &self,
        snapshot: &str,
        skip: usize,
        limit: usize,
        path: Option<&Path>,
        author: Option<&str>,
        keyword: Option<&str>,
    ) -> Result<Vec<CommitSummary>, GitError> {
        self.history_page_filtered_with_cancellation(
            snapshot, skip, limit, path, author, keyword, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn history_page_filtered_cancellable(
        &self,
        snapshot: &str,
        skip: usize,
        limit: usize,
        path: Option<&Path>,
        author: Option<&str>,
        keyword: Option<&str>,
        cancellation: &GitCancellation,
    ) -> Result<Vec<CommitSummary>, GitError> {
        self.history_page_filtered_with_cancellation(
            snapshot,
            skip,
            limit,
            path,
            author,
            keyword,
            Some(cancellation),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn history_page_filtered_with_cancellation(
        &self,
        snapshot: &str,
        skip: usize,
        limit: usize,
        path: Option<&Path>,
        author: Option<&str>,
        keyword: Option<&str>,
        cancellation: Option<&GitCancellation>,
    ) -> Result<Vec<CommitSummary>, GitError> {
        let head = match cancellation {
            Some(cancellation) => {
                let output = run_git_cancellable(&self.root, &["rev-parse", "HEAD"], cancellation)?;
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            }
            None => self.head_id()?,
        };
        if head != snapshot {
            return Err(GitError::SnapshotChanged);
        }
        let limit = limit.clamp(1, 1000).to_string();
        let skip = skip.to_string();
        let format = "--format=%H%x00%an%x00%s%x00";
        let author_arg = author
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("--author={}", value.trim()));
        let keyword_arg = keyword
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("--grep={}", value.trim()));
        let path_arg = path.map(|value| value.to_string_lossy().into_owned());
        let mut args = vec![
            "log".to_owned(),
            "--no-decorate".to_owned(),
            format.to_owned(),
            snapshot.to_owned(),
            "--skip".to_owned(),
            skip,
            "-n".to_owned(),
            limit,
        ];
        if let Some(author) = author_arg {
            args.push(author);
        }
        if let Some(keyword) = keyword_arg {
            args.push(keyword);
        }
        if let Some(path) = path_arg {
            args.extend(["--".to_owned(), path]);
        }
        let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = match cancellation {
            Some(cancellation) => run_git_cancellable(&self.root, &borrowed, cancellation)?,
            None => run_git(&self.root, &borrowed)?,
        };
        let fields = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        Ok(fields
            .chunks(3)
            .filter(|chunk| chunk.len() == 3)
            .map(|chunk| CommitSummary {
                id: String::from_utf8_lossy(chunk[0]).into_owned(),
                author: String::from_utf8_lossy(chunk[1]).into_owned(),
                subject: String::from_utf8_lossy(chunk[2]).into_owned(),
            })
            .collect())
    }
    pub fn commit_detail(&self, id: &str) -> Result<String, GitError> {
        self.commit_detail_cancellable(id, &GitCancellation::default())
    }

    pub fn commit_detail_cancellable(
        &self,
        id: &str,
        cancellation: &GitCancellation,
    ) -> Result<String, GitError> {
        let output = run_git_cancellable(
            &self.root,
            &[
                "show",
                "--no-ext-diff",
                "--format=fuller",
                "--stat",
                "--patch",
                "--find-renames",
                id,
            ],
            cancellation,
        )?;
        let mut detail = String::from_utf8_lossy(&output.stdout).into_owned();
        const MAX_DETAIL_BYTES: usize = 512 * 1024;
        if detail.len() > MAX_DETAIL_BYTES {
            detail.truncate(MAX_DETAIL_BYTES);
            detail.push_str("\n[commit detail truncated]\n");
        }
        Ok(detail)
    }
    fn relative_path<'a>(&self, path: &'a Path) -> Result<&'a Path, GitError> {
        path.strip_prefix(&self.root)
            .map_err(|_| GitError::NotRepository(path.to_path_buf()))
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    let output = run_command_with_timeout(&mut command, GIT_COMMAND_TIMEOUT, args)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if args.first() == Some(&"rev-parse") {
            return Err(GitError::NotRepository(cwd.to_path_buf()));
        }
        return Err(GitError::Command(message));
    }
    Ok(output)
}

fn run_git_cancellable(
    cwd: &Path,
    args: &[&str],
    cancellation: &GitCancellation,
) -> Result<std::process::Output, GitError> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    let output = run_command_with_input_timeout_and_cancel(
        &mut command,
        &[],
        GIT_COMMAND_TIMEOUT,
        args,
        Some(cancellation),
    )?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output)
}

fn run_git_with_input(
    cwd: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<std::process::Output, GitError> {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args).stdin(Stdio::piped());
    let output = run_command_with_input_timeout(&mut command, input, GIT_COMMAND_TIMEOUT, args)?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output)
}

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
    args: &[&str],
) -> Result<Output, GitError> {
    run_command_with_input_timeout(command, &[], timeout, args)
}

fn run_command_with_input_timeout(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
    args: &[&str],
) -> Result<Output, GitError> {
    run_command_with_input_timeout_and_cancel(command, input, timeout, args, None)
}

fn run_command_with_input_timeout_and_cancel(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
    args: &[&str],
    cancellation: Option<&GitCancellation>,
) -> Result<Output, GitError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            GitError::Unavailable
        } else {
            GitError::Io(error)
        }
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "git stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "git stderr unavailable"))?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));
    let input_writer = if input.is_empty() {
        None
    } else {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "git stdin unavailable"))?;
        let bytes = input.to_vec();
        Some(thread::spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&bytes)
        }))
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if cancellation.is_some_and(GitCancellation::is_cancelled) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                if let Some(writer) = input_writer {
                    let _ = writer.join();
                }
                return Err(GitError::Cancelled(args.join(" ")));
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                if let Some(writer) = input_writer {
                    let _ = writer.join();
                }
                return Err(GitError::Timeout(args.join(" ")));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    if let Some(writer) = input_writer {
        writer
            .join()
            .map_err(|_| io::Error::other("git stdin writer panicked"))??;
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("git stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("git stderr reader panicked"))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe<R: Read>(mut reader: R) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn parse_status(bytes: &[u8]) -> Result<Vec<StatusEntry>, GitError> {
    let mut entries = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let text = String::from_utf8_lossy(record);
        let mut fields = text.split(' ');
        let tag = fields.next().unwrap_or_default();
        match tag {
            "1" => {
                let xy = fields.next().unwrap_or("  ");
                let _submodule = fields.next();
                for _ in 0..5 {
                    let _ = fields.next();
                }
                let path = fields.collect::<Vec<_>>().join(" ");
                entries.push(StatusEntry {
                    path: PathBuf::from(path),
                    original_path: None,
                    kind: kind_for_code(
                        xy.chars().next().unwrap_or(' '),
                        xy.chars().nth(1).unwrap_or(' '),
                    ),
                    index_code: xy.chars().next().unwrap_or(' '),
                    worktree_code: xy.chars().nth(1).unwrap_or(' '),
                });
            }
            "2" => {
                let xy = fields.next().unwrap_or("  ");
                let _submodule = fields.next();
                for _ in 0..5 {
                    let _ = fields.next();
                }
                let _score = fields.next();
                let path = fields.collect::<Vec<_>>().join(" ");
                // Rename records carry the old path in the next NUL record; the
                // parser keeps the new path and leaves association to the caller.
                entries.push(StatusEntry {
                    path: PathBuf::from(path),
                    original_path: None,
                    kind: ChangeKind::Renamed,
                    index_code: xy.chars().next().unwrap_or(' '),
                    worktree_code: xy.chars().nth(1).unwrap_or(' '),
                });
            }
            "u" => {
                let xy = fields.next().unwrap_or("UU");
                let _ = fields.next();
                for _ in 0..6 {
                    let _ = fields.next();
                }
                let path = fields.collect::<Vec<_>>().join(" ");
                entries.push(StatusEntry {
                    path: PathBuf::from(path),
                    original_path: None,
                    kind: ChangeKind::Unmerged,
                    index_code: xy.chars().next().unwrap_or('U'),
                    worktree_code: xy.chars().nth(1).unwrap_or('U'),
                });
            }
            "?" => entries.push(StatusEntry {
                path: PathBuf::from(fields.collect::<Vec<_>>().join(" ")),
                original_path: None,
                kind: ChangeKind::Untracked,
                index_code: '?',
                worktree_code: '?',
            }),
            "!" => {}
            _ => {}
        }
    }
    Ok(entries)
}

fn kind_for_code(index: char, worktree: char) -> ChangeKind {
    let code = if index != ' ' && index != '.' {
        index
    } else {
        worktree
    };
    match code {
        'A' => ChangeKind::Added,
        'M' => ChangeKind::Modified,
        'D' => ChangeKind::Deleted,
        'R' => ChangeKind::Renamed,
        'C' => ChangeKind::Copied,
        _ => ChangeKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_status, run_command_with_input_timeout_and_cancel, run_command_with_timeout,
        ChangeKind, GitCancellation, GitError, Repository, StatusEntry,
    };
    use mtide_diff::DiffOp;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    #[cfg(unix)]
    #[test]
    fn fr_gt12_git_command_timeout_kills_child_and_returns_error() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 1"]);
        let started = std::time::Instant::now();
        let result = run_command_with_timeout(
            &mut command,
            std::time::Duration::from_millis(50),
            &["sleep-test"],
        );
        assert!(matches!(result, Err(GitError::Timeout(command)) if command == "sleep-test"));
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn fr_gt12_git_command_cancellation_kills_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 1"]);
        let cancellation = GitCancellation::default();
        let signal = cancellation.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            signal.cancel();
        });
        let result = run_command_with_input_timeout_and_cancel(
            &mut command,
            &[],
            std::time::Duration::from_secs(2),
            &["sleep-cancel-test"],
            Some(&cancellation),
        );
        assert!(
            matches!(result, Err(GitError::Cancelled(command)) if command == "sleep-cancel-test")
        );
    }

    #[test]
    fn fr_gt02_porcelain_v2_parser_handles_staged_and_untracked_records() {
        let bytes = b"1 .M N... 100644 100644 100644 abc def src/main.rs\0? scratch.txt\0";
        assert_eq!(
            parse_status(bytes).expect("parse status"),
            vec![
                StatusEntry {
                    path: PathBuf::from("src/main.rs"),
                    original_path: None,
                    kind: ChangeKind::Modified,
                    index_code: '.',
                    worktree_code: 'M'
                },
                StatusEntry {
                    path: PathBuf::from("scratch.txt"),
                    original_path: None,
                    kind: ChangeKind::Untracked,
                    index_code: '?',
                    worktree_code: '?'
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn fr_gt01_repository_discovers_root_and_status() {
        let root = std::env::temp_dir().join(format!("mtide-git-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create repo");
        Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .status()
            .expect("init git");
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "mtide-test"),
        ] {
            Command::new("git")
                .current_dir(&root)
                .args(["config", key, value])
                .status()
                .expect("configure git");
        }
        fs::write(root.join("main.rs"), "a\nb\n").expect("write tracked file");
        Command::new("git")
            .current_dir(&root)
            .args(["add", "main.rs"])
            .status()
            .expect("stage file");
        Command::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("commit file");
        fs::write(root.join("scratch.txt"), "untracked").expect("write file");
        let repository = Repository::discover(&root).expect("discover repo");
        assert_eq!(repository.root(), root.as_path());
        let blame = repository
            .blame_line(&root.join("main.rs"), 1)
            .expect("blame line")
            .expect("tracked line blame");
        assert!(blame.contains("mtide-test"));
        assert!(blame.contains("initial"));
        assert_eq!(
            repository.status().expect("status")[0].kind,
            ChangeKind::Untracked
        );
        fs::write(root.join("main.rs"), "a\nB\n").expect("modify tracked file");
        assert_eq!(
            repository
                .diff_file(&root.join("main.rs"))
                .expect("diff")
                .hunks
                .len(),
            1
        );
        repository
            .stage(&root.join("main.rs"))
            .expect("stage change");
        assert_eq!(
            repository.status().expect("staged status")[0].index_code,
            'M'
        );
        repository
            .unstage(&root.join("main.rs"))
            .expect("unstage change");
        assert_eq!(
            repository.status().expect("unstaged status")[0].worktree_code,
            'M'
        );
        assert!(matches!(
            repository.commit(" "),
            Err(super::GitError::EmptyCommitMessage)
        ));
        let branches = repository.branches().expect("branches");
        assert!(branches
            .iter()
            .any(|branch| branch.current && branch.name == "master" || branch.name == "main"));
        let history = repository.history(0, 10).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].subject, "initial");
        let detail = repository
            .commit_detail(&history[0].id)
            .expect("commit detail");
        assert!(detail.contains("main.rs"));
        let snapshot = repository.head_id().expect("head snapshot");
        assert_eq!(
            repository
                .history_page(&snapshot, 0, 10)
                .expect("history page")
                .len(),
            1
        );
        assert_eq!(
            repository
                .history_page_filtered(
                    &snapshot,
                    0,
                    10,
                    Some(Path::new("main.rs")),
                    Some("mtide-test"),
                    Some("initial"),
                )
                .expect("filtered history")
                .len(),
            1
        );
        repository
            .stage(&root.join("main.rs"))
            .expect("stage second change");
        repository.commit("second").expect("second commit");
        assert!(matches!(
            repository.history_page(&snapshot, 1, 10),
            Err(super::GitError::SnapshotChanged)
        ));
        repository
            .commit_with_options("amended", true)
            .expect("amend commit");
        assert_eq!(
            repository.history(0, 1).expect("amended history")[0].subject,
            "amended"
        );
        let base_branch = repository
            .branches()
            .expect("base branches")
            .into_iter()
            .find(|branch| branch.current)
            .expect("current branch")
            .name;
        repository
            .create_branch("feature", None)
            .expect("create branch");
        repository
            .checkout_branch("feature")
            .expect("checkout feature");
        repository
            .rename_branch("feature", "renamed-feature")
            .expect("rename branch");
        repository
            .checkout_branch(&base_branch)
            .expect("checkout base");
        repository
            .delete_branch("renamed-feature", false)
            .expect("delete branch");
        assert!(!repository
            .branches()
            .expect("final branches")
            .iter()
            .any(|branch| branch.name == "renamed-feature"));
        repository
            .create_branch("merge-feature", None)
            .expect("create merge branch");
        repository
            .checkout_branch("merge-feature")
            .expect("checkout merge branch");
        fs::write(root.join("feature.txt"), "feature\n").expect("write feature");
        repository
            .stage(&root.join("feature.txt"))
            .expect("stage feature");
        repository.commit("feature").expect("commit feature");
        repository
            .checkout_branch(&base_branch)
            .expect("return to base");
        repository.merge("merge-feature").expect("merge feature");
        assert!(root.join("feature.txt").is_file());
        repository
            .delete_branch("merge-feature", false)
            .expect("delete merge branch");
        fs::write(root.join("main.rs"), "stashed change\n").expect("write stash change");
        repository
            .stash_push("work in progress", false)
            .expect("create stash");
        let stashes = repository.stashes().expect("list stashes");
        assert_eq!(stashes.len(), 1);
        assert!(stashes[0].message.contains("work in progress"));
        repository.stash_apply("stash@{0}").expect("apply stash");
        assert_eq!(
            repository.status().expect("status after stash apply")[0].worktree_code,
            'M'
        );
        let stash_detail = repository.stash_detail("stash@{0}").expect("stash detail");
        assert!(stash_detail.contains("main.rs"));
        repository.stash_drop("stash@{0}").expect("drop stash");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_gt03_stage_hunks_only_stages_selected_changes() {
        let root = std::env::temp_dir().join(format!("mtide-git-hunks-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create repo");
        Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .status()
            .expect("init git");
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "mtide-test"),
        ] {
            Command::new("git")
                .current_dir(&root)
                .args(["config", key, value])
                .status()
                .expect("configure git");
        }
        let file = root.join("main.rs");
        fs::write(&file, "a\nb\nc\nd\n").expect("write initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["add", "main.rs"])
            .status()
            .expect("stage initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("commit initial file");
        fs::write(&file, "A\nb\nc\nD\n").expect("write two changes");
        let repository = Repository::discover(&root).expect("discover repo");
        let diff = repository.diff_file(&file).expect("calculate diff");
        assert_eq!(diff.hunks.len(), 2);
        repository
            .stage_hunks(&file, &diff, &[1])
            .expect("stage second hunk");
        let staged = Command::new("git")
            .current_dir(&root)
            .args(["show", ":main.rs"])
            .output()
            .expect("read staged file");
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "a\nb\nc\nD\n");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_gt03_stage_lines_only_stages_selected_replacement() {
        let root = std::env::temp_dir().join(format!("mtide-git-lines-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create repo");
        Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .status()
            .expect("init git");
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "mtide-test"),
        ] {
            Command::new("git")
                .current_dir(&root)
                .args(["config", key, value])
                .status()
                .expect("configure git");
        }
        let file = root.join("main.rs");
        fs::write(&file, "a\nb\nc\nd\n").expect("write initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["add", "main.rs"])
            .status()
            .expect("stage initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("commit initial file");
        fs::write(&file, "A\nb\nc\nD\n").expect("write changes");
        let repository = Repository::discover(&root).expect("discover repo");
        let diff = repository.diff_file(&file).expect("calculate diff");
        let selected = diff
            .operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| {
                matches!(operation, DiffOp::Delete(line) if line == "d")
                    .then_some(index)
                    .or_else(|| {
                        matches!(operation, DiffOp::Insert(line) if line == "D").then_some(index)
                    })
            })
            .collect::<Vec<_>>();
        repository
            .stage_lines(&file, &diff, &selected)
            .expect("stage selected lines");
        let staged = Command::new("git")
            .current_dir(&root)
            .args(["show", ":main.rs"])
            .output()
            .expect("read staged file");
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "a\nb\nc\nD\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_gt03_unstage_lines_only_unstages_selected_replacement() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-unstage-lines-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create repo");
        Command::new("git")
            .current_dir(&root)
            .args(["init", "-q"])
            .status()
            .expect("init git");
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "mtide-test"),
        ] {
            Command::new("git")
                .current_dir(&root)
                .args(["config", key, value])
                .status()
                .expect("configure git");
        }
        let file = root.join("main.rs");
        fs::write(&file, "a\nb\nc\nd\n").expect("write initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["add", "main.rs"])
            .status()
            .expect("stage initial file");
        Command::new("git")
            .current_dir(&root)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("commit initial file");
        fs::write(&file, "a\nb\nc\nD\n").expect("write staged change");
        Command::new("git")
            .current_dir(&root)
            .args(["add", "main.rs"])
            .status()
            .expect("stage change");
        let repository = Repository::discover(&root).expect("discover repo");
        let diff = repository
            .cached_diff_file(&file)
            .expect("calculate cached diff");
        let selected = diff
            .operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| {
                matches!(operation, DiffOp::Delete(line) if line == "d")
                    .then_some(index)
                    .or_else(|| {
                        matches!(operation, DiffOp::Insert(line) if line == "D").then_some(index)
                    })
            })
            .collect::<Vec<_>>();
        repository
            .unstage_lines(&file, &diff, &selected)
            .expect("unstage selected lines");
        let staged = Command::new("git")
            .current_dir(&root)
            .args(["show", ":main.rs"])
            .output()
            .expect("read staged file");
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "a\nb\nc\nd\n");
        assert_eq!(
            fs::read_to_string(&file).expect("read worktree"),
            "a\nb\nc\nD\n"
        );
        let _ = fs::remove_dir_all(root);
    }
}
