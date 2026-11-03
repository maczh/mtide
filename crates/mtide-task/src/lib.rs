//! Non-blocking build/task execution and lightweight problem matching.

use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskContext {
    pub workspace_folder: Option<PathBuf>,
    pub file: Option<PathBuf>,
    pub selected_text: String,
    pub line: Option<usize>,
    pub environment: BTreeMap<String, String>,
}

impl TaskSpec {
    /// Expands the documented task variables without invoking a shell.
    pub fn expand(&self, context: &TaskContext) -> Self {
        Self {
            label: expand_variables(&self.label, context),
            program: expand_variables(&self.program, context),
            args: self
                .args
                .iter()
                .map(|argument| expand_variables(argument, context))
                .collect(),
            cwd: self
                .cwd
                .as_ref()
                .map(|cwd| PathBuf::from(expand_variables(&cwd.to_string_lossy(), context))),
            environment: self
                .environment
                .iter()
                .map(|(name, value)| (name.clone(), expand_variables(value, context)))
                .collect(),
        }
    }
}

pub fn expand_variables(input: &str, context: &TaskContext) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative_start) = input[cursor..].find("${") {
        let start = cursor + relative_start;
        output.push_str(&input[cursor..start]);
        let Some(relative_end) = input[start + 2..].find('}') else {
            output.push_str(&input[start..]);
            cursor = input.len();
            break;
        };
        let end = start + 2 + relative_end;
        let variable = &input[start + 2..end];
        if let Some(value) = resolve_variable(variable, context) {
            output.push_str(&value);
        } else {
            output.push_str(&input[start..=end]);
        }
        cursor = end + 1;
    }
    if cursor < input.len() {
        output.push_str(&input[cursor..]);
    }
    output
}

