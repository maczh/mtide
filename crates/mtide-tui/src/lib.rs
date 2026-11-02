//! Terminal primitives: semantic cells, diff rendering, input, and cleanup.

use crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Color, Print, SetBackgroundColor, SetForegroundColor},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use mtide_api::ColorSlot;
use serde::Deserialize;
use std::io::{self, Write};
use std::{fs, path::Path};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub symbol: char,
    pub style: ColorSlot,
    pub background: Option<ColorSlot>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: ColorSlot::Default,
            background: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellBuffer {
    pub width: u16,
    pub height: u16,
    cells: Vec<Cell>,
}

/// Small keyboard-first component states shared by panels and dialogs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputState {
    value: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
}

impl InputState {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self {
            value,
            cursor,
            history: Vec::new(),
            history_index: None,
        }
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn submit(&mut self) -> String {
        let value = std::mem::take(&mut self.value);
        self.cursor = 0;
        self.history_index = None;
        if !value.is_empty() && self.history.last() != Some(&value) {
            self.history.push(value.clone());
        }
        value
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        match key.code {
            KeyCode::Char(ch) if key.modifiers.is_empty() => {
                self.value.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let start = self.value[..self.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.value.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Left => {
                self.cursor = self.value[..self.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            KeyCode::Right => {
                self.cursor += self.value[self.cursor..]
                    .graphemes(true)
                    .next()
                    .map(str::len)
                    .unwrap_or(0);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Enter => return Some(self.submit()),
            KeyCode::Up | KeyCode::Down if !self.history.is_empty() => {
                let last = self.history.len() - 1;
                let current = self.history_index.unwrap_or(last);
                let next = if key.code == KeyCode::Up {
                    current.saturating_sub(1)
                } else {
                    (current + 1).min(last)
                };
                self.history_index = Some(next);
                self.value = self.history[next].clone();
                self.cursor = self.value.len();
            }
            _ => return None,
        }
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListState {
    selected: usize,
    offset: usize,
    length: usize,
}

impl ListState {
    pub fn new(length: usize) -> Self {
        Self {
            selected: 0,
            offset: 0,
            length,
        }
    }
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn offset(&self) -> usize {
        self.offset
    }
    pub fn set_viewport(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
    }
    pub fn handle_key(&mut self, key: KeyEvent, height: usize) -> bool {
        let old = self.selected;
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.length.saturating_sub(1)),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.length.saturating_sub(1),
            _ => return false,
        }
        self.set_viewport(height);
        old != self.selected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmResult {
    Pending,
    Confirmed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmState {
    result: ConfirmResult,
}

impl Default for ConfirmState {
    fn default() -> Self {
        Self {
            result: ConfirmResult::Pending,
        }
    }
}
impl ConfirmState {
    pub fn result(&self) -> ConfirmResult {
        self.result
    }
    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmResult {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.result = ConfirmResult::Confirmed
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.result = ConfirmResult::Cancelled
            }
            _ => {}
        }
        self.result
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressState {
    current: u64,
    total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEntry {
    pub id: String,
    pub title: String,
    pub shortcut: Option<String>,
    last_used: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CommandRegistry {
    entries: Vec<CommandEntry>,
    clock: u64,
}

impl CommandRegistry {
    pub fn register(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        shortcut: Option<String>,
    ) {
        self.entries.push(CommandEntry {
            id: id.into(),
            title: title.into(),
            shortcut,
            last_used: 0,
        });
    }
    pub fn mark_used(&mut self, id: &str) {
        self.clock = self.clock.saturating_add(1);
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.last_used = self.clock;
        }
    }
    pub fn search(&self, query: &str, limit: usize) -> Vec<CommandEntry> {
        let query = query.to_ascii_lowercase();
        let mut matches = self
            .entries
            .iter()
            .filter(|entry| {
                query.is_empty()
                    || fuzzy_subsequence(&query, &entry.title.to_ascii_lowercase())
                    || fuzzy_subsequence(&query, &entry.id.to_ascii_lowercase())
            })
            .cloned()
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .last_used
                .cmp(&left.last_used)
                .then_with(|| left.title.cmp(&right.title))
        });
        matches.truncate(limit);
        matches
    }
}

fn fuzzy_subsequence(query: &str, candidate: &str) -> bool {
    let mut remaining = candidate.chars();
    query
        .chars()
        .all(|wanted| remaining.any(|actual| actual == wanted))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClipboardHistory {
    entries: Vec<String>,
    limit: usize,
}

impl ClipboardHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            limit: limit.max(1),
        }
    }
    pub fn push(&mut self, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.entries.retain(|entry| entry != &text);
        self.entries.insert(0, text);
        self.entries.truncate(self.limit);
    }
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
    /// Returns an OSC 52 sequence suitable for terminals that expose a
    /// clipboard but do not provide a local clipboard command.
    pub fn osc52(&self, index: usize) -> Option<String> {
        let value = self.entries.get(index)?;
        Some(format!("\x1b]52;c;{}\x07", base64(value.as_bytes())))
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        output.push(TABLE[((a >> 2) & 0x3f) as usize] as char);
        output.push(TABLE[(((a << 4) | (b >> 4)) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((b << 2) | (c >> 6)) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

impl ProgressState {
    pub fn new(total: Option<u64>) -> Self {
        Self { current: 0, total }
    }
    pub fn set(&mut self, current: u64) {
        self.current = current;
    }
    pub fn fraction(&self) -> Option<f32> {
        self.total
            .filter(|total| *total > 0)
            .map(|total| (self.current.min(total) as f32) / total as f32)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tab {
    pub title: String,
    pub pinned: bool,
    pub dirty: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TabState {
    tabs: Vec<Tab>,
    selected: usize,
}

impl TabState {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }
    pub fn selected(&self) -> Option<&Tab> {
        self.tabs.get(self.selected)
    }
    pub fn selected_index(&self) -> usize {
        self.selected
    }
    pub fn push(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.selected = self.tabs.len().saturating_sub(1);
    }
    pub fn select(&mut self, index: usize) {
        if !self.tabs.is_empty() {
            self.selected = index.min(self.tabs.len() - 1);
        }
    }
    pub fn next(&mut self) {
        if !self.tabs.is_empty() {
            self.selected = (self.selected + 1) % self.tabs.len();
        }
    }
    pub fn previous(&mut self) {
        if !self.tabs.is_empty() {
            self.selected = (self.selected + self.tabs.len() - 1) % self.tabs.len();
        }
    }
    pub fn close_selected(&mut self) -> Option<Tab> {
        if self.tabs.is_empty() {
            return None;
        }
        let removed = self.tabs.remove(self.selected);
        self.selected = self.selected.min(self.tabs.len().saturating_sub(1));
        Some(removed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeNode {
    pub label: String,
    pub depth: usize,
    pub expanded: bool,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeState {
    nodes: Vec<TreeNode>,
    selected: usize,
    offset: usize,
}

impl TreeState {
    pub fn new(nodes: Vec<TreeNode>) -> Self {
        Self {
            nodes,
            ..Self::default()
        }
    }
    pub fn visible(&self) -> &[TreeNode] {
        &self.nodes
    }
    pub fn selected(&self) -> Option<&TreeNode> {
        self.nodes.get(self.selected)
    }
    pub fn select(&mut self, index: usize) {
        if !self.nodes.is_empty() {
            self.selected = index.min(self.nodes.len() - 1);
        }
    }
    pub fn move_by(&mut self, delta: isize, viewport: usize) {
        if self.nodes.is_empty() {
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(self.nodes.len() - 1)
        };
        if viewport > 0 {
            if self.selected < self.offset {
                self.offset = self.selected;
            }
            if self.selected >= self.offset + viewport {
                self.offset = self.selected + 1 - viewport;
            }
        }
    }
    pub fn viewport(&self) -> usize {
        self.offset
    }
    pub fn toggle_selected(&mut self) {
        if let Some(node) = self.nodes.get_mut(self.selected) {
            if node.is_dir {
                node.expanded = !node.expanded;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Editor,
    Sidebar,
    Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Save,
    Quit,
    Undo,
    Redo,
    QuickOpen,
    CommandPalette,
    Find,
    Replace,
    GoToLine,
    ToggleTerminal,
    Build,
    Run,
    Debug,
    Format,
    NavigateBack,
    NavigateForward,
    AddCursor,
    ToggleSidebar,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Home,
    End,
    Insert(char),
    Backspace,
    Enter,
    None,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyBinding {
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Keymap {
    bindings: Vec<(KeyBinding, Action)>,
}

impl Keymap {
    pub fn preset(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "vim" => Self::vim(),
            "emacs" => Self::emacs(),
            "borland" => Self::borland(),
            _ => Self::vscode(),
        }
    }

    pub fn vscode() -> Self {
        Self {
            bindings: vec![
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('s'),
                    },
                    Action::Save,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('p'),
                    },
                    Action::QuickOpen,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('f'),
                    },
                    Action::Find,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('h'),
                    },
                    Action::Replace,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('z'),
                    },
                    Action::Undo,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('y'),
                    },
                    Action::Redo,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                        code: KeyCode::Char('p'),
                    },
                    Action::CommandPalette,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('g'),
                    },
                    Action::GoToLine,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL,
                        code: KeyCode::Char('`'),
                    },
                    Action::ToggleTerminal,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                        code: KeyCode::Char('b'),
                    },
                    Action::Build,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::NONE,
                        code: KeyCode::F(5),
                    },
                    Action::Run,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::NONE,
                        code: KeyCode::F(9),
                    },
                    Action::Debug,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
                        code: KeyCode::Char('f'),
                    },
                    Action::Format,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::ALT,
                        code: KeyCode::Left,
                    },
                    Action::NavigateBack,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::ALT,
                        code: KeyCode::Right,
                    },
                    Action::NavigateForward,
                ),
                (
                    KeyBinding {
                        modifiers: KeyModifiers::ALT,
                        code: KeyCode::Char('d'),
                    },
                    Action::AddCursor,
                ),
            ],
        }
    }

    pub fn vim() -> Self {
        let mut keymap = Self::vscode();
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('w'),
            },
            Action::Quit,
        );
        keymap
    }

    pub fn emacs() -> Self {
        let mut keymap = Self::vscode();
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('x'),
            },
            Action::Quit,
        );
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('b'),
            },
            Action::MoveLeft,
        );
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('f'),
            },
            Action::MoveRight,
        );
        keymap
    }

    pub fn borland() -> Self {
        let mut keymap = Self::vscode();
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('q'),
            },
            Action::Quit,
        );
        keymap.bind(
            KeyBinding {
                modifiers: KeyModifiers::CONTROL,
                code: KeyCode::Char('s'),
            },
            Action::Save,
        );
        keymap
    }

    pub fn bind(&mut self, binding: KeyBinding, action: Action) {
        self.bindings.push((binding, action));
    }
    pub fn has_binding(&self, key: KeyEvent) -> bool {
        self.bindings
            .iter()
            .any(|(binding, _)| binding.code == key.code && binding.modifiers == key.modifiers)
    }
    pub fn action(&self, key: KeyEvent) -> Action {
        self.bindings
            .iter()
            .rev()
            .find(|(binding, _)| binding.code == key.code && binding.modifiers == key.modifiers)
            .map(|(_, action)| *action)
            .unwrap_or_else(|| action_for_key(key))
    }
    pub fn conflicts(&self) -> Vec<KeyBinding> {
        let mut conflicts = Vec::new();
        for (index, (binding, _)) in self.bindings.iter().enumerate() {
            if self.bindings[..index]
                .iter()
                .any(|(other, _)| other == binding)
            {
                conflicts.push(*binding);
            }
        }
        conflicts
    }

    /// Imports the small, safe subset of vimrc mappings that can be expressed
    /// by mtide's command-oriented keymap. Ex commands are never executed.
    /// The return value is `(imported, ignored_lines)` for UI feedback.
    pub fn import_vimrc(&mut self, path: &Path) -> io::Result<(usize, Vec<usize>)> {
        let source = fs::read_to_string(path)?;
        let mut imported = 0;
        let mut ignored = Vec::new();
        for (line_number, line) in source.lines().enumerate() {
            let line = line.split('"').next().unwrap_or_default().trim();
            if line.is_empty() || line.starts_with("set ") || line.starts_with("let ") {
                continue;
            }
            let mut parts = line.split_whitespace();
            let directive = parts.next().unwrap_or_default();
            if !matches!(
                directive,
                "map" | "nmap" | "nnoremap" | "inoremap" | "vnoremap"
            ) {
                ignored.push(line_number + 1);
                continue;
            }
            let Some(lhs) = parts.next() else {
                ignored.push(line_number + 1);
                continue;
            };
            let rhs = parts.collect::<Vec<_>>().join(" ");
            let Some(binding) = parse_vim_key(lhs) else {
                ignored.push(line_number + 1);
                continue;
            };
            let Some(action) = vim_command_action(&rhs) else {
                ignored.push(line_number + 1);
                continue;
            };
            self.bind(binding, action);
            imported += 1;
        }
        Ok((imported, ignored))
    }
}

