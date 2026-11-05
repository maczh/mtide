mod app;
mod config;
mod core;
mod error;
mod git;
mod lsp;
mod plugin;
mod search;
mod terminal;
mod ui;

use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use config::Settings;
use error::AppResult;

fn main() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-v"))
    {
        println!("mtide {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let instance_server =
        if args.iter().any(|arg| arg == "--new-window") || !reuse_window_enabled(&args) {
            None
        } else {
            #[cfg(unix)]
            {
                if forward_to_existing_instance(&args)? {
                    return Ok(());
                }
                Some(bind_instance_server()?)
            }
            #[cfg(not(unix))]
            {
                None
            }
        };
    let has_explicit_target = args.iter().enumerate().any(|(index, arg)| {
        if arg == "-" {
            return true;
        }
        !arg.starts_with('-')
            && (index == 0
                || !matches!(
                    args.get(index - 1).map(String::as_str),
                    Some(
                        "--config"
                            | "--plugin"
                            | "--workspace"
                            | "--exec"
                            | "--exec-split-on"
                            | "--size",
                    )
                ))
    });
    let welcome = args.iter().any(|arg| arg == "--welcome");
    let stdin_text = if args.iter().any(|arg| arg == "-")
        || (!welcome && !has_explicit_target && !io::stdin().is_terminal())
    {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        Some(text)
    } else {
        None
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(
        &mut terminal,
        &args,
        stdin_text.as_deref(),
        instance_server.as_ref().map(|server| &server.receiver),
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    args: &[String],
    stdin_text: Option<&str>,
    #[cfg(unix)] instance_requests: Option<&Receiver<String>>,
    #[cfg(not(unix))] _instance_requests: Option<&Receiver<String>>,
) -> AppResult<()> {
    let mut app = match stdin_text {
        Some(text) => App::new_with_stdin(args, Some(text))?,
        None => App::new(args)?,
    };
    if args.iter().any(|arg| arg == "--debug") {
        eprintln!(
            "mtide debug: app initialized focus={:?} lines={}",
            app.focus(),
            app.file_lines().len()
        );
    }
    if let Some(size) = option_values(args, "--size").into_iter().next() {
        let (columns, rows) = parse_size(&size)?;
        app.resize_terminal(columns, rows)?;
    }
    let separator = option_values(args, "--exec-split-on")
        .into_iter()
        .next()
        .filter(|value| !value.is_empty());
    for script in option_values(args, "--exec") {
        let scripts = separator.as_deref().map_or_else(
            || vec![script.clone()],
            |separator| script.split(separator).map(str::to_owned).collect(),
        );
        for script in scripts {
            let size = terminal.size()?;
            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            if args.iter().any(|arg| arg == "--debug") {
                execute_script_with_debug(&mut app, &script, area, true)?;
            } else {
                execute_script(&mut app, &script, area)?;
            }
            if app.should_quit() {
                return Ok(());
            }
        }
    }
    loop {
        app.poll_background();
        #[cfg(unix)]
        if let Some(requests) = instance_requests {
            for target in requests.try_iter() {
                if let Err(error) = app.open_external_target(&target) {
                    app.set_status_message(format!("Open request failed: {error}"));
                }
            }
        }
        if !app.should_quit() {
            let size = terminal.size()?;
            for mouse in app.drain_plugin_mouse_events() {
                ui::handle_mouse(
                    &mut app,
                    mouse,
                    ratatui::layout::Rect::new(0, 0, size.width, size.height),
                );
            }
        }
        terminal.draw(|frame| ui::draw(frame, &app))?;
        if event::poll(std::time::Duration::from_millis(250))? {
            let input = event::read()?;
            match input {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let should_quit = handle_key(&mut app, key);
                    if let Some(clipboard) = app.take_osc52_clipboard() {
                        write_osc52_clipboard(&clipboard)?;
                    }
                    if should_quit {
                        break;
                    }
                }
                Event::Paste(text) => {
                    if let Err(error) = app.handle_paste(&text) {
                        app.set_status_message(format!("Paste error: {error}"));
                    }
                }
                Event::Resize(columns, rows) => {
                    if let Err(error) = app.resize_terminal(columns, rows) {
                        app.set_status_message(format!("Resize error: {error}"));
                    }
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    ui::handle_mouse(
                        &mut app,
                        mouse,
                        ratatui::layout::Rect::new(0, 0, size.width, size.height),
                    );
                }
                Event::FocusGained => {
                    if let Err(error) = app.handle_focus_event(true) {
                        app.set_status_message(format!("Focus reporting error: {error}"));
                    }
                }
                Event::FocusLost => {
                    if let Err(error) = app.handle_focus_event(false) {
                        app.set_status_message(format!("Focus reporting error: {error}"));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
struct InstanceServer {
    path: PathBuf,
    receiver: Receiver<String>,
}

#[cfg(unix)]
impl Drop for InstanceServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn instance_socket_path() -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::env::current_dir()
        .unwrap_or_default()
        .hash(&mut hasher);
    std::env::var_os("USER").hash(&mut hasher);
    std::env::temp_dir().join(format!("mtide-{:x}.sock", hasher.finish()))
}

#[cfg(unix)]
fn target_arguments(args: &[String]) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter_map(|(index, arg)| {
            if arg == "-" || arg.starts_with('-') {
                return None;
            }
            if matches!(
                args.get(index.wrapping_sub(1)).map(String::as_str),
                Some(
                    "--config"
                        | "--plugin"
                        | "--workspace"
                        | "--exec"
                        | "--exec-split-on"
                        | "--size"
                )
            ) {
                None
            } else {
                Some(arg.clone())
            }
        })
        .collect()
}

#[cfg(unix)]
fn forward_to_existing_instance(args: &[String]) -> AppResult<bool> {
    let targets = target_arguments(args);
    if targets.is_empty() {
        return Ok(false);
    }
    let path = instance_socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return Ok(false);
    };
    for target in targets {
        let encoded = serde_json::to_string(&target)
            .map_err(|error| error::AppError::Workspace(format!("encode open request: {error}")))?;
        writeln!(stream, "{encoded}")?;
    }
    Ok(true)
}

#[cfg(unix)]
fn bind_instance_server() -> AppResult<InstanceServer> {
    let path = instance_socket_path();
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path)?
        }
    };
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let sender = sender.clone();
            thread::spawn(move || read_instance_requests(stream, sender));
        }
    });
    Ok(InstanceServer { path, receiver })
}