fn resolve_variable(variable: &str, context: &TaskContext) -> Option<String> {
    match variable {
        "workspaceFolder" => context
            .workspace_folder
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        "file" => context
            .file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        "fileDirname" => context
            .file
            .as_ref()
            .and_then(|path| path.parent())
            .map(|path| path.to_string_lossy().into_owned()),
        "fileBasenameNoExtension" => context.file.as_ref().and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        }),
        "selectedText" => Some(context.selected_text.clone()),
        "line" => context.line.map(|line| line.to_string()),
        name => name
            .strip_prefix("env:")
            .and_then(|key| context.environment.get(key))
            .cloned(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    pub file: PathBuf,
    pub line: usize,
    pub column: Option<usize>,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultStatus {
    Running,
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultNode {
    pub name: String,
    pub status: ResultStatus,
    pub children: Vec<ResultNode>,
}

impl ResultNode {
    pub fn root(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ResultStatus::Running,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum TaskError {
    #[error("task I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("task is not running")]
    NotRunning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskState {
    Running,
    Finished(ExitStatusCode),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitStatusCode(pub Option<i32>);

struct OutputChunk {
    bytes: Vec<u8>,
    eof: bool,
}

const DEFAULT_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

pub struct TaskRun {
    spec: TaskSpec,
    child: Child,
    output: Receiver<OutputChunk>,
    output_text: String,
    problems: Vec<Problem>,
    problem_buffer: String,
    /// Bytes of an incomplete UTF-8 sequence held over for the next chunk.
    pending_bytes: Vec<u8>,
    state: TaskState,
    max_output_bytes: usize,
    readers_closed: usize,
    pending_status: Option<ExitStatusCode>,
    started_at: Instant,
    timeout: Duration,
    timed_out: bool,
}

impl std::fmt::Debug for TaskRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskRun")
            .field("label", &self.spec.label)
            .field("state", &self.state)
            .field("output_bytes", &self.output_text.len())
            .field("problems", &self.problems.len())
            .finish()
    }
}

impl TaskRun {
    pub fn start(spec: TaskSpec) -> Result<Self, TaskError> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.envs(&spec.environment);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: the closure only performs async-signal-safe session setup
            // before exec and does not touch shared Rust state.
            unsafe {
                command.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(io::Error::other));
            }
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "task stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "task stderr unavailable"))?;
        let (sender, output) = mpsc::sync_channel(256);
        spawn_reader(stdout, sender.clone());
        spawn_reader(stderr, sender);
        Ok(Self {
            spec,
            child,
            output,
            output_text: String::new(),
            problems: Vec::new(),
            problem_buffer: String::new(),
            pending_bytes: Vec::new(),
            state: TaskState::Running,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            readers_closed: 0,
            pending_status: None,
            started_at: Instant::now(),
            timeout: DEFAULT_TIMEOUT,
            timed_out: false,
        })
    }

    pub fn label(&self) -> &str {
        &self.spec.label
    }
    pub fn cwd(&self) -> Option<&Path> {
        self.spec.cwd.as_deref()
    }
    pub fn state(&self) -> &TaskState {
        &self.state
    }
    pub fn output(&self) -> &str {
        &self.output_text
    }
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }
    pub fn set_output_limit(&mut self, limit: usize) {
        self.max_output_bytes = limit.max(1);
        trim_output(&mut self.output_text, self.max_output_bytes);
    }
    /// Sets the maximum wall-clock time for the task. A zero duration is
    /// accepted for tests and causes the next non-blocking poll to time out.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }
    pub fn result_tree(&self) -> ResultNode {
        parse_result_tree(&self.output_text, &self.state)
    }

    /// Drains output and performs no blocking I/O.
    pub fn poll(&mut self) -> Result<usize, TaskError> {
        let mut bytes = 0;
        while let Ok(chunk) = self.output.try_recv() {
            if chunk.eof {
                self.readers_closed = self.readers_closed.saturating_add(1);
                continue;
            }
            bytes += chunk.bytes.len();
            let text = decode_chunk(&mut self.pending_bytes, &chunk.bytes);
            self.output_text.push_str(&text);
            trim_output(&mut self.output_text, self.max_output_bytes);
            collect_problem_lines(&mut self.problem_buffer, &mut self.problems, &text, false);
        }
        if matches!(self.state, TaskState::Running) {
            if self.started_at.elapsed() >= self.timeout {
                terminate_child(&mut self.child)?;
                let _ = self.child.wait();
                self.timed_out = true;
                self.state = TaskState::Cancelled;
                return Ok(bytes);
            }
            if self.pending_status.is_none() {
                if let Some(status) = self.child.try_wait()? {
                    self.pending_status = Some(ExitStatusCode(status.code()));
                }
            }
            if let Some(status) = self
                .pending_status
                .take()
                .filter(|_| self.readers_closed >= 2)
            {
                if !self.problem_buffer.is_empty() {
                    collect_problem_lines(&mut self.problem_buffer, &mut self.problems, "", true);
                }
                self.state = TaskState::Finished(status);
            }
        }
        Ok(bytes)
    }

    pub fn cancel(&mut self) -> Result<(), TaskError> {
        if !matches!(self.state, TaskState::Running) {
            return Ok(());
        }
        terminate_child(&mut self.child)?;
        let _ = self.child.wait();
        self.state = TaskState::Cancelled;
        Ok(())
    }
}

/// Decodes a streamed chunk, holding back an incomplete trailing UTF-8
/// sequence so multi-byte output split across reads is decoded correctly.
fn decode_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    let mut buffer = std::mem::take(pending);
    buffer.extend_from_slice(chunk);
    let mut text = String::new();
    let mut rest = buffer.as_slice();
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                text.push_str(std::str::from_utf8(valid).expect("valid up to error"));
                match error.error_len() {
                    Some(length) => rest = &after[length..],
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    *pending = rest.to_vec();
    text
}

fn trim_output(output: &mut String, max_bytes: usize) {
    if output.len() <= max_bytes {
        return;
    }
    let excess = output.len() - max_bytes;
    let boundary = output
        .char_indices()
        .find_map(|(index, _)| (index >= excess).then_some(index))
        .unwrap_or(output.len());
    output.drain(..boundary);
}

fn collect_problem_lines(
    buffer: &mut String,
    problems: &mut Vec<Problem>,
    chunk: &str,
    flush: bool,
) {
    buffer.push_str(chunk);
    while let Some(newline) = buffer.find('\n') {
        let line = buffer[..newline].trim_end_matches('\r');
        problems.extend(parse_problems(line));
        buffer.drain(..=newline);
    }
    if flush && !buffer.is_empty() {
        problems.extend(parse_problems(buffer));
        buffer.clear();
    }
}

impl Drop for TaskRun {
    fn drop(&mut self) {
        if matches!(self.state, TaskState::Running) {
            let _ = terminate_child(&mut self.child);
            let _ = self.child.wait();
        }
    }
}

fn terminate_child(child: &mut Child) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{killpg, Signal},
            unistd::Pid,
        };
        let pid = Pid::from_raw(child.id().try_into().unwrap_or(i32::MAX));
        if killpg(pid, Signal::SIGKILL).is_ok() {
            return Ok(());
        }
    }
    child.kill()
}