fn parse_vim_key(value: &str) -> Option<KeyBinding> {
    let value = value.trim();
    if value.len() == 1 {
        return Some(KeyBinding {
            modifiers: KeyModifiers::NONE,
            code: KeyCode::Char(value.chars().next()?),
        });
    }
    let inner = value.strip_prefix('<')?.strip_suffix('>')?;
    let mut modifiers = KeyModifiers::NONE;
    let mut key = inner;
    for prefix in ["C-", "S-", "A-", "M-"] {
        if let Some(rest) = key.strip_prefix(prefix) {
            modifiers |= match prefix {
                "C-" => KeyModifiers::CONTROL,
                "S-" => KeyModifiers::SHIFT,
                _ => KeyModifiers::ALT,
            };
            key = rest;
        }
    }
    let code = match key.to_ascii_lowercase().as_str() {
        "esc" => KeyCode::Esc,
        "cr" | "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        value
            if value
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .is_some() =>
        {
            KeyCode::F(value[1..].parse().ok()?)
        }
        value if value.len() == 1 => KeyCode::Char(value.chars().next()?),
        _ => return None,
    };
    Some(KeyBinding { modifiers, code })
}

fn vim_command_action(rhs: &str) -> Option<Action> {
    let command = rhs.trim().trim_start_matches(':').to_ascii_lowercase();
    if command.contains("<plug>") {
        return None;
    }
    if command.starts_with("wq") || command.starts_with("x") {
        return Some(Action::Save);
    }
    if command.starts_with('w') || command.contains("write") {
        return Some(Action::Save);
    }
    if command.starts_with('q') || command.contains("quit") {
        return Some(Action::Quit);
    }
    if command.contains("files") || command.contains("find") || command.contains("edit ") {
        return Some(Action::QuickOpen);
    }
    if command.contains("buffers") {
        return Some(Action::QuickOpen);
    }
    if command.starts_with('/') || command.contains("search") {
        return Some(Action::Find);
    }
    if command.contains("undo") {
        return Some(Action::Undo);
    }
    if command.contains("redo") {
        return Some(Action::Redo);
    }
    None
}