#[cfg(unix)]
fn read_instance_requests(stream: UnixStream, sender: mpsc::Sender<String>) {
    use std::io::{BufRead, BufReader};
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if let Ok(target) = serde_json::from_str::<String>(&line) {
            let _ = sender.send(target);
        }
    }
}

fn reuse_window_enabled(args: &[String]) -> bool {
    if let Some(path) = option_values(args, "--config").into_iter().next() {
        return Settings::load_from_path(path).reuse_window;
    }
    Settings::load().reuse_window
}

fn option_values(args: &[String], option: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].clone())
        .collect()
}

fn parse_size(value: &str) -> AppResult<(u16, u16)> {
    let (columns, rows) = value
        .split_once('x')
        .or_else(|| value.split_once('X'))
        .ok_or_else(|| error::AppError::Workspace(format!("invalid --size: {value}")))?;
    let columns = columns
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| error::AppError::Workspace(format!("invalid --size: {value}")))?;
    let rows = rows
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| error::AppError::Workspace(format!("invalid --size: {value}")))?;
    Ok((columns, rows))
}

fn execute_script(app: &mut App, script: &str, area: ratatui::layout::Rect) -> AppResult<()> {
    execute_script_with_debug(app, script, area, false)
}

fn execute_script_with_debug(
    app: &mut App,
    script: &str,
    area: ratatui::layout::Rect,
    debug: bool,
) -> AppResult<()> {
    let actions: Vec<serde_json::Value> = serde_json::from_str(script)
        .map_err(|error| error::AppError::Workspace(format!("invalid --exec script: {error}")))?;
    for action in actions {
        let kind = action
            .get("action")
            .or_else(|| action.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if debug {
            eprintln!("mtide debug: exec action={kind}");
        }
        match kind {
            "hover" => {
                let column = script_coordinate(&action, "x")?;
                let row = script_coordinate(&action, "y")?;
                ui::handle_mouse(
                    app,
                    MouseEvent {
                        kind: MouseEventKind::Moved,
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                );
            }
            "click" | "rclick" => {
                let column = script_coordinate(&action, "x")?;
                let row = script_coordinate(&action, "y")?;
                let button = if kind == "click" {
                    MouseButton::Left
                } else {
                    MouseButton::Right
                };
                ui::handle_mouse(
                    app,
                    MouseEvent {
                        kind: MouseEventKind::Down(button),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                );
            }
            "drag" => {
                let x1 = script_coordinate(&action, "x1")?;
                let y1 = script_coordinate(&action, "y1")?;
                let x2 = script_coordinate(&action, "x2")?;
                let y2 = script_coordinate(&action, "y2")?;
                for (kind, column, row) in [
                    (MouseEventKind::Down(MouseButton::Left), x1, y1),
                    (MouseEventKind::Drag(MouseButton::Left), x2, y2),
                    (MouseEventKind::Up(MouseButton::Left), x2, y2),
                ] {
                    ui::handle_mouse(
                        app,
                        MouseEvent {
                            kind,
                            column,
                            row,
                            modifiers: KeyModifiers::NONE,
                        },
                        area,
                    );
                }
            }
            "key" => {
                let key = action
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| error::AppError::Workspace("--exec key is required".into()))?;
                app.handle_key(parse_script_key(key)?)?;
            }
            "type" | "paste" => {
                let text = action
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| error::AppError::Workspace("--exec text is required".into()))?;
                app.handle_paste(text)?;
            }
            "copy" => {
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))?;
            }
            "debug" => {
                let snapshot = app.automation_debug().to_string();
                if let Some(path) = action.get("path").and_then(serde_json::Value::as_str) {
                    std::fs::write(path, snapshot).map_err(|error| {
                        error::AppError::Workspace(format!("--exec debug failed: {error}"))
                    })?;
                } else {
                    println!("{snapshot}");
                }
            }
            "screenshot" => {
                let snapshot = app.automation_screenshot();
                if let Some(path) = action.get("path").and_then(serde_json::Value::as_str) {
                    std::fs::write(path, snapshot).map_err(|error| {
                        error::AppError::Workspace(format!("--exec screenshot failed: {error}"))
                    })?;
                } else {
                    println!("{snapshot}");
                }
            }
            "panel" => {
                let panel = action
                    .get("panel")
                    .or_else(|| action.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| error::AppError::Workspace("--exec panel is required".into()))?;
                app.automation_panel(panel)?;
            }
            "open" => {
                let path = action
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        error::AppError::Workspace("--exec open path is required".into())
                    })?;
                let line = action
                    .get("line")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as usize);
                let column = action
                    .get("column")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as usize);
                let readonly = action
                    .get("readonly")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                app.automation_open(path, line, column, readonly)?;
            }
            "exec" => execute_external_script_action(&action)?,
            "wait" => {
                let milliseconds = action
                    .get("ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .min(60_000);
                std::thread::sleep(std::time::Duration::from_millis(milliseconds));
            }
            "wait-for" => {
                let path = action
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        error::AppError::Workspace("--exec wait-for path is required".into())
                    })?;
                let timeout = action
                    .get("ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(60_000)
                    .min(60_000);
                let started = std::time::Instant::now();
                while !std::path::Path::new(path).exists() {
                    if started.elapsed() >= std::time::Duration::from_millis(timeout) {
                        return Err(error::AppError::Workspace(format!(
                            "--exec wait-for timed out: {path}"
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            "quit" => app.quit(),
            other => {
                return Err(error::AppError::Workspace(format!(
                    "unsupported --exec action: {other}"
                )))
            }
        }
    }
    Ok(())
}

fn execute_external_script_action(action: &serde_json::Value) -> AppResult<()> {
    let command = action
        .get("command")
        .or_else(|| action.get("cmd"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            error::AppError::Workspace(
                "--exec exec command must be a non-empty string array".into(),
            )
        })?;
    let parts = command
        .iter()
        .map(|value| {
            value.as_str().ok_or_else(|| {
                error::AppError::Workspace("--exec exec command entries must be strings".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((program, args)) = parts.split_first() else {
        return Err(error::AppError::Workspace(
            "--exec exec command must not be empty".into(),
        ));
    };
    let timeout = action
        .get("ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(60_000)
        .min(60_000);
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| error::AppError::Workspace(format!("--exec exec failed: {error}")))?;
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            error::AppError::Workspace(format!("--exec exec wait failed: {error}"))
        })? {
            let output = child.wait_with_output().map_err(|error| {
                error::AppError::Workspace(format!("--exec exec read failed: {error}"))
            })?;
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            if !status.success() {
                return Err(error::AppError::Workspace(format!(
                    "--exec exec exited with {status}"
                )));
            }
            return Ok(());
        }
        if started.elapsed() >= std::time::Duration::from_millis(timeout) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error::AppError::Workspace("--exec exec timed out".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn script_coordinate(action: &serde_json::Value, name: &str) -> AppResult<u16> {
    action
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.min(u16::MAX as u64) as u16)
        .ok_or_else(|| error::AppError::Workspace(format!("--exec {name} is required")))
}

fn parse_script_key(value: &str) -> AppResult<KeyEvent> {
    let mut modifiers = KeyModifiers::NONE;
    let mut code = None;
    for token in value.split('+') {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "enter" | "return" => code = Some(KeyCode::Enter),
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "tab" => code = Some(KeyCode::Tab),
            "backspace" => code = Some(KeyCode::Backspace),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "home" => code = Some(KeyCode::Home),
            "end" => code = Some(KeyCode::End),
            "pageup" | "pgup" => code = Some(KeyCode::PageUp),
            "pagedown" | "pgdown" | "pgdn" => code = Some(KeyCode::PageDown),
            token if token.chars().count() == 1 => code = token.chars().next().map(KeyCode::Char),
            _ => {
                return Err(error::AppError::Workspace(format!(
                    "invalid --exec key: {value}"
                )))
            }
        }
    }
    code.map(|code| KeyEvent::new(code, modifiers))
        .ok_or_else(|| error::AppError::Workspace(format!("invalid --exec key: {value}")))
}

fn print_help() {
    println!(
        "mtide {}\n\nUSAGE:\n    mtide [OPTIONS] [FILE|DIRECTORY]\n\nOPTIONS:\n    --plugin DIR      Load a TTT-compatible Lua plugin package (repeatable)\n    --workspace FILE  Open a .ttt workspace\n    --config FILE     Load settings from FILE\n    --exec JSON       Execute JSON automation actions before entering the UI\n    --size WxH        Set initial PTY and Agent dimensions\n    --listen          Accept file targets from later mtide invocations\n    --new-window      Start a new editor window\n    --clear-default-folders  Clear saved startup folders\n    --welcome         Start with a welcome buffer\n    -h, --help        Show this help\n    --version, -v     Show version\n\nKEYS:\n    Ctrl-Q         Quit\n    Tab             Cycle focus\n    Ctrl-B/T/A      Toggle sidebar/terminal/agent\n    Ctrl-P          Plugin command palette",
        env!("CARGO_PKG_VERSION")
    );
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit();
        return true;
    }
    if let Err(error) = app.handle_key(key) {
        app.set_status_message(format!("Error: {error}"));
    }
    app.should_quit()
}

const OSC52_MAX_BYTES: usize = 64 * 1024;

fn write_osc52_clipboard(text: &str) -> AppResult<()> {
    let Some(payload) = osc52_payload(text) else {
        return Ok(());
    };
    let mut stdout = io::stdout();
    stdout.write_all(format!("\x1b]52;c;{payload}\x07").as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn osc52_payload(text: &str) -> Option<String> {
    if text.len() > OSC52_MAX_BYTES {
        return None;
    }
    let bytes = text.as_bytes();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[((first & 0b11) << 4 | second >> 4) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((second & 0b1111) << 2 | third >> 6) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b111111) as usize] as char
        } else {
            '='
        });
    }
    Some(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exec_keys_and_modifiers() {
        let key = parse_script_key("ctrl+shift+enter").expect("key");
        assert_eq!(key.code, KeyCode::Enter);
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
        assert!(key.modifiers.contains(KeyModifiers::SHIFT));
        assert_eq!(parse_script_key("home").expect("home").code, KeyCode::Home);
        assert_eq!(
            parse_script_key("pagedown").expect("pagedown").code,
            KeyCode::PageDown
        );
        assert!(parse_script_key("unknown").is_err());
    }

    #[test]
    fn collects_repeated_exec_options() {
        let args = vec!["--exec".into(), "[]".into(), "--exec".into(), "[]".into()];
        assert_eq!(option_values(&args, "--exec"), vec!["[]", "[]"]);
    }

    #[cfg(unix)]
    #[test]
    fn instance_forwarding_extracts_only_file_targets() {
        let args = vec![
            "--config".into(),
            "settings.json".into(),
            "src/main.rs:3:4".into(),
            "--new-window".into(),
            "README.md".into(),
        ];
        assert_eq!(
            target_arguments(&args),
            vec!["src/main.rs:3:4".to_owned(), "README.md".to_owned()]
        );
    }

    #[test]
    fn reuse_window_setting_controls_instance_startup() {
        let path = std::env::temp_dir().join(format!("mtide-reuse-window-{}", std::process::id()));
        std::fs::write(&path, r#"{"reuseWindow":false}"#).expect("config");
        let args = vec!["--config".to_owned(), path.to_string_lossy().into_owned()];
        assert!(!reuse_window_enabled(&args));
        std::fs::write(&path, r#"{"reuseWindow":true}"#).expect("config");
        assert!(reuse_window_enabled(&args));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn splits_exec_scripts_with_configured_separator() {
        let args = vec![
            "--exec-split-on".into(),
            "|".into(),
            "--exec".into(),
            "[]|[]".into(),
        ];
        let separator = option_values(&args, "--exec-split-on").remove(0);
        assert_eq!("[]|[]".split(&separator).count(), 2);
    }

    #[test]
    fn package_metadata_is_available_for_cli() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn parses_cli_size_case_insensitively_and_rejects_zero() {
        assert_eq!(parse_size("120x40").expect("size"), (120, 40));
        assert_eq!(parse_size("80X24").expect("size"), (80, 24));
        assert!(parse_size("0x24").is_err());
        assert!(parse_size("80").is_err());
    }

    #[test]
    fn script_coordinates_are_bounded_and_required() {
        let action = serde_json::json!({"x": 7, "y": 9});
        assert_eq!(script_coordinate(&action, "x").expect("x"), 7);
        assert_eq!(script_coordinate(&action, "y").expect("y"), 9);
        assert!(script_coordinate(&action, "missing").is_err());
    }

    #[test]
    fn wait_for_uses_an_observable_file_condition() {
        let path = std::env::temp_dir().join(format!("mtide-wait-for-{}", std::process::id()));
        std::fs::write(&path, "ready").expect("file");
        let script = format!(
            r#"[{{"action":"wait-for","path":{},"ms":10}}]"#,
            serde_json::to_string(&path).expect("path")
        );
        let mut app = App::new(&["--welcome".to_owned()]).expect("app");
        execute_script(&mut app, &script, ratatui::layout::Rect::new(0, 0, 80, 24))
            .expect("wait-for");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn panel_action_selects_named_focus() {
        let mut app = App::new(&["--welcome".to_owned()]).expect("app");
        let script = r#"[{"action":"panel","panel":"output"}]"#;
        execute_script(&mut app, script, ratatui::layout::Rect::new(0, 0, 80, 24)).expect("panel");
        assert_eq!(app.focus(), app::Focus::Output);
        assert!(execute_script(
            &mut app,
            r#"[{"action":"panel","panel":"unknown"}]"#,
            ratatui::layout::Rect::new(0, 0, 80, 24),
        )
        .is_err());
    }

    #[test]
    fn open_action_loads_file_and_positions_cursor() {
        let path = std::env::temp_dir().join(format!("mtide-exec-open-{}", std::process::id()));
        std::fs::write(&path, "one\ntwo\nthree").expect("file");
        let mut app = App::new(&["--welcome".to_owned()]).expect("app");
        let script = format!(
            r#"[{{"action":"open","path":{},"line":2,"column":2,"readonly":true}}]"#,
            serde_json::to_string(&path).expect("path")
        );
        execute_script(&mut app, &script, ratatui::layout::Rect::new(0, 0, 80, 24)).expect("open");
        assert_eq!(app.file_lines(), &["one", "two", "three"]);
        assert_eq!((app.cursor().line, app.cursor().column), (1, 1));
        assert_eq!(app.focus(), app::Focus::Editor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exec_action_runs_bounded_command_array() {
        let action = serde_json::json!({
            "command": ["/bin/echo", "mtide"],
            "ms": 1000
        });
        execute_external_script_action(&action).expect("exec");
    }

    #[test]
    fn screenshot_action_can_write_a_snapshot_path() {
        let path = std::env::temp_dir().join(format!("mtide-screenshot-{}", std::process::id()));
        let script = format!(
            r#"[{{"action":"screenshot","path":{}}}]"#,
            serde_json::to_string(&path).expect("path")
        );
        let mut app = App::new(&["--welcome".to_owned()]).expect("app");
        execute_script(&mut app, &script, ratatui::layout::Rect::new(0, 0, 80, 24))
            .expect("screenshot");
        assert!(std::fs::read_to_string(&path)
            .expect("snapshot")
            .contains("# mtide"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn debug_action_can_write_json_snapshot_path() {
        let path = std::env::temp_dir().join(format!("mtide-debug-{}", std::process::id()));
        let script = format!(
            r#"[{{"action":"debug","path":{}}}]"#,
            serde_json::to_string(&path).expect("path")
        );
        let mut app = App::new(&["--welcome".to_owned()]).expect("app");
        execute_script(&mut app, &script, ratatui::layout::Rect::new(0, 0, 80, 24)).expect("debug");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("debug snapshot"))
                .expect("json");
        assert_eq!(value["line_count"], app.file_lines().len());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn size_value_is_not_treated_as_an_editor_target() {
        let args = vec![
            "--size".to_owned(),
            "120x40".to_owned(),
            "--welcome".to_owned(),
        ];
        let app = App::new(&args).expect("app");
        assert_eq!(app.file_lines()[0], "# mtide");
    }

    #[test]
    fn stdin_target_is_read_as_an_unsaved_buffer() {
        let args = vec!["-".to_owned()];
        let app = App::new_with_stdin(&args, Some("hello\nworld\n")).expect("stdin app");
        assert_eq!(app.file_lines(), &["hello", "world", ""]);
    }

    #[test]
    fn piped_stdin_without_a_target_is_read_as_an_unsaved_buffer() {
        let app = App::new_with_stdin(&[], Some("piped")).expect("stdin app");
        assert_eq!(app.file_lines(), &["piped"]);
    }

    #[test]
    fn welcome_flag_suppresses_piped_stdin() {
        let args = vec!["--welcome".to_owned()];
        let app = App::new_with_stdin(&args, Some("piped")).expect("welcome app");
        assert_eq!(app.file_lines()[0], "# mtide");
    }

    #[test]
    fn osc52_payload_uses_bounded_base64_encoding() {
        assert_eq!(super::osc52_payload("hello"), Some("aGVsbG8=".to_owned()));
        assert_eq!(super::osc52_payload("你好"), Some("5L2g5aW9".to_owned()));
        assert!(super::osc52_payload(&"x".repeat(super::OSC52_MAX_BYTES + 1)).is_none());
    }
}
