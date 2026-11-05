use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub panel: Color,
    pub border: Color,
    pub muted: Color,
    pub accent: Color,
    pub positive: Color,
    pub foreground: Color,
    pub selection: Color,
    pub warning: Color,
    pub error: Color,
    pub terminal_colors: [Color; 16],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::Rgb(16, 18, 26),
            panel: Color::Rgb(25, 28, 40),
            border: Color::Rgb(69, 77, 105),
            muted: Color::Rgb(137, 145, 173),
            accent: Color::Rgb(113, 192, 255),
            positive: Color::Rgb(141, 218, 164),
            foreground: Color::White,
            selection: Color::Rgb(44, 57, 86),
            warning: Color::Yellow,
            error: Color::Red,
            terminal_colors: [
                Color::Black,
                Color::Red,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Cyan,
                Color::Gray,
                Color::DarkGray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
                Color::White,
            ],
        }
    }
}

impl Theme {
    pub fn from_path(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&contents).map_err(|error| {
            AppError::Workspace(format!("parse theme {}: {error}", path.display()))
        })?;
        Ok(Self::from_json(&value))
    }

    pub fn from_json(value: &serde_json::Value) -> Self {
        let defaults = Self::default();
        let colors = value.get("colors").unwrap_or(value);
        let terminal = colors
            .get("terminal")
            .or_else(|| value.get("terminal"))
            .unwrap_or(&serde_json::Value::Null);
        let mut terminal_colors = defaults.terminal_colors;
        let terminal_names = [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "white",
            "brightBlack",
            "brightRed",
            "brightGreen",
            "brightYellow",
            "brightBlue",
            "brightMagenta",
            "brightCyan",
            "brightWhite",
        ];
        for (index, name) in terminal_names.iter().enumerate() {
            if let Some(parsed) = color(terminal, &[*name]) {
                terminal_colors[index] = parsed;
            }
        }
        Self {
            background: color(
                colors,
                &["background", "ui.background", "editor.background"],
            )
            .unwrap_or(defaults.background),
            panel: color(colors, &["panel", "ui.panel", "sideBar.background"])
                .unwrap_or(defaults.panel),
            border: color(colors, &["border", "ui.border", "panel.border"])
                .unwrap_or(defaults.border),
            muted: color(colors, &["muted", "foregroundMuted", "ui.muted"])
                .unwrap_or(defaults.muted),
            accent: color(colors, &["accent", "focus", "ui.accent"]).unwrap_or(defaults.accent),
            positive: color(colors, &["positive", "success", "git.added"])
                .unwrap_or(defaults.positive),
            foreground: color(colors, &["foreground", "editor.foreground"])
                .unwrap_or(defaults.foreground),
            selection: color(colors, &["selection", "editor.selectionBackground"])
                .unwrap_or(defaults.selection),
            warning: color(colors, &["warning", "editor.warning"]).unwrap_or(defaults.warning),
            error: color(colors, &["error", "editor.error"]).unwrap_or(defaults.error),
            terminal_colors,
        }
    }
}

fn color(value: &serde_json::Value, keys: &[&str]) -> Option<Color> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .and_then(parse_color)
}

fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    let hex = value.strip_prefix('#')?;
    let (red, green, blue) = match hex.len() {
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        3 => (
            u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
        ),
        _ => return None,
    };
    Some(Color::Rgb(red, green, blue))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub sidebar_visible: bool,
    pub bottom_visible: bool,
    pub agent_visible: bool,
    #[serde(
        default,
        rename = "defaultFolders",
        alias = "default_folders",
        alias = "startupFolders"
    )]
    pub default_folders: Vec<String>,
    #[serde(rename = "saveAllOnQuit", alias = "save_all_on_quit")]
    pub save_all_on_quit: bool,
    #[serde(rename = "reuseWindow", alias = "reuse_window")]
    pub reuse_window: bool,
    pub sidebar_width: u16,
    #[serde(rename = "bottomHeight", alias = "bottom_height")]
    pub bottom_height: u16,
    pub agent_width: u16,
    pub theme: Option<String>,
    #[serde(default)]
    pub keyboard: KeyboardSettings,
    #[serde(rename = "quickOpen", alias = "quick_open")]
    pub quick_open: QuickOpenSettings,
    #[serde(rename = "explorer")]
    pub explorer: ExplorerSettings,
    pub editor: EditorSettings,
    pub search: SearchSettings,
    #[serde(default)]
    pub formatters: BTreeMap<String, String>,
    #[serde(default)]
    pub ai: AiSettings,
    pub lsp: LspSettings,
    pub agent: AgentSettings,
    #[serde(default, rename = "agents")]
    pub legacy_agents: BTreeMap<String, AgentProviderConfig>,
    pub terminal: TerminalSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KeyboardSettings {
    #[serde(rename = "modifierKey", alias = "modifier_key")]
    pub modifier_key: String,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            modifier_key: "control".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct QuickOpenSettings {
    #[serde(rename = "showHidden", alias = "show_hidden")]
    pub show_hidden: bool,
    #[serde(rename = "showGitIgnored", alias = "show_git_ignored")]
    pub show_git_ignored: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExplorerSettings {
    #[serde(rename = "showHidden", alias = "show_hidden")]
    pub show_hidden: bool,
    #[serde(rename = "showGitIgnored", alias = "show_git_ignored")]
    pub show_git_ignored: bool,
    #[serde(rename = "autoReveal", alias = "auto_reveal")]
    pub auto_reveal: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EditorSettings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(rename = "autoClosingBrackets", alias = "auto_closing_brackets")]
    pub auto_closing_brackets: bool,
    #[serde(rename = "selectionStyle", alias = "selection_style")]
    pub selection_style: String,
    #[serde(rename = "wordWrap", alias = "word_wrap")]
    pub word_wrap: bool,
    #[serde(rename = "wordWrapMaxColumn", alias = "word_wrap_column")]
    pub word_wrap_column: usize,
    #[serde(rename = "diffContext", alias = "diff_context")]
    pub diff_context: String,
    #[serde(rename = "trimTrailingWhitespace", alias = "trim_trailing_whitespace")]
    pub trim_trailing_whitespace: bool,
    #[serde(rename = "insertFinalNewline", alias = "insert_final_newline")]
    pub insert_final_newline: bool,
    #[serde(rename = "formatOnSave", alias = "format_on_save")]
    pub format_on_save: bool,
    #[serde(rename = "autoSave", alias = "auto_save")]
    pub auto_save: bool,
    #[serde(rename = "autoSaveDelayMs", alias = "auto_save_delay_ms")]
    pub auto_save_delay_ms: u64,
    #[serde(rename = "indentSize", alias = "indent_size")]
    pub indent_size: usize,
    #[serde(rename = "tabSize", alias = "tab_size")]
    pub tab_size: usize,
    #[serde(rename = "useTabs", alias = "use_tabs")]
    pub use_tabs: bool,
    #[serde(rename = "autoDetectIndent", alias = "auto_detect_indent")]
    pub auto_detect_indent: bool,
    #[serde(rename = "markdownImageDir", alias = "markdown_image_dir")]
    pub markdown_image_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchSettings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            include: Vec::new(),
            exclude: vec![
                "target/**".to_owned(),
                "node_modules/**".to_owned(),
                "vendor/**".to_owned(),
            ],
        }
    }
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            language: None,
            auto_closing_brackets: true,
            selection_style: "background".to_owned(),
            word_wrap: false,
            word_wrap_column: 120,
            diff_context: "changes".to_owned(),
            trim_trailing_whitespace: false,
            insert_final_newline: false,
            format_on_save: false,
            auto_save: true,
            auto_save_delay_ms: 5_000,
            indent_size: 4,
            tab_size: 4,
            use_tabs: false,
            auto_detect_indent: true,
            markdown_image_dir: ".mtide/assets".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TerminalSettings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub scrollback: usize,
    #[serde(rename = "ctrlV", alias = "ctrl_v")]
    pub ctrl_v: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentSettings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub enabled: bool,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub install: Option<String>,
    pub active: Option<String>,
    pub providers: BTreeMap<String, AgentProviderConfig>,
    pub tools: Vec<String>,
    #[serde(alias = "persistent", alias = "persist_session")]
    pub persist: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentProviderConfig {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub install: Option<String>,
    pub tools: Vec<String>,
    #[serde(alias = "persistent", alias = "persist_session")]
    pub persist: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    String(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawAgentProviderConfig {
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
    name: String,
    command: Option<StringOrList>,
    cli: Option<String>,
    args: Option<StringOrList>,
    cwd: Option<String>,
    install: Option<String>,
    tools: Vec<String>,
    #[serde(alias = "persistent", alias = "persist_session")]
    persist: bool,
}

impl<'de> serde::Deserialize<'de> for AgentProviderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawAgentProviderConfig::deserialize(deserializer)?;
        let command = if let Some(command) = raw.command {
            match command {
                StringOrList::String(command) => vec![command],
                StringOrList::List(command) => command,
            }
        } else if let Some(cli) = raw.cli {
            let mut command = vec![cli];
            if let Some(args) = raw.args {
                command.extend(match args {
                    StringOrList::String(args) => vec![args],
                    StringOrList::List(args) => args,
                });
            }
            command
        } else {
            Vec::new()
        };
        Ok(Self {
            extra: raw.extra,
            name: raw.name,
            command,
            cwd: raw.cwd,
            install: raw.install,
            tools: raw.tools,
            persist: raw.persist,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LspSettings {
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
    pub enabled: bool,
    pub servers: BTreeMap<String, LspServerConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: Vec<String>,
    pub languages: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            sidebar_visible: true,
            bottom_visible: true,
            agent_visible: true,
            default_folders: Vec::new(),
            save_all_on_quit: false,
            reuse_window: true,
            sidebar_width: 25,
            bottom_height: 5,
            agent_width: 29,
            theme: None,
            keyboard: KeyboardSettings::default(),
            quick_open: QuickOpenSettings::default(),
            explorer: ExplorerSettings::default(),
            editor: EditorSettings::default(),
            search: SearchSettings::default(),
            formatters: BTreeMap::new(),
            ai: AiSettings::default(),
            lsp: LspSettings::default(),
            agent: AgentSettings::default(),
            legacy_agents: BTreeMap::new(),
            terminal: TerminalSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AiSettings {
    pub enabled: bool,
    #[serde(rename = "autoSuggest", alias = "auto_suggest")]
    pub auto_suggest: bool,
    pub endpoint: String,
    pub model: String,
    #[serde(rename = "apiKeyEnv", alias = "api_key_env")]
    pub api_key_env: String,
    #[serde(rename = "maxContextLines", alias = "max_context_lines")]
    pub max_context_lines: usize,
    #[serde(rename = "timeoutMs", alias = "timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_suggest: true,
            endpoint: "https://api.openai.com/v1/chat/completions".to_owned(),
            model: "gpt-4.1-mini".to_owned(),
            api_key_env: "OPENAI_API_KEY".to_owned(),
            max_context_lines: 80,
            timeout_ms: 15_000,
        }
    }
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            enabled: true,
            servers: BTreeMap::new(),
        }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            enabled: true,
            name: "Agent".to_owned(),
            command: Vec::new(),
            cwd: None,
            install: None,
            active: None,
            providers: BTreeMap::new(),
            tools: Vec::new(),
            persist: false,
        }
    }
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            extra: BTreeMap::new(),
            shell: None,
            args: Vec::new(),
            scrollback: 10_000,
            ctrl_v: false,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::load_from_path(path)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        let mut settings: Self = serde_json::from_str(&contents).unwrap_or_default();
        settings.normalize_agents();
        settings.normalize_layout();
        settings
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> AppResult<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|error| AppError::Workspace(format!("serialize settings: {error}")))?;
        fs::write(path, format!("{contents}\n"))?;
        Ok(())
    }

    pub fn save(&self) -> AppResult<()> {
        let path = config_path()
            .ok_or_else(|| AppError::Workspace("settings path is unavailable".to_owned()))?;
        self.save_to_path(path)
    }

    fn normalize_layout(&mut self) {
        self.sidebar_width = self.sidebar_width.clamp(20, 80);
        self.bottom_height = self.bottom_height.clamp(3, 24);
        self.agent_width = self.agent_width.clamp(24, 80);
        self.editor.auto_save_delay_ms = self.editor.auto_save_delay_ms.clamp(500, 60_000);
        if !matches!(
            self.keyboard.modifier_key.to_ascii_lowercase().as_str(),
            "control" | "control+super" | "super"
        ) {
            self.keyboard.modifier_key = "control".to_owned();
        }
    }

    pub fn normalize_agents(&mut self) {
        if self.agent.providers.is_empty() {
            self.agent.providers = self.legacy_agents.clone();
        }
        if self.agent.active.is_none() {
            self.agent.active = self.agent.providers.keys().next().cloned();
        }
    }

    pub fn apply_editorconfig(&mut self, root: &Path, language: &str) -> bool {
        let paths = find_editorconfig_files(root);
        if paths.is_empty() {
            return false;
        }
        for path in paths {
            let Ok(contents) = fs::read_to_string(path) else {
                continue;
            };
            let mut active = false;
            for raw_line in contents.lines() {
                let line = raw_line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    continue;
                }
                if line.starts_with('[') && line.ends_with(']') {
                    let pattern = &line[1..line.len() - 1];
                    active = editorconfig_pattern_matches(pattern, language);
                    continue;
                }
                if !active {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let key = key.trim();
                let value = value.trim();
                match key {
                    "indent_style" => self.editor.use_tabs = value.eq_ignore_ascii_case("tab"),
                    "indent_size" => {
                        if let Ok(size) = value.parse::<usize>() {
                            self.editor.indent_size = size.max(1);
                        }
                    }
                    "tab_width" => {
                        if let Ok(size) = value.parse::<usize>() {
                            self.editor.tab_size = size.max(1);
                        }
                    }
                    "insert_final_newline" => {
                        if let Some(value) = parse_bool(value) {
                            self.editor.insert_final_newline = value;
                        }
                    }
                    "trim_trailing_whitespace" => {
                        if let Some(value) = parse_bool(value) {
                            self.editor.trim_trailing_whitespace = value;
                        }
                    }
                    _ => {}
                }
            }
        }
        true
    }
}