pub fn action_for_key(key: KeyEvent) -> Action {
    let primary = key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
    match (primary, key.code) {
        (true, KeyCode::Char('s')) => Action::Save,
        (true, KeyCode::Char('q')) | (true, KeyCode::Char('c')) => Action::Quit,
        (true, KeyCode::Char('z')) => Action::Undo,
        (true, KeyCode::Char('y')) => Action::Redo,
        (true, KeyCode::Char('p')) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Action::CommandPalette
        }
        (true, KeyCode::Char('p')) => Action::QuickOpen,
        (true, KeyCode::Char('f')) => Action::Find,
        (true, KeyCode::Char('h')) => Action::Replace,
        (true, KeyCode::Char('g')) => Action::GoToLine,
        (true, KeyCode::Char('`')) => Action::ToggleTerminal,
        (true, KeyCode::Char('b')) if key.modifiers.contains(KeyModifiers::SHIFT) => Action::Build,
        (true, KeyCode::Char('b')) => Action::ToggleSidebar,
        (true, KeyCode::Char('k')) => Action::CommandPalette,
        (false, KeyCode::Left) if key.modifiers.contains(KeyModifiers::ALT) => Action::NavigateBack,
        (false, KeyCode::Right) if key.modifiers.contains(KeyModifiers::ALT) => {
            Action::NavigateForward
        }
        (_, KeyCode::Left) => Action::MoveLeft,
        (_, KeyCode::Right) => Action::MoveRight,
        (_, KeyCode::Up) => Action::MoveUp,
        (_, KeyCode::Down) => Action::MoveDown,
        (_, KeyCode::Home) => Action::Home,
        (_, KeyCode::End) => Action::End,
        (_, KeyCode::Backspace) => Action::Backspace,
        (_, KeyCode::Enter) => Action::Enter,
        (false, KeyCode::F(5)) => Action::Run,
        (false, KeyCode::F(9)) => Action::Debug,
        (_, KeyCode::Char(ch)) if !primary => Action::Insert(ch),
        _ => Action::None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub sidebar: Option<(u16, u16)>,
    pub editor: (u16, u16),
    pub status: (u16, u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorCapability {
    NoColor,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Theme {
    Retro,
    Modern,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThemePackage {
    pub name: String,
    #[serde(default)]
    pub function_key_bar: bool,
    #[serde(default = "default_horizontal")]
    pub border_horizontal: String,
    #[serde(default = "default_vertical")]
    pub border_vertical: String,
    pub foreground: Option<String>,
    pub background: Option<String>,
    /// Optional ANSI palette overrides used by terminal-backed panels.
    /// Keys are conventional names such as `black`, `red`, `bright_blue`,
    /// `foreground`, and `background`; values are terminal colour strings.
    #[serde(default)]
    pub ansi_colors: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Error)]
pub enum ThemePackageError {
    #[error("theme package I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("theme package is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("theme package name is empty")]
    EmptyName,
}

fn default_horizontal() -> String {
    "─".into()
}
fn default_vertical() -> String {
    "│".into()
}

pub fn load_theme_package(path: &Path) -> Result<ThemePackage, ThemePackageError> {
    let package: ThemePackage = serde_json::from_slice(&fs::read(path)?)?;
    if package.name.trim().is_empty() {
        return Err(ThemePackageError::EmptyName);
    }
    Ok(package)
}

/// Discover user-installed theme packages in deterministic order. Invalid
/// files are ignored so one broken community theme cannot prevent startup.
pub fn discover_theme_packages(dir: &Path) -> Vec<(String, ThemePackage)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let package = load_theme_package(&path).ok()?;
            Some((package.name.clone(), package))
        })
        .collect()
}