pub fn parse_problems(text: &str) -> Vec<Problem> {
    text.lines().filter_map(parse_problem_line).collect()
}

pub fn parse_result_tree(text: &str, state: &TaskState) -> ResultNode {
    let mut root = ResultNode::root("task");
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("test ") else {
            continue;
        };
        let (name, status) = if let Some(name) = rest.strip_suffix(" ... ok") {
            (name, ResultStatus::Passed)
        } else if let Some(name) = rest.strip_suffix(" ... FAILED") {
            (name, ResultStatus::Failed)
        } else if let Some(name) = rest.strip_suffix(" ... ignored") {
            (name, ResultStatus::Skipped)
        } else {
            continue;
        };
        root.children.push(ResultNode {
            name: name.to_owned(),
            status,
            children: Vec::new(),
        });
    }
    root.status = if root
        .children
        .iter()
        .any(|node| node.status == ResultStatus::Failed)
    {
        ResultStatus::Failed
    } else if !root.children.is_empty()
        && root
            .children
            .iter()
            .all(|node| matches!(node.status, ResultStatus::Passed | ResultStatus::Skipped))
    {
        ResultStatus::Passed
    } else if matches!(state, TaskState::Finished(_)) {
        ResultStatus::Failed
    } else {
        ResultStatus::Running
    };
    root
}

fn parse_problem_line(line: &str) -> Option<Problem> {
    let trimmed = line.trim();
    if let Some(location) = trimmed.strip_prefix("-->") {
        let (file, line_number, column) = parse_location(location.trim())?;
        return Some(Problem {
            file: Path::new(file).to_path_buf(),
            line: line_number,
            column,
            severity: Severity::Error,
            message: "compiler diagnostic".into(),
        });
    }

    // TypeScript/tsc uses `file(line,column): error ...`. A non-numeric
    // parenthesis must fall through to the generic matchers below instead of
    // aborting the whole line.
    if let Some(open) = trimmed.rfind('(') {
        if let Some(close) = trimmed[open..].find("): ") {
            let close = open + close;
            let location = trimmed[open + 1..close]
                .split_once(',')
                .and_then(|(line, column)| {
                    Some((line.parse().ok()?, column.parse::<usize>().ok()?))
                });
            if let Some((line_number, column)) = location {
                let file = &trimmed[..open];
                let message = trimmed[close + 3..].trim();
                let severity = if message.starts_with("error") {
                    Severity::Error
                } else if message.starts_with("warning") {
                    Severity::Warning
                } else {
                    Severity::Info
                };
                return Some(Problem {
                    file: Path::new(file).to_path_buf(),
                    line: line_number,
                    column: Some(column),
                    severity,
                    message: message.to_owned(),
                });
            }
        }
    }

    let (prefix, severity, message) =
        if let Some((prefix, message)) = trimmed.rsplit_once(": error:") {
            (prefix, Severity::Error, message.trim())
        } else if let Some((prefix, message)) = trimmed.rsplit_once(": warning:") {
            (prefix, Severity::Warning, message.trim())
        } else if let Some((prefix, message)) = trimmed.rsplit_once(": error ") {
            (prefix, Severity::Error, message.trim())
        } else if let Some((prefix, message)) = trimmed.rsplit_once(": warning ") {
            (prefix, Severity::Warning, message.trim())
        } else if let Some((prefix, rest)) = trimmed.rsplit_once(": error") {
            let message = rest
                .find(':')
                .map(|index| rest[index + 1..].trim())
                .unwrap_or_default();
            (prefix, Severity::Error, message)
        } else if let Some((prefix, rest)) = trimmed.rsplit_once(": warning") {
            let message = rest
                .find(':')
                .map(|index| rest[index + 1..].trim())
                .unwrap_or_default();
            (prefix, Severity::Warning, message)
        } else if let Some((prefix, message)) = trimmed.rsplit_once(':') {
            let message = message.trim();
            let severity = match message
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .chars()
                .next()
            {
                Some('E' | 'F') => Severity::Error,
                Some('W') => Severity::Warning,
                _ => Severity::Info,
            };
            (prefix, severity, message)
        } else {
            return None;
        };
    let (file, line_number, column) = parse_location(prefix.trim())?;
    Some(Problem {
        file: Path::new(file).to_path_buf(),
        line: line_number,
        column,
        severity,
        message: message.to_owned(),
    })
}

