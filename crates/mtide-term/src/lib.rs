//! Terminal-session lifecycle and panel state.
//!
//! The current backend uses bounded stdio pipes as a portable fallback. The
//! session boundary is deliberately separate from the VT screen so a PTY
//! backend can be added without changing panel state or rendering contracts.

use mtide_vt::Screen;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("terminal session name is empty")]
    EmptyName,
    #[error("terminal session not found: {0}")]
    MissingSession(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    PipeFallback,
    Pty,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalLinkTarget {
    File {
        path: String,
        line: usize,
        column: Option<usize>,
    },
    Url(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalLink {
    pub text: String,
    pub target: TerminalLinkTarget,
}

/// Finds navigable file locations and URLs in one terminal output line.
///
/// The parser deliberately works on whitespace-delimited tokens instead of
/// interpreting shell syntax. This keeps punctuation around compiler output
/// harmless while keeping parsing independent from shell quoting and command
/// execution.
pub fn extract_links(line: &str) -> Vec<TerminalLink> {
    line.split_whitespace()
        .filter_map(|token| {
            let text = token.trim_matches(|character: char| {
                matches!(
                    character,
                    '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | ':' | '"' | '\''
                )
            });
            if text.starts_with("http://") || text.starts_with("https://") {
                let url = text.trim_end_matches(['.', '!', '?']);
                return Some(TerminalLink {
                    text: url.to_owned(),
                    target: TerminalLinkTarget::Url(url.to_owned()),
                });
            }
            let fields = text.rsplit(':').collect::<Vec<_>>();
            let last = fields.first()?.parse::<usize>().ok()?;
            let (path, line_number, column) = if fields.get(1)?.parse::<usize>().is_ok() {
                let path = fields[2..]
                    .iter()
                    .rev()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(":");
                (path, fields[1].parse::<usize>().ok()?, Some(last))
            } else {
                let path = fields[1..]
                    .iter()
                    .rev()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(":");
                (path, last, None)
            };
            if path.is_empty() || path.starts_with('-') || path.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            Some(TerminalLink {
                text: text.to_owned(),
                target: TerminalLinkTarget::File {
                    path,
                    line: line_number,
                    column,
                },
            })
        })
        .collect()
}

enum SessionInput {
    Pipe(ChildStdin),
    #[cfg(unix)]
    Pty(std::fs::File),
}

pub struct TerminalSession {
    name: String,
    command: String,
    cwd: Option<PathBuf>,
    child: Child,
    input: SessionInput,
    output: Receiver<Vec<u8>>,
    pub screen: Screen,
    scrollback: VecDeque<String>,
    partial_line: String,
    /// Bytes of an incomplete UTF-8 sequence held over for the next read.
    pending_bytes: Vec<u8>,
    max_scrollback: usize,
    backend: BackendKind,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalSession")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("backend", &self.backend)
            .field("scrollback_len", &self.scrollback.len())
            .finish()
    }
}

impl TerminalSession {
    pub fn spawn(
        name: impl Into<String>,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        width: usize,
        height: usize,
        max_scrollback: usize,
    ) -> Result<Self, TerminalError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(TerminalError::EmptyName);
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = cwd {
            command.current_dir(directory);
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "terminal stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "terminal stdout unavailable")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "terminal stderr unavailable")
        })?;
        let (sender, output) = mpsc::sync_channel(256);
        spawn_reader(stdout, sender.clone());
        spawn_reader(stderr, sender);
        Ok(Self {
            name,
            command: std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            cwd: cwd.map(Path::to_path_buf),
            child,
            input: SessionInput::Pipe(input),
            output,
            screen: Screen::new(width, height),
            scrollback: VecDeque::new(),
            partial_line: String::new(),
            pending_bytes: Vec::new(),
            max_scrollback: max_scrollback.max(1),
            backend: BackendKind::PipeFallback,
        })
    }

    /// Starts a real Unix PTY session. Non-Unix callers should use [`Self::spawn`]
    /// or provide a platform backend with the same session boundary.
    #[cfg(unix)]
    pub fn spawn_pty(
        name: impl Into<String>,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        width: usize,
        height: usize,
        max_scrollback: usize,
    ) -> Result<Self, TerminalError> {
        use nix::{
            pty::{openpty, Winsize},
            unistd::close,
        };
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;

        let name = name.into();
        if name.trim().is_empty() {
            return Err(TerminalError::EmptyName);
        }
        let dimensions = Winsize {
            ws_row: height.max(1).min(u16::MAX as usize) as u16,
            ws_col: width.max(1).min(u16::MAX as usize) as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pair = openpty(Some(&dimensions), None).map_err(io::Error::from)?;
        let slave = pair.slave;
        let mut command = Command::new(program);
        command
            .args(args)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(directory) = cwd {
            command.current_dir(directory);
        }
        // SAFETY: the closure runs in the child immediately before exec. It only
        // installs the already-open PTY slave as stdio and closes that fd.
        unsafe {
            command.pre_exec(move || {
                if nix::libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                for target in [
                    nix::libc::STDIN_FILENO,
                    nix::libc::STDOUT_FILENO,
                    nix::libc::STDERR_FILENO,
                ] {
                    if nix::libc::dup2(slave, target) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if nix::libc::close(slave) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = close(slave);
                let _ = close(pair.master);
                return Err(error.into());
            }
        };
        close(slave).map_err(io::Error::from)?;
        // SAFETY: openpty returned a valid owned master fd and no other value
        // wraps it after this point, so File takes unique responsibility for it.
        let master = unsafe { std::fs::File::from_raw_fd(pair.master) };
        let reader = master.try_clone()?;
        let input = master.try_clone()?;
        let (sender, output) = mpsc::sync_channel(256);
        spawn_reader(reader, sender);
        Ok(Self {
            name,
            command: std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            cwd: cwd.map(Path::to_path_buf),
            child,
            input: SessionInput::Pty(input),
            output,
            screen: Screen::new(width, height),
            scrollback: VecDeque::new(),
            partial_line: String::new(),
            pending_bytes: Vec::new(),
            max_scrollback: max_scrollback.max(1),
            backend: BackendKind::Pty,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn rename(&mut self, name: impl Into<String>) -> Result<(), TerminalError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(TerminalError::EmptyName);
        }
        self.name = name;
        Ok(())
    }
    pub fn command(&self) -> &str {
        &self.command
    }
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }
    pub fn backend(&self) -> BackendKind {
        self.backend
    }
    pub fn scrollback(&self) -> impl ExactSizeIterator<Item = &str> {
        self.scrollback.iter().map(String::as_str)
    }
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.max_scrollback = limit.max(1);
        while self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
        }
    }

    pub fn send_input(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        match &mut self.input {
            SessionInput::Pipe(input) => {
                input.write_all(bytes)?;
                input.flush()?;
            }
            #[cfg(unix)]
            SessionInput::Pty(input) => {
                input.write_all(bytes)?;
                input.flush()?;
            }
        }
        Ok(())
    }

    pub fn resize(&mut self, width: usize, height: usize) -> Result<(), TerminalError> {
        self.screen.resize(width, height);
        #[cfg(unix)]
        if let SessionInput::Pty(input) = &self.input {
            use std::os::fd::AsRawFd;
            let dimensions = nix::pty::Winsize {
                ws_row: height.max(1).min(u16::MAX as usize) as u16,
                ws_col: width.max(1).min(u16::MAX as usize) as u16,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: input is a live PTY master fd and dimensions points to a
            // valid winsize value for the duration of the ioctl call.
            let result = unsafe {
                nix::libc::ioctl(
                    input.as_raw_fd(),
                    nix::libc::TIOCSWINSZ,
                    &dimensions as *const nix::pty::Winsize,
                )
            };
            if result == -1 {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(())
    }

    /// Drains available output without blocking the UI thread.
    pub fn poll_output(&mut self) -> usize {
        let mut bytes = 0;
        while let Ok(chunk) = self.output.try_recv() {
            bytes += chunk.len();
            self.screen.feed(&chunk);
            self.collect_scrollback(&chunk);
        }
        bytes
    }

    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, TerminalError> {
        Ok(self.child.try_wait()?)
    }

    pub fn terminate(&mut self) -> Result<Option<std::process::ExitStatus>, TerminalError> {
        if self.child.try_wait()?.is_none() {
            self.kill_tree()?;
        }
        Ok(self.child.wait().ok())
    }

    /// Kills the session and, on Unix PTY sessions, everything in its process
    /// group so no orphan keeps the terminal device alive (PRD NF-12).
    fn kill_tree(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            if matches!(self.input, SessionInput::Pty(_)) {
                if let Ok(pid) = i32::try_from(self.child.id()) {
                    // SAFETY: sending a signal to a negative pid targets the
                    // child's process group; an error is reported by the return
                    // value and is harmless here because the direct kill below
                    // still runs.
                    unsafe {
                        nix::libc::kill(-pid, nix::libc::SIGKILL);
                    }
                }
            }
        }
        self.child.kill()
    }

    /// Appends output to the scrollback, decoding UTF-8 incrementally so a
    /// multi-byte character split across two reads is not mangled.
    fn collect_scrollback(&mut self, chunk: &[u8]) {
        let mut buffer = std::mem::take(&mut self.pending_bytes);
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
        self.pending_bytes = rest.to_vec();
        self.partial_line.push_str(&text);
        let mut complete = Vec::new();
        while let Some(index) = self.partial_line.find('\n') {
            let line = self.partial_line[..index].trim_end_matches('\r').to_owned();
            complete.push(line);
            self.partial_line.drain(..=index);
        }
        for line in complete {
            self.scrollback.push_back(line);
            while self.scrollback.len() > self.max_scrollback {
                self.scrollback.pop_front();
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.kill_tree();
        }
        let _ = self.child.wait();
    }
}

#[derive(Debug, Default)]
pub struct TerminalManager {
    sessions: Vec<TerminalSession>,
    selected: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyStroke {
    pub key: char,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyStroke {
    pub const fn plain(key: char) -> Self {
        Self {
            key,
            control: false,
            alt: false,
            shift: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyMode {
    ForceKeys,
    PassThrough,
    Prefix(char),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutedInput {
    Ide,
    Terminal,
    PrefixPending,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalKeyPolicy {
    mode: KeyMode,
    force_keys: std::collections::BTreeSet<KeyStroke>,
    prefix_pending: bool,
}

impl Default for TerminalKeyPolicy {
    fn default() -> Self {
        let mut force_keys = std::collections::BTreeSet::new();
        force_keys.insert(KeyStroke {
            key: 'q',
            control: true,
            alt: false,
            shift: false,
        });
        force_keys.insert(KeyStroke {
            key: '`',
            control: true,
            alt: false,
            shift: false,
        });
        force_keys.insert(KeyStroke {
            key: 'p',
            control: true,
            alt: false,
            shift: false,
        });
        Self {
            mode: KeyMode::ForceKeys,
            force_keys,
            prefix_pending: false,
        }
    }
}

impl TerminalKeyPolicy {
    pub fn mode(&self) -> KeyMode {
        self.mode
    }
    pub fn set_mode(&mut self, mode: KeyMode) {
        self.mode = mode;
        self.prefix_pending = false;
    }
    pub fn set_force_keys(&mut self, keys: impl IntoIterator<Item = KeyStroke>) {
        self.force_keys = keys.into_iter().collect();
    }
    pub fn force_keys(&self) -> &std::collections::BTreeSet<KeyStroke> {
        &self.force_keys
    }
    pub fn route(&mut self, key: KeyStroke) -> RoutedInput {
        if let KeyMode::Prefix(prefix) = self.mode {
            if self.prefix_pending {
                self.prefix_pending = false;
                return if self.force_keys.contains(&key) {
                    RoutedInput::Ide
                } else {
                    RoutedInput::Terminal
                };
            }
            if key == KeyStroke::plain(prefix) {
                self.prefix_pending = true;
                return RoutedInput::PrefixPending;
            }
        }
        if self.mode == KeyMode::PassThrough || !self.force_keys.contains(&key) {
            RoutedInput::Terminal
        } else {
            RoutedInput::Ide
        }
    }
    pub fn prefix_pending(&self) -> bool {
        self.prefix_pending
    }
}

#[derive(Debug)]
pub struct TerminalPanel {
    pub sessions: TerminalManager,
    pub key_policy: TerminalKeyPolicy,
    fullscreen: bool,
    focused: bool,
    scroll_offset: usize,
}

impl Default for TerminalPanel {
    fn default() -> Self {
        Self {
            sessions: TerminalManager::new(),
            key_policy: TerminalKeyPolicy::default(),
            fullscreen: false,
            focused: false,
            scroll_offset: 0,
        }
    }
}

impl TerminalPanel {
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }
    pub fn is_focused(&self) -> bool {
        self.focused
    }
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }
    pub fn toggle_fullscreen(&mut self) -> bool {
        self.fullscreen = !self.fullscreen;
        self.fullscreen
    }
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }
    pub fn scroll(&mut self, delta: isize, available_lines: usize) {
        let max_offset = self
            .sessions
            .selected()
            .map(|session| session.scrollback().len().saturating_sub(available_lines))
            .unwrap_or(0);
        self.scroll_offset = if delta.is_negative() {
            self.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll_offset
                .saturating_add(delta as usize)
                .min(max_offset)
        };
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.sessions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
    pub fn selected_index(&self) -> Option<usize> {
        if self.sessions.is_empty() {
            None
        } else {
            Some(self.selected.min(self.sessions.len() - 1))
        }
    }
    pub fn selected(&self) -> Option<&TerminalSession> {
        self.selected_index()
            .and_then(|index| self.sessions.get(index))
    }
    pub fn selected_mut(&mut self) -> Option<&mut TerminalSession> {
        let index = self.selected_index()?;
        self.sessions.get_mut(index)
    }
    pub fn get(&self, index: usize) -> Option<&TerminalSession> {
        self.sessions.get(index)
    }
    pub fn select(&mut self, index: usize) -> Result<(), TerminalError> {
        if index >= self.sessions.len() {
            return Err(TerminalError::MissingSession(index));
        }
        self.selected = index;
        Ok(())
    }
    pub fn next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }
    pub fn previous(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + self.sessions.len() - 1) % self.sessions.len();
        }
    }
    pub fn add(&mut self, session: TerminalSession) -> usize {
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        self.selected
    }
    pub fn close_selected(&mut self) -> Option<TerminalSession> {
        if self.sessions.is_empty() {
            return None;
        }
        let removed = self.sessions.remove(self.selected);
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        Some(removed)
    }
    pub fn poll_all(&mut self) -> usize {
        self.sessions
            .iter_mut()
            .map(TerminalSession::poll_output)
            .sum()
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R, sender: SyncSender<Vec<u8>>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    if sender.send(buffer[..length].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        extract_links, BackendKind, KeyMode, KeyStroke, RoutedInput, TerminalError,
        TerminalKeyPolicy, TerminalLinkTarget, TerminalManager, TerminalPanel, TerminalSession,
    };
    use std::{thread, time::Duration};

    #[test]
    fn fr_tr05_terminal_output_links_parse_files_urls_and_punctuation() {
        let links = extract_links(
            "error at (src/main.rs:12:8): see https://example.test/docs?q=1, and lib.rs:4",
        );
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0].target,
            TerminalLinkTarget::File {
                path: "src/main.rs".into(),
                line: 12,
                column: Some(8),
            }
        );
        assert_eq!(
            links[1].target,
            TerminalLinkTarget::Url("https://example.test/docs?q=1".into())
        );
        assert_eq!(
            links[2].target,
            TerminalLinkTarget::File {
                path: "lib.rs".into(),
                line: 4,
                column: None,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr02_terminal_session_reads_output_and_bounds_scrollback() {
        let mut session = TerminalSession::spawn(
            "build",
            "sh",
            &["-c".into(), "printf 'one\\ntwo\\nthree\\n'".into()],
            None,
            20,
            4,
            2,
        )
        .expect("spawn test shell");
        for _ in 0..20 {
            if session.poll_output() > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(session.backend(), BackendKind::PipeFallback);
        assert_eq!(
            session.scrollback().collect::<Vec<_>>(),
            vec!["two", "three"]
        );
        assert!(session.screen.row_text(0).starts_with("one"));
    }

    #[test]
    fn fr_tr02_terminal_session_rejects_empty_names() {
        let error = TerminalSession::spawn(" ", "missing-program", &[], None, 8, 2, 10)
            .expect_err("empty names must be rejected before spawn");
        assert!(matches!(error, TerminalError::EmptyName));
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr02_terminal_manager_keeps_named_instances_and_focus() {
        let mut manager = TerminalManager::new();
        for name in ["one", "two", "three"] {
            manager.add(
                TerminalSession::spawn(name, "sh", &["-c".into(), "true".into()], None, 8, 2, 10)
                    .expect("spawn terminal"),
            );
        }
        assert_eq!(manager.len(), 3);
        assert_eq!(manager.selected().map(TerminalSession::name), Some("three"));
        manager.select(0).expect("select first");
        manager.next();
        assert_eq!(manager.selected().map(TerminalSession::name), Some("two"));
        assert_eq!(
            manager
                .close_selected()
                .map(|session| session.name().to_owned()),
            Some("two".into())
        );
        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn fr_tr03_force_keys_and_prefix_mode_route_input_deterministically() {
        let mut policy = TerminalKeyPolicy::default();
        assert_eq!(
            policy.route(KeyStroke {
                key: 'q',
                control: true,
                alt: false,
                shift: false
            }),
            RoutedInput::Ide
        );
        assert_eq!(policy.route(KeyStroke::plain('x')), RoutedInput::Terminal);
        policy.set_mode(KeyMode::Prefix(' '));
        assert_eq!(
            policy.route(KeyStroke::plain(' ')),
            RoutedInput::PrefixPending
        );
        assert!(policy.prefix_pending());
        assert_eq!(policy.route(KeyStroke::plain('x')), RoutedInput::Terminal);
        assert!(!policy.prefix_pending());
    }

    #[test]
    fn fr_tr06_terminal_panel_restores_focus_and_fullscreen_state() {
        let mut panel = TerminalPanel::default();
        panel.set_focus(true);
        assert!(panel.is_focused());
        assert!(panel.toggle_fullscreen());
        assert!(panel.is_fullscreen());
        assert!(!panel.toggle_fullscreen());
        assert!(!panel.is_fullscreen());
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr01_scrollback_decodes_utf8_split_across_reads() {
        let mut session = TerminalSession::spawn(
            "utf8",
            "sh",
            &[
                "-c".into(),
                "printf 'a\\344'; sleep 0.05; printf '\\275\\240\\n'".into(),
            ],
            None,
            20,
            4,
            10,
        )
        .expect("spawn test shell");
        for _ in 0..100 {
            session.poll_output();
            if session.scrollback().any(|line| line == "a你") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(session.scrollback().collect::<Vec<_>>(), vec!["a你"]);
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr01_real_pty_echoes_input_and_accepts_resize() {
        let mut session = TerminalSession::spawn_pty(
            "pty",
            "sh",
            &[
                "-c".into(),
                "read line; printf 'got:%s\\n' \"$line\"".into(),
            ],
            None,
            20,
            4,
            10,
        )
        .expect("spawn pty");
        assert_eq!(session.backend(), BackendKind::Pty);
        session.resize(30, 6).expect("resize pty");
        assert_eq!((session.screen.width(), session.screen.height()), (30, 6));
        session.send_input(b"hello\n").expect("send pty input");
        for _ in 0..50 {
            session.poll_output();
            if session.scrollback().any(|line| line == "got:hello") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(session.scrollback().any(|line| line == "got:hello"));
    }
}