impl Theme {
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("modern") {
            Self::Modern
        } else {
            Self::Retro
        }
    }
    pub fn shows_function_key_bar(self) -> bool {
        matches!(self, Self::Retro)
    }
    pub fn border_chars(self, capabilities: TerminalCapabilities) -> (&'static str, &'static str) {
        if matches!(self, Self::Modern) {
            (" ", " ")
        } else {
            capabilities.border_chars()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCapabilities {
    pub color: ColorCapability,
    pub unicode: bool,
}

impl TerminalCapabilities {
    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Self {
                color: ColorCapability::NoColor,
                unicode: false,
            };
        }
        let term = std::env::var("TERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let color = match std::env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "truecolor" | "24bit" => ColorCapability::TrueColor,
            _ if term.contains("256color") => ColorCapability::Ansi256,
            _ if term.is_empty() || term == "dumb" => ColorCapability::NoColor,
            _ => ColorCapability::Ansi16,
        };
        let unicode = !term.contains("linux")
            || std::env::var("LANG")
                .map(|lang| lang.to_ascii_lowercase().contains("utf-8"))
                .unwrap_or(false);
        Self { color, unicode }
    }
    pub fn border_chars(&self) -> (&'static str, &'static str) {
        if self.unicode {
            ("─", "│")
        } else {
            ("-", "|")
        }
    }
}