fn find_editorconfig_files(root: &Path) -> Vec<PathBuf> {
    let mut current = if root.is_file() {
        root.parent().unwrap_or(root)
    } else {
        root
    };
    let mut files = Vec::new();
    loop {
        let candidate = current.join(".editorconfig");
        if candidate.is_file() {
            let is_root = fs::read_to_string(&candidate)
                .ok()
                .map(|contents| {
                    contents.lines().any(|line| {
                        let line = line.trim();
                        line.split_once('=').is_some_and(|(key, value)| {
                            key.trim().eq_ignore_ascii_case("root")
                                && value.trim().eq_ignore_ascii_case("true")
                        })
                    })
                })
                .unwrap_or(false);
            files.push(candidate);
            if is_root {
                break;
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    files.reverse();
    files
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn editorconfig_pattern_matches(patterns: &str, language: &str) -> bool {
    let mut alternatives = Vec::new();
    let mut start = 0;
    let mut braces: usize = 0;
    for (index, character) in patterns.char_indices() {
        match character {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            ',' if braces == 0 => {
                alternatives.push(&patterns[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    alternatives.push(&patterns[start..]);
    alternatives.into_iter().any(|pattern| {
        let pattern = pattern.trim();
        if pattern == "*" {
            return true;
        }
        if let Some(inner) = pattern.strip_prefix("*.").and_then(|value| {
            value
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
        }) {
            return inner.split(',').any(|value| value.trim() == language);
        }
        pattern == format!("*.{language}")
    })
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("MTIDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
        .map(PathBuf::from)
        .map(|dir| dir.join("settings.json"))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.join(".mtide/settings.json"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_usable_for_tui() {
        let settings = Settings::default();
        assert!(settings.sidebar_visible);
        assert!(settings.sidebar_width >= 20);
    }
    #[test]
    fn partial_json_uses_defaults() {
        let settings: Settings =
            serde_json::from_str(r#"{"agent_visible":false,"saveAllOnQuit":true}"#)
                .expect("settings");
        assert!(!settings.agent_visible);
        assert!(settings.save_all_on_quit);
        assert!(settings.bottom_visible);
        assert!(settings.lsp.enabled);
    }

    #[test]
    fn parses_default_startup_folders() {
        let settings: Settings = serde_json::from_str(
            r#"{"defaultFolders":["~/work","projects/demo"],"agent_visible":false}"#,
        )
        .expect("settings");
        assert_eq!(settings.default_folders, vec!["~/work", "projects/demo"]);
        assert!(!settings.agent_visible);
    }

    #[test]
    fn editorconfig_overrides_editor_defaults() {
        let root = std::env::temp_dir().join(format!("mtide-editorconfig-{}", std::process::id()));
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join(".editorconfig"),
            "root = true\n[*]\nindent_style = tab\nindent_size = 2\ntab_width = 8\ninsert_final_newline = true\ntrim_trailing_whitespace = yes\n",
        )
        .expect("editorconfig");
        let mut settings = Settings::default();
        settings.apply_editorconfig(&root, "rs");
        assert!(settings.editor.use_tabs);
        assert_eq!(settings.editor.indent_size, 2);
        assert_eq!(settings.editor.tab_size, 8);
        assert!(settings.editor.insert_final_newline);
        assert!(settings.editor.trim_trailing_whitespace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn editorconfig_matches_comma_and_brace_language_patterns() {
        assert!(editorconfig_pattern_matches("*.rs,*.toml", "rs"));
        assert!(editorconfig_pattern_matches("*.{rs,toml}", "toml"));
        assert!(!editorconfig_pattern_matches("*.{rs,toml}", "lua"));
    }

    #[test]
    fn editorconfig_merges_parent_before_child_until_root_marker() {
        let root =
            std::env::temp_dir().join(format!("mtide-editorconfig-chain-{}", std::process::id()));
        let nested = root.join("src");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::write(
            root.join(".editorconfig"),
            "root = true\n[*]\nindent_style = space\nindent_size = 2\ntrim_trailing_whitespace = true\n",
        )
        .expect("parent editorconfig");
        fs::write(nested.join(".editorconfig"), "[*.rs]\nindent_size = 4\n")
            .expect("child editorconfig");
        let mut settings = Settings::default();
        assert!(settings.apply_editorconfig(&nested, "rs"));
        assert!(!settings.editor.use_tabs);
        assert_eq!(settings.editor.indent_size, 4);
        assert!(settings.editor.trim_trailing_whitespace);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parses_lsp_server_configuration() {
        let settings: Settings = serde_json::from_str(
            r#"{"lsp":{"servers":{"rust":{"command":["rust-analyzer"],"languages":["rust"]}}}}"#,
        )
        .expect("settings");
        assert_eq!(settings.lsp.servers["rust"].command, vec!["rust-analyzer"]);
    }

    #[test]
    fn parses_agent_configuration() {
        let settings: Settings = serde_json::from_str(
            r#"{"agent":{"name":"local","command":["codex","--interactive"],"cwd":"/tmp","install":"npm install -g codex"}}"#,
        )
        .expect("settings");
        assert_eq!(settings.agent.name, "local");
        assert_eq!(settings.agent.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            settings.agent.install.as_deref(),
            Some("npm install -g codex")
        );
    }

    #[test]
    fn parses_multiple_agent_providers() {
        let settings: Settings = serde_json::from_str(
            r#"{"agent":{"active":"local","providers":{"local":{"name":"Local","command":["codex"]},"review":{"name":"Review","command":["claude"]}}}}"#,
        )
        .expect("settings");
        assert_eq!(settings.agent.active.as_deref(), Some("local"));
        assert_eq!(settings.agent.providers.len(), 2);
    }

    #[test]
    fn parses_agent_tool_permissions_without_granting_defaults() {
        let settings: Settings = serde_json::from_str(
            r#"{"agent":{"tools":["read_file"],"providers":{"local":{"command":["codex"],"tools":["shell"]}}}}"#,
        )
        .expect("settings");
        assert_eq!(settings.agent.tools, vec!["read_file"]);
        assert_eq!(settings.agent.providers["local"].tools, vec!["shell"]);
        assert!(Settings::default().agent.tools.is_empty());
    }

    #[test]
    fn parses_agent_persistence_aliases() {
        let settings: Settings = serde_json::from_str(
            r#"{"agent":{"persistent":true,"providers":{"local":{"command":["codex"],"persist_session":true}}}}"#,
        )
        .expect("settings");
        assert!(settings.agent.persist);
        assert!(settings.agent.providers["local"].persist);
    }

    #[test]
    fn parses_ttt_agents_schema_and_normalizes_it() {
        let mut settings: Settings = serde_json::from_str(
            r#"{"agents":{"codex":{"cli":"codex","args":"--interactive","install":"npm install -g @openai/codex"},"claude":{"cli":"claude","args":["--print"]}}}"#,
        )
        .expect("settings");
        settings.normalize_agents();
        assert_eq!(settings.agent.active.as_deref(), Some("claude"));
        assert_eq!(
            settings.agent.providers["codex"].command,
            vec!["codex", "--interactive"]
        );
        assert_eq!(
            settings.agent.providers["codex"].install.as_deref(),
            Some("npm install -g @openai/codex")
        );
    }

    #[test]
    fn parses_terminal_configuration() {
        let settings: Settings = serde_json::from_str(
            r#"{"terminal":{"shell":"bash","args":["--noprofile"],"scrollback":5000,"ctrlV":true}}"#,
        )
        .expect("settings");
        assert_eq!(settings.terminal.shell.as_deref(), Some("bash"));
        assert_eq!(settings.terminal.scrollback, 5000);
        assert!(settings.terminal.ctrl_v);
    }

    #[test]
    fn parses_search_include_and_exclude_globs() {
        let settings: Settings = serde_json::from_str(
            r#"{"search":{"include":["**/*.rs"],"exclude":["generated/**","*.snap"]}}"#,
        )
        .expect("settings");
        assert_eq!(settings.search.include, vec!["**/*.rs"]);
        assert_eq!(settings.search.exclude, vec!["generated/**", "*.snap"]);
    }

    #[test]
    fn parses_diff_context_and_ai_settings() {
        let settings: Settings = serde_json::from_str(
            r#"{"editor":{"diffContext":"full"},"ai":{"enabled":true,"autoSuggest":false,"model":"local"}}"#,
        )
        .expect("diff and AI settings");
        assert_eq!(settings.editor.diff_context, "full");
        assert!(settings.ai.enabled);
        assert!(!settings.ai.auto_suggest);
        assert_eq!(settings.ai.model, "local");
    }

    #[test]
    fn parses_quick_open_visibility_settings() {
        let settings: Settings = serde_json::from_str(
            r#"{"quickOpen":{"showHidden":true,"showGitIgnored":true},"explorer":{"showHidden":true,"showGitIgnored":true,"autoReveal":true}}"#,
        )
                .expect("quick open settings");
        assert!(settings.quick_open.show_hidden);
        assert!(settings.quick_open.show_git_ignored);
        assert!(settings.explorer.show_hidden);
        assert!(settings.explorer.show_git_ignored);
        assert!(settings.explorer.auto_reveal);
        assert!(!Settings::default().quick_open.show_hidden);
        assert!(!Settings::default().quick_open.show_git_ignored);
        assert!(!Settings::default().explorer.show_hidden);
        assert!(!Settings::default().explorer.show_git_ignored);
        assert!(!Settings::default().explorer.auto_reveal);
    }

    #[test]
    fn parses_editor_word_wrap_configuration() {
        let settings: Settings = serde_json::from_str(
            r#"{"editor":{"language":"python","autoClosingBrackets":false,"selectionStyle":"invert","wordWrap":true,"wordWrapMaxColumn":88}}"#,
        )
        .expect("settings");
        assert_eq!(settings.editor.language.as_deref(), Some("python"));
        assert!(!settings.editor.auto_closing_brackets);
        assert_eq!(settings.editor.selection_style, "invert");
        assert!(settings.editor.word_wrap);
        assert_eq!(settings.editor.word_wrap_column, 88);
        assert!(!settings.editor.trim_trailing_whitespace);
        assert!(!settings.editor.insert_final_newline);
        let save_settings: Settings = serde_json::from_str(
            r#"{"editor":{"trimTrailingWhitespace":true,"insertFinalNewline":true}}"#,
        )
        .expect("save settings");
        assert!(save_settings.editor.trim_trailing_whitespace);
        assert!(save_settings.editor.insert_final_newline);
        assert!(!save_settings.editor.format_on_save);
        let format_settings: Settings =
            serde_json::from_str(r#"{"editor":{"formatOnSave":true}}"#).expect("format settings");
        assert!(format_settings.editor.format_on_save);
    }

    #[test]
    fn parses_keyboard_modifier_setting() {
        let settings: Settings =
            serde_json::from_str(r#"{"keyboard":{"modifierKey":"control+super"}}"#)
                .expect("keyboard settings");
        assert_eq!(settings.keyboard.modifier_key, "control+super");
        assert_eq!(Settings::default().keyboard.modifier_key, "control");
    }

    #[test]
    fn normalizes_unknown_keyboard_modifier_setting() {
        let path = std::env::temp_dir().join(format!(
            "mtide-keyboard-modifier-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, r#"{"keyboard":{"modifierKey":"invalid"}}"#).expect("settings");
        let settings = Settings::load_from_path(&path);
        assert_eq!(settings.keyboard.modifier_key, "control");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn preserves_unknown_settings_fields_across_json_round_trip() {
        let settings: Settings = serde_json::from_str(
            r#"{"futureSetting":{"enabled":true},"editor":{"futureEditorOption":"keep"},"search":{"futureSearchOption":1},"terminal":{"futureTerminalOption":true},"agent":{"futureAgentOption":"a","providers":{"local":{"cli":"sh","futureProviderOption":false}}},"lsp":{"futureLspOption":[1,2]}}"#,
        )
        .expect("settings");
        let value = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(value["futureSetting"]["enabled"], true);
        assert_eq!(value["editor"]["futureEditorOption"], "keep");
        assert_eq!(value["search"]["futureSearchOption"], 1);
        assert_eq!(value["terminal"]["futureTerminalOption"], true);
        assert_eq!(value["agent"]["futureAgentOption"], "a");
        assert_eq!(
            value["agent"]["providers"]["local"]["futureProviderOption"],
            false
        );
        assert_eq!(value["lsp"]["futureLspOption"], serde_json::json!([1, 2]));
    }

    #[test]
    fn parses_reuse_window_setting_and_defaults_to_enabled() {
        let disabled: Settings =
            serde_json::from_str(r#"{"reuseWindow":false}"#).expect("reuse setting");
        assert!(!disabled.reuse_window);
        assert!(Settings::default().reuse_window);
        let serialized = serde_json::to_value(&disabled).expect("serialize reuse setting");
        assert_eq!(serialized["reuseWindow"], false);
    }

    #[test]
    fn loads_settings_from_explicit_path() {
        let path = std::env::temp_dir().join(format!("mtide-config-{}", std::process::id()));
        fs::write(&path, r#"{"sidebar_visible":false}"#).expect("write config");
        let settings = Settings::load_from_path(&path);
        assert!(!settings.sidebar_visible);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn settings_round_trip_preserves_editable_values() {
        let path = std::env::temp_dir().join(format!("mtide-settings-{}.json", std::process::id()));
        let settings: Settings = serde_json::from_str(
            r#"{"sidebar_visible":false,"editor":{"wordWrap":true},"terminal":{"ctrlV":true},"lsp":{"enabled":false}}"#,
        )
        .expect("settings");
        settings.save_to_path(&path).expect("save settings");
        let loaded = Settings::load_from_path(&path);
        assert!(!loaded.sidebar_visible);
        assert!(loaded.editor.word_wrap);
        assert!(loaded.terminal.ctrl_v);
        assert!(!loaded.lsp.enabled);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_semantic_theme_colors_with_defaults() {
        let theme = Theme::from_json(&serde_json::json!({
            "colors": {
                "background": "#010203",
                "accent": "#abc",
                "foreground": "not-a-color"
            }
        }));
        assert_eq!(theme.background, Color::Rgb(1, 2, 3));
        assert_eq!(theme.accent, Color::Rgb(170, 187, 204));
        assert_eq!(theme.foreground, Theme::default().foreground);
        assert_eq!(theme.panel, Theme::default().panel);
    }

    #[test]
    fn parses_terminal_theme_palette_without_changing_truecolor_defaults() {
        let theme = Theme::from_json(&serde_json::json!({
            "colors": {
                "terminal": {
                    "red": "#112233",
                    "brightBlue": "#abcdef"
                }
            }
        }));
        assert_eq!(theme.terminal_colors[1], Color::Rgb(17, 34, 51));
        assert_eq!(theme.terminal_colors[12], Color::Rgb(171, 205, 239));
        assert_eq!(
            theme.terminal_colors[0],
            Theme::default().terminal_colors[0]
        );
    }
}