fn parse_location(value: &str) -> Option<(&str, usize, Option<usize>)> {
    let location = value.rsplitn(3, ':').collect::<Vec<_>>();
    match location.as_slice() {
        [column, line, file] if column.parse::<usize>().is_ok() => Some((
            *file,
            line.parse::<usize>().ok()?,
            column.parse::<usize>().ok(),
        )),
        [line, file] => Some((*file, line.parse::<usize>().ok()?, None)),
        _ => None,
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R, sender: SyncSender<OutputChunk>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    if sender
                        .send(OutputChunk {
                            bytes: buffer[..length].to_vec(),
                            eof: false,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = sender.send(OutputChunk {
            bytes: Vec::new(),
            eof: true,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        collect_problem_lines, expand_variables, parse_problems, parse_result_tree, Problem,
        ResultStatus, Severity, TaskContext, TaskRun, TaskSpec, TaskState,
    };
    use std::{collections::BTreeMap, path::PathBuf, thread, time::Duration};

    #[test]
    fn fr_bd01_task_variables_expand_without_shell_interpolation() {
        let context = TaskContext {
            workspace_folder: Some(PathBuf::from("/workspace")),
            file: Some(PathBuf::from("/workspace/src/main.rs")),
            selected_text: "hello world".into(),
            line: Some(17),
            environment: BTreeMap::from([(String::from("PROFILE"), String::from("debug"))]),
        };
        assert_eq!(
            expand_variables(
                "${workspaceFolder}/${fileBasenameNoExtension}:${line}:${env:PROFILE} ${unknown}",
                &context
            ),
            "/workspace/main:17:debug ${unknown}"
        );
        let spec = TaskSpec {
            label: "build ${fileBasenameNoExtension}".into(),
            program: "cargo".into(),
            args: vec!["--manifest-path".into(), "${file}".into()],
            cwd: Some(PathBuf::from("${workspaceFolder}")),
            environment: BTreeMap::new(),
        };
        let expanded = spec.expand(&context);
        assert_eq!(expanded.label, "build main");
        assert_eq!(expanded.args[1], "/workspace/src/main.rs");
        assert_eq!(expanded.cwd, Some(PathBuf::from("/workspace")));
    }

    #[test]
    fn fr_bd03_problem_matcher_parses_file_line_column_and_severity() {
        assert_eq!(
            parse_problems("src/main.rs:12:4: error: missing semicolon\n"),
            vec![Problem {
                file: "src/main.rs".into(),
                line: 12,
                column: Some(4),
                severity: Severity::Error,
                message: "missing semicolon".into(),
            }]
        );
    }

    #[test]
    fn fr_bd03_problem_matcher_supports_common_tool_formats() {
        let problems = parse_problems(
            "src/lib.rs:7:3: error[E0308]: mismatched types\nsrc/app.ts(12,4): error TS2322: bad type\n--> src/main.rs:9:5\nsrc/main.py:3:2: E501 line too long\n",
        );
        assert_eq!(problems.len(), 4);
        assert_eq!(problems[0].severity, Severity::Error);
        assert_eq!(problems[1].file, PathBuf::from("src/app.ts"));
        assert_eq!(problems[1].column, Some(4));
        assert_eq!(problems[2].message, "compiler diagnostic");
        assert_eq!(problems[3].severity, Severity::Error);
    }

    #[test]
    fn fr_bd03_problem_matcher_handles_lines_split_across_output_chunks() {
        let mut buffer = String::new();
        let mut problems = Vec::new();
        collect_problem_lines(&mut buffer, &mut problems, "src/main.rs:12:4: err", false);
        assert!(problems.is_empty());
        collect_problem_lines(&mut buffer, &mut problems, "or: missing semicolon\n", false);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].file, PathBuf::from("src/main.rs"));
        assert_eq!(problems[0].line, 12);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd01_task_streams_output_and_finishes_without_blocking() {
        let mut run = TaskRun::start(TaskSpec {
            label: "check".into(),
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf 'src/lib.rs:2:3: warning: note\\n'".into(),
            ],
            cwd: None,
            environment: BTreeMap::new(),
        })
        .expect("start task");
        for _ in 0..40 {
            run.poll().expect("poll task");
            if !matches!(run.state(), TaskState::Running) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(run.output().contains("src/lib.rs"));
        assert_eq!(run.problems().len(), 1);
        assert!(matches!(run.state(), TaskState::Finished(_)));
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd07_task_cancellation_reaps_running_process() {
        let mut run = TaskRun::start(TaskSpec {
            label: "long".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 10".into()],
            cwd: None,
            environment: BTreeMap::new(),
        })
        .expect("start task");
        run.cancel().expect("cancel task");
        assert_eq!(run.state(), &TaskState::Cancelled);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd07_task_timeout_reaps_process_and_preserves_output() {
        let mut run = TaskRun::start(TaskSpec {
            label: "timeout".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "printf 'before-timeout'; sleep 10".into()],
            cwd: None,
            environment: BTreeMap::new(),
        })
        .expect("start timeout task");
        run.set_timeout(Duration::from_millis(20));
        for _ in 0..100 {
            run.poll().expect("poll timeout task");
            if !matches!(run.state(), TaskState::Running) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(run.state(), &TaskState::Cancelled);
        assert!(run.timed_out());
        assert!(run.output().contains("before-timeout"));
    }

    #[test]
    fn fr_bd03_problem_matcher_keeps_non_numeric_parens_from_swallowing_lines() {
        let problems = parse_problems("src/app.rs:3:1: error: see docs (rustc)\n");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].file, PathBuf::from("src/app.rs"));
        assert_eq!(problems[0].message, "see docs (rustc)");
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd05_task_output_decodes_multibyte_characters_split_across_chunks() {
        let mut run = TaskRun::start(TaskSpec {
            label: "utf8".into(),
            program: "sh".into(),
            args: vec![
                "-c".into(),
                r"printf '\344'; sleep 0.05; printf '\275\240\n'".into(),
            ],
            cwd: None,
            environment: BTreeMap::new(),
        })
        .expect("start task");
        for _ in 0..100 {
            run.poll().expect("poll task");
            if !matches!(run.state(), TaskState::Running) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(run.output(), "你\n");
    }
    #[test]
    fn fr_bd06_result_tree_groups_pass_fail_and_skipped_tests() {
        let tree = parse_result_tree(
            "test parser::ok ... ok\ntest parser::broken ... FAILED\ntest parser::slow ... ignored\n",
            &TaskState::Finished(super::ExitStatusCode(Some(1))),
        );
        assert_eq!(tree.status, ResultStatus::Failed);
        assert_eq!(tree.children.len(), 3);
        assert_eq!(tree.children[2].status, ResultStatus::Skipped);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd05_task_output_keeps_only_configured_tail() {
        let mut run = TaskRun::start(TaskSpec {
            label: "output".into(),
            program: "sh".into(),
            environment: BTreeMap::new(),
            args: vec!["-c".into(), "printf '0123456789abcdef'".into()],
            cwd: None,
        })
        .expect("start task");
        run.set_output_limit(8);
        for _ in 0..20 {
            run.poll().expect("poll task");
            if !matches!(run.state(), TaskState::Running) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(run.output(), "89abcdef");
    }
}