/// Keeps the editor usable at 80x24 by collapsing secondary chrome.
pub fn layout(width: u16, height: u16, sidebar_requested: bool) -> Layout {
    layout_with_sidebar(width, height, sidebar_requested, None)
}

pub fn layout_with_sidebar(
    width: u16,
    height: u16,
    sidebar_requested: bool,
    sidebar_width: Option<u16>,
) -> Layout {
    let status_height = height.min(1);
    let editor_height = height.saturating_sub(status_height);
    if sidebar_requested && width >= 100 {
        let sidebar_width = sidebar_width
            .unwrap_or(width / 5)
            .clamp(20, width.saturating_sub(40));
        Layout {
            sidebar: Some((sidebar_width, editor_height)),
            editor: (width - sidebar_width, editor_height),
            status: (width, status_height),
        }
    } else {
        Layout {
            sidebar: None,
            editor: (width, editor_height),
            status: (width, status_height),
        }
    }
}

pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    for grapheme in line.graphemes(true) {
        let current_width = UnicodeWidthStr::width(current.as_str());
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if !current.is_empty() && current_width + grapheme_width > width {
            rows.push(std::mem::take(&mut current));
        }
        current.push_str(grapheme);
    }
    rows.push(current);
    rows
}

impl CellBuffer {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); width as usize * height as usize],
        }
    }
    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x < self.width && y < self.height {
            self.cells[y as usize * self.width as usize + x as usize] = cell;
        }
    }
    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        (x < self.width && y < self.height)
            .then(|| &self.cells[y as usize * self.width as usize + x as usize])
    }
    pub fn write_str(&mut self, x: u16, y: u16, text: &str, style: ColorSlot) {
        let mut offset = 0u16;
        for grapheme in text.graphemes(true) {
            let Some(position) = x.checked_add(offset) else {
                break;
            };
            let Some(symbol) = grapheme.chars().next() else {
                continue;
            };
            self.set(
                position,
                y,
                Cell {
                    symbol,
                    style,
                    background: None,
                },
            );
            offset = offset.saturating_add(UnicodeWidthStr::width(grapheme).max(1) as u16);
        }
    }

    /// Writes a small, renderer-independent subset of ANSI SGR output.
    ///
    /// Task and terminal producers commonly emit SGR colour sequences.  The
    /// cell buffer deliberately stores semantic colours, so map the portable
    /// 16-colour subset to slots and discard control bytes instead of leaking
    /// escape sequences into the UI.  Unknown SGR values retain the current
    /// style and reset returns to `base`.
    pub fn write_ansi(&mut self, x: u16, y: u16, text: &str, base: ColorSlot) {
        let mut style = base;
        let mut visible = String::new();
        let mut offset = 0_u16;
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' && chars.peek() == Some(&'[') {
                let _ = chars.next();
                let mut parameters = String::new();
                for next in chars.by_ref() {
                    if next == 'm' {
                        for parameter in parameters
                            .split(';')
                            .filter_map(|value| value.parse::<u16>().ok())
                        {
                            style = match parameter {
                                0 => base,
                                1 => ColorSlot::Focus,
                                30 | 90 => ColorSlot::Muted,
                                31 | 91 => ColorSlot::Error,
                                32 | 92 => ColorSlot::Success,
                                33 | 93 => ColorSlot::Focus,
                                34 | 94 => ColorSlot::Focus,
                                36 | 96 => ColorSlot::Focus,
                                _ => style,
                            };
                        }
                        break;
                    }
                    if next.is_ascii_digit() || next == ';' {
                        parameters.push(next);
                    } else {
                        break;
                    }
                }
                continue;
            }
            visible.clear();
            visible.push(character);
            self.write_str(x.saturating_add(offset), y, &visible, style);
            offset = offset.saturating_add(UnicodeWidthStr::width(visible.as_str()).max(1) as u16);
        }
    }
    pub fn diff<'a>(
        &'a self,
        previous: &'a Self,
    ) -> impl Iterator<Item = (u16, u16, &'a Cell)> + 'a {
        self.cells
            .iter()
            .enumerate()
            .filter_map(move |(index, cell)| {
                if previous.cells.get(index) != Some(cell) {
                    let x = (index % self.width as usize) as u16;
                    let y = (index / self.width as usize) as u16;
                    Some((x, y, cell))
                } else {
                    None
                }
            })
    }
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("terminal setup failed: {0}")]
    Setup(String),
}

pub struct TerminalGuard;

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let _ = terminal::disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
        previous(panic);
    }));
}

impl TerminalGuard {
    pub fn enter() -> Result<Self, TuiError> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(error) = execute!(out, EnterAlternateScreen, EnableMouseCapture, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(TuiError::Io(error));
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
    }
}

pub fn render<W: Write>(
    out: &mut W,
    current: &CellBuffer,
    previous: &CellBuffer,
) -> Result<(), TuiError> {
    render_with_options(out, current, previous, RenderOptions::default())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderOptions {
    pub inherit_terminal_colors: bool,
    pub transparent_background: bool,
}

pub fn render_with_options<W: Write>(
    out: &mut W,
    current: &CellBuffer,
    previous: &CellBuffer,
    options: RenderOptions,
) -> Result<(), TuiError> {
    for (x, y, cell) in current.diff(previous) {
        execute!(
            out,
            cursor::MoveTo(x, y),
            SetForegroundColor(color(cell.style, options.inherit_terminal_colors)),
            SetBackgroundColor(
                cell.background
                    .map(|background| color(background, false))
                    .unwrap_or(if options.transparent_background {
                        Color::Reset
                    } else {
                        Color::Black
                    })
            ),
            Print(cell.symbol)
        )?;
    }
    out.flush()?;
    Ok(())
}

fn color(slot: ColorSlot, inherit_terminal_colors: bool) -> Color {
    if std::env::var_os("NO_COLOR").is_some() {
        return Color::Reset;
    }
    match slot {
        ColorSlot::Default if inherit_terminal_colors => Color::Reset,
        ColorSlot::Default => Color::White,
        ColorSlot::Muted => Color::DarkGrey,
        ColorSlot::Focus => Color::Cyan,
        ColorSlot::Error => Color::Red,
        ColorSlot::Success => Color::Green,
        ColorSlot::Indexed(index) => Color::AnsiValue(index),
        ColorSlot::Rgb(red, green, blue) => Color::Rgb {
            r: red,
            g: green,
            b: blue,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fr_ui01_diff_only_contains_changed_cells() {
        let old = CellBuffer::new(80, 24);
        let mut new = old.clone();
        new.write_str(2, 3, "编辑🙂", ColorSlot::Focus);
        let changed: Vec<_> = new.diff(&old).collect();
        assert_eq!(changed.len(), "编辑🙂".chars().count());
        assert_eq!((changed[0].0, changed[0].1), (2, 3));
        assert_eq!(changed[1].0, 4);
    }

    #[test]
    fn fr_ui01_narrow_buffer_bounds_writes() {
        let mut buffer = CellBuffer::new(4, 1);
        buffer.write_str(3, 0, "宽字符", ColorSlot::Default);
        assert_eq!(buffer.get(3, 0).map(|cell| cell.symbol), Some('宽'));
        assert!(buffer.get(4, 0).is_none());
    }

    #[test]
    fn fr_ui08_ansi_sgr_is_rendered_as_semantic_cells() {
        let mut buffer = CellBuffer::new(24, 1);
        buffer.write_ansi(0, 0, "ok \u{1b}[31merror\u{1b}[0m done", ColorSlot::Default);
        assert_eq!(
            buffer.get(0, 0).map(|cell| cell.style),
            Some(ColorSlot::Default)
        );
        assert_eq!(
            buffer.get(3, 0).map(|cell| cell.style),
            Some(ColorSlot::Error)
        );
        assert_eq!(
            buffer.get(8, 0).map(|cell| cell.style),
            Some(ColorSlot::Default)
        );
        assert_eq!(buffer.get(3, 0).map(|cell| cell.symbol), Some('e'));
    }

    #[test]
    fn fr_ui01_small_terminal_collapses_sidebar_and_preserves_focus_keys() {
        assert_eq!(layout(80, 24, true).sidebar, None);
        assert_eq!(layout(120, 40, true).sidebar, Some((24, 39)));
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Save
        );
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Insert('x')
        );
        assert_eq!(
            layout_with_sidebar(120, 40, true, Some(32)).sidebar,
            Some((32, 39))
        );
    }

    #[test]
    fn fr_ui06_input_history_and_grapheme_cursor_are_keyboard_safe() {
        let mut input = InputState::default();
        for ch in "中🙂".chars() {
            input.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        input.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.value(), "中");
        assert_eq!(input.submit(), "中");
        input.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(input.value(), "中");
    }

    #[test]
    fn fr_ui06_list_confirmation_and_progress_preserve_bounds() {
        let mut list = ListState::new(10);
        for _ in 0..20 {
            list.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 3);
        }
        assert_eq!(list.selected(), 9);
        assert_eq!(list.offset(), 7);
        let mut confirm = ConfirmState::default();
        assert_eq!(
            confirm.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ConfirmResult::Cancelled
        );
        let mut progress = ProgressState::new(Some(10));
        progress.set(20);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn fr_ui10_capabilities_provide_unicode_and_ascii_border_fallbacks() {
        let unicode = TerminalCapabilities {
            color: ColorCapability::Ansi256,
            unicode: true,
        };
        let ascii = TerminalCapabilities {
            color: ColorCapability::NoColor,
            unicode: false,
        };
        assert_eq!(unicode.border_chars(), ("─", "│"));
        assert_eq!(ascii.border_chars(), ("-", "|"));
        let detected = TerminalCapabilities::detect();
        assert!(matches!(
            detected.color,
            ColorCapability::NoColor
                | ColorCapability::Ansi16
                | ColorCapability::Ansi256
                | ColorCapability::TrueColor
        ));
    }

    #[test]
    fn fr_ui02_themes_only_change_chrome_and_unknown_values_fall_back() {
        assert_eq!(Theme::parse("modern"), Theme::Modern);
        assert_eq!(Theme::parse("unknown"), Theme::Retro);
        assert!(Theme::Retro.shows_function_key_bar());
        assert!(!Theme::Modern.shows_function_key_bar());
        let ascii = TerminalCapabilities {
            color: ColorCapability::NoColor,
            unicode: false,
        };
        assert_eq!(Theme::Retro.border_chars(ascii), ("-", "|"));
        assert_eq!(Theme::Modern.border_chars(ascii), (" ", " "));
    }

    #[test]
    fn fr_ui02_theme_packages_load_and_reject_unknown_or_empty_names() {
        let path = std::env::temp_dir().join(format!("mtide-theme-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r##"{"name":"Solarized","function_key_bar":false,"foreground":null,"background":"#002b36","ansi_colors":{"red":"#dc322f","bright_blue":"#268bd2"}}"##,
        )
        .expect("write theme");
        let package = load_theme_package(&path).expect("load theme");
        assert_eq!(package.name, "Solarized");
        assert_eq!(package.border_horizontal, "─");
        assert_eq!(package.ansi_colors.get("red"), Some(&"#dc322f".to_owned()));
        std::fs::write(&path, r#"{"name":"","foreground":null,"background":null}"#)
            .expect("write invalid theme");
        assert!(matches!(
            load_theme_package(&path),
            Err(ThemePackageError::EmptyName)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fr_ui02_theme_package_discovery_is_sorted_and_skips_broken_files() {
        let dir = std::env::temp_dir().join(format!("mtide-themes-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("z.json"),
            r#"{"name":"Zulu","foreground":null,"background":null}"#,
        )
        .expect("write z theme");
        std::fs::write(
            dir.join("a.json"),
            r#"{"name":"Alpha","foreground":null,"background":null}"#,
        )
        .expect("write a theme");
        std::fs::write(dir.join("broken.json"), "not json").expect("write broken theme");
        let packages = discover_theme_packages(&dir);
        assert_eq!(
            packages
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["Alpha", "Zulu"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fr_ui11_render_options_inherit_terminal_colors_and_control_background() {
        assert_eq!(color(ColorSlot::Default, true), Color::Reset);
        assert_eq!(
            color(ColorSlot::Default, false),
            if std::env::var_os("NO_COLOR").is_some() {
                Color::Reset
            } else {
                Color::White
            }
        );
        assert!(
            RenderOptions {
                inherit_terminal_colors: true,
                transparent_background: true,
            }
            .transparent_background
        );
    }

    #[test]
    fn fr_tr01_renderer_preserves_indexed_and_truecolor_cells() {
        let mut buffer = CellBuffer::new(2, 1);
        buffer.set(
            0,
            0,
            Cell {
                symbol: 'A',
                style: ColorSlot::Indexed(201),
                background: Some(ColorSlot::Rgb(1, 2, 3)),
            },
        );
        let cell = buffer.get(0, 0).expect("colored cell");
        assert_eq!(cell.style, ColorSlot::Indexed(201));
        assert_eq!(cell.background, Some(ColorSlot::Rgb(1, 2, 3)));
    }

    #[test]
    fn fr_ed04_clipboard_history_is_bounded_and_osc52_is_unicode_safe() {
        let mut clipboard = ClipboardHistory::new(2);
        clipboard.push("第一🙂");
        clipboard.push("second");
        clipboard.push("第一🙂");
        clipboard.push("third");
        assert_eq!(clipboard.entries(), &["third", "第一🙂"]);
        assert_eq!(
            clipboard.osc52(1),
            Some("\x1b]52;c;56ys5LiA8J+Zgg==\x07".into())
        );
        assert_eq!(clipboard.osc52(9), None);
    }

    #[test]
    fn fr_p0_snapshot_dimensions() {
        for (width, height) in [(80, 24), (120, 40), (200, 60)] {
            let frame = CellBuffer::new(width, height);
            assert_eq!(frame.width, width);
            assert_eq!(frame.height, height);
            let geometry = layout(width, height, true);
            assert!(geometry.editor.0 >= 40 || geometry.sidebar.is_none());
            assert!(geometry.status.1 < height);
        }
    }

    #[test]
    fn fr_ui04_vscode_keymap_supports_override_and_conflict_detection() {
        let mut keymap = Keymap::vscode();
        let save = KeyBinding {
            modifiers: KeyModifiers::CONTROL,
            code: KeyCode::Char('s'),
        };
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Save
        );
        keymap.bind(save, Action::Quit);
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        assert_eq!(keymap.conflicts(), vec![save]);
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            Action::Home
        );
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            Action::End
        );
        assert_eq!(
            action_for_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::SUPER)),
            Action::Save
        );
    }

    #[test]
    fn fr_ui04_vimrc_import_accepts_common_safe_mappings_and_reports_unknown_lines() {
        let path = std::env::temp_dir().join(format!("mtide-vimrc-{}.vim", std::process::id()));
        std::fs::write(
            &path,
            "nnoremap <C-s> :w<CR>\nnnoremap <C-p> :Files<CR>\nmap <F5> :unknown-ex\nset number\n",
        )
        .expect("write vimrc");
        let mut keymap = Keymap::vscode();
        let (imported, ignored) = keymap.import_vimrc(&path).expect("import vimrc");
        assert_eq!(imported, 2);
        assert_eq!(ignored, vec![3]);
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Save
        );
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Action::QuickOpen
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fr_ui04_keyboard_presets_share_actions_and_report_overrides() {
        assert_eq!(Keymap::preset("vim"), Keymap::vim());
        assert_eq!(Keymap::preset("emacs"), Keymap::emacs());
        assert_eq!(Keymap::preset("borland"), Keymap::borland());
        assert_eq!(Keymap::preset("unknown"), Keymap::vscode());
        assert_eq!(
            Keymap::vim().action(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        assert_eq!(
            Keymap::emacs().action(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Action::MoveLeft
        );
        let borland = Keymap::borland();
        assert_eq!(
            borland.action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        assert!(!borland.conflicts().is_empty());
    }

    #[test]
    fn fr_ui19_vscode_preset_covers_navigation_and_workflow_commands() {
        let keymap = Keymap::vscode();
        assert_eq!(
            keymap.action(KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Action::CommandPalette
        );
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Action::GoToLine
        );
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Action::Run
        );
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE)),
            Action::Debug
        );
        assert_eq!(
            keymap.action(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            Action::NavigateBack
        );
    }

    #[test]
    fn fr_ui03_command_registry_filters_fuzzily_and_prioritizes_recent_use() {
        let mut registry = CommandRegistry::default();
        for index in 0..10_000 {
            registry.register(
                format!("editor.command{index}"),
                format!("Command {index}"),
                None,
            );
        }
        registry.mark_used("editor.command42");
        registry.mark_used("editor.command420");
        let results = registry.search("c42", 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "editor.command420");
        assert!(registry.search("zzzz", 10).is_empty());
    }

    #[test]
    fn fr_ed21_wraps_without_inserting_newlines() {
        assert_eq!(wrap_line("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap_line("你好世界", 3), vec!["你", "好", "世", "界"]);
        assert_eq!(wrap_line("e\u{301}👩‍💻Z", 2), vec!["e\u{301}", "👩‍💻", "Z"]);
    }

    #[test]
    fn fr_ui06_tabs_and_tree_are_keyboard_safe_and_bounded() {
        let mut tabs = TabState::default();
        tabs.push(Tab {
            title: "one".into(),
            pinned: false,
            dirty: true,
        });
        tabs.push(Tab {
            title: "two".into(),
            pinned: true,
            dirty: false,
        });
        tabs.previous();
        assert_eq!(tabs.selected().map(|tab| tab.title.as_str()), Some("one"));
        assert!(tabs.close_selected().is_some());

        let mut tree = TreeState::new(vec![
            TreeNode {
                label: "src".into(),
                depth: 0,
                expanded: false,
                is_dir: true,
            },
            TreeNode {
                label: "main.rs".into(),
                depth: 1,
                expanded: false,
                is_dir: false,
            },
        ]);
        tree.toggle_selected();
        tree.move_by(100, 1);
        assert_eq!(
            tree.selected().map(|node| node.label.as_str()),
            Some("main.rs")
        );
        assert_eq!(tree.viewport(), 1);
    }
}
